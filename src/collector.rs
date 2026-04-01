//! Live market collector: Gamma discovery + CLOB WebSocket with token rotation.

use anyhow::Result;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use crate::gamma_client::GammaClient;
use crate::poly_history::{PolyHistory, FLUSH_INTERVAL_SECS};
use crate::websocket_client::{
    LastTradePrice, MarketWebSocket, OrderbookSnapshot, PriceChange, WSS_MARKET_URL, WsCommand,
};

/// Configuration for [`PolymarketCollector`].
#[derive(Debug, Clone)]
pub struct CollectorConfig {
    pub gamma_host: String,
    pub gamma_timeout_secs: u64,
    pub coins: Vec<String>,
    pub refresh_interval: Duration,
    pub ws_url: String,
}

impl Default for CollectorConfig {
    fn default() -> Self {
        Self {
            gamma_host: "https://gamma-api.polymarket.com".to_string(),
            gamma_timeout_secs: 10,
            coins: vec![
                "BTC".to_string(),
                "ETH".to_string(),
                "SOL".to_string(),
                "XRP".to_string(),
            ],
            refresh_interval: Duration::from_secs(2),
            ws_url: WSS_MARKET_URL.to_string(),
        }
    }
}

/// Which feed produced the quote update.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QuoteSource {
    Book,
    PriceChange,
    LastTrade,
}

/// Normalized real-time quote for one CLOB token (outcome leg).
#[derive(Debug, Clone, Serialize)]
pub struct TokenQuote {
    pub coin: String,
    pub outcome: String,
    pub market_slug: String,
    pub token_id: String,
    pub best_bid: f64,
    pub best_ask: f64,
    pub mid: f64,
    pub last_trade_price: Option<f64>,
    pub last_trade_side: Option<String>,
    pub last_trade_time_ms: Option<i64>,
    pub event_time_ms: Option<i64>,
    pub source: QuoteSource,
}

/// Gamma metadata for one active window per coin.
#[derive(Debug, Clone, Serialize)]
pub struct MarketCard {
    pub coin: String,
    pub slug: String,
    pub question: String,
    pub tokens: HashMap<String, String>,
}

/// In-memory snapshot (quotes + active markets).
#[derive(Debug, Clone, Default, Serialize)]
pub struct CollectorState {
    pub quotes: HashMap<String, TokenQuote>,
    pub markets: HashMap<String, MarketCard>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CollectorEvent {
    Book { quote: TokenQuote },
    /// Full quote after book merge (includes `token_id`, `market_slug`, `mid`).
    PriceChange { quote: TokenQuote },
    Trade { quote: TokenQuote },
    MarketRotated {
        coin: String,
        old_slug: Option<String>,
        new_slug: String,
    },
    DiscoverySync {
        coins_live: usize,
        tokens: usize,
    },
    Error { message: String },
}

#[derive(Clone)]
struct TokenMeta {
    coin: String,
    outcome: String,
    market_slug: String,
}

struct Discovery {
    markets: HashMap<String, MarketCard>,
    token_meta: HashMap<String, TokenMeta>,
    all_token_ids: Vec<String>,
}

async fn discover(gamma: &GammaClient, coins: &[String]) -> Result<Discovery> {
    let mut markets = HashMap::new();
    let mut token_meta = HashMap::new();
    let mut all_token_ids = Vec::new();

    for coin in coins {
        let c = coin.trim().to_ascii_uppercase();
        if c.is_empty() {
            continue;
        }
        let Some(market) = gamma.get_current_5m_market(&c).await? else {
            continue;
        };
        let slug = market.slug.clone();
        let question = market.question.clone().unwrap_or_default();
        let tokens = gamma.parse_token_ids(&market).await;

        let mut card_tokens = HashMap::new();
        for (outcome, tid) in tokens {
            token_meta.insert(
                tid.clone(),
                TokenMeta {
                    coin: c.clone(),
                    outcome: outcome.clone(),
                    market_slug: slug.clone(),
                },
            );
            card_tokens.insert(outcome, tid.clone());
            all_token_ids.push(tid);
        }

        markets.insert(
            c.clone(),
            MarketCard {
                coin: c,
                slug,
                question,
                tokens: card_tokens,
            },
        );
    }

    Ok(Discovery {
        markets,
        token_meta,
        all_token_ids,
    })
}

fn needs_rotate(old: &MarketCard, new: &MarketCard) -> bool {
    if old.slug != new.slug {
        return true;
    }
    let mut a: Vec<_> = old.tokens.values().cloned().collect();
    let mut b: Vec<_> = new.tokens.values().cloned().collect();
    a.sort();
    b.sort();
    a != b
}

fn quote_mid(bb: f64, ba: f64) -> f64 {
    if bb > 0.0 && ba < 1.0 {
        (bb + ba) / 2.0
    } else if bb > 0.0 {
        bb
    } else if ba < 1.0 {
        ba
    } else {
        0.5
    }
}

fn token_quote_from_book(meta: &TokenMeta, ob: &OrderbookSnapshot) -> TokenQuote {
    let bb = ob.best_bid();
    let ba = ob.best_ask();
    TokenQuote {
        coin: meta.coin.clone(),
        outcome: meta.outcome.clone(),
        market_slug: meta.market_slug.clone(),
        token_id: ob.asset_id.clone(),
        best_bid: bb,
        best_ask: ba,
        mid: ob.mid_price(),
        last_trade_price: None,
        last_trade_side: None,
        last_trade_time_ms: None,
        event_time_ms: if ob.timestamp > 0 {
            Some(ob.timestamp)
        } else {
            None
        },
        source: QuoteSource::Book,
    }
}

fn merge_book_quotes(
    quotes: &mut HashMap<String, TokenQuote>,
    meta: &TokenMeta,
    ob: &OrderbookSnapshot,
) -> TokenQuote {
    let mut q = token_quote_from_book(meta, ob);
    if let Some(p) = quotes.get(&ob.asset_id) {
        q.last_trade_price = p.last_trade_price;
        q.last_trade_side = p.last_trade_side.clone();
        q.last_trade_time_ms = p.last_trade_time_ms;
    }
    quotes.insert(ob.asset_id.clone(), q.clone());
    q
}

fn token_quote_from_price_change(meta: &TokenMeta, pc: &PriceChange) -> TokenQuote {
    let bb = pc.best_bid;
    let ba = pc.best_ask;
    TokenQuote {
        coin: meta.coin.clone(),
        outcome: meta.outcome.clone(),
        market_slug: meta.market_slug.clone(),
        token_id: pc.asset_id.clone(),
        best_bid: bb,
        best_ask: ba,
        mid: quote_mid(bb, ba),
        last_trade_price: None,
        last_trade_side: None,
        last_trade_time_ms: None,
        event_time_ms: None,
        source: QuoteSource::PriceChange,
    }
}

fn merge_price_change(
    quotes: &mut HashMap<String, TokenQuote>,
    meta: &TokenMeta,
    pc: &PriceChange,
) -> TokenQuote {
    let mut q = token_quote_from_price_change(meta, pc);
    if let Some(p) = quotes.get(&pc.asset_id) {
        q.last_trade_price = p.last_trade_price;
        q.last_trade_side = p.last_trade_side.clone();
        q.last_trade_time_ms = p.last_trade_time_ms;
    }
    quotes.insert(pc.asset_id.clone(), q.clone());
    q
}

fn merge_trade(
    quotes: &mut HashMap<String, TokenQuote>,
    meta: &TokenMeta,
    tr: &LastTradePrice,
) -> TokenQuote {
    let base = quotes.get(&tr.asset_id).cloned().unwrap_or_else(|| TokenQuote {
        coin: meta.coin.clone(),
        outcome: meta.outcome.clone(),
        market_slug: meta.market_slug.clone(),
        token_id: tr.asset_id.clone(),
        best_bid: 0.0,
        best_ask: 1.0,
        mid: 0.5,
        last_trade_price: None,
        last_trade_side: None,
        last_trade_time_ms: None,
        event_time_ms: None,
        source: QuoteSource::LastTrade,
    });

    let mut q = base;
    q.last_trade_price = Some(tr.price);
    q.last_trade_side = Some(tr.side.clone());
    q.last_trade_time_ms = Some(tr.timestamp);
    q.event_time_ms = Some(tr.timestamp);
    q.source = QuoteSource::LastTrade;
    quotes.insert(tr.asset_id.clone(), q.clone());
    q
}

fn outcome_label(outcome: &str) -> String {
    if outcome.eq_ignore_ascii_case("up") {
        "Up".to_string()
    } else if outcome.eq_ignore_ascii_case("down") {
        "Down".to_string()
    } else {
        let mut it = outcome.chars();
        match it.next() {
            None => String::new(),
            Some(c) => c.to_uppercase().collect::<String>() + it.as_str(),
        }
    }
}

fn replace_coin_meta(
    meta: &mut HashMap<String, TokenMeta>,
    coin: &str,
    slug: &str,
    tokens: &HashMap<String, String>,
) {
    meta.retain(|_, m| m.coin != coin);
    for (outcome, tid) in tokens {
        meta.insert(
            tid.clone(),
            TokenMeta {
                coin: coin.to_string(),
                outcome: outcome.clone(),
                market_slug: slug.to_string(),
            },
        );
    }
}

async fn refresh_loop(
    gamma: Arc<GammaClient>,
    meta: Arc<RwLock<HashMap<String, TokenMeta>>>,
    state: Arc<RwLock<CollectorState>>,
    events: tokio::sync::broadcast::Sender<CollectorEvent>,
    cmd_tx: tokio::sync::mpsc::Sender<WsCommand>,
    history: Arc<PolyHistory>,
    coins: Vec<String>,
    interval: Duration,
) {
    let mut ticker = tokio::time::interval(interval);
    ticker.tick().await;

    loop {
        ticker.tick().await;
        let discovered = match discover(&gamma, &coins).await {
            Ok(d) => d,
            Err(e) => {
                let _ = events.send(CollectorEvent::Error {
                    message: e.to_string(),
                });
                continue;
            }
        };

        for coin in &coins {
            let c = coin.to_ascii_uppercase();
            let old = match state.read() {
                Ok(g) => g.markets.get(&c).cloned(),
                Err(_e) => {
                    // log::error!("collector state poisoned: {e}");
                    continue;
                }
            };
            let new_card = discovered.markets.get(&c).cloned();

            match (&old, &new_card) {
                (Some(prev), Some(next)) if needs_rotate(prev, next) => {
                    let off: Vec<_> = prev.tokens.values().cloned().collect();
                    let on: Vec<_> = next.tokens.values().cloned().collect();
                    let old_slug = prev.slug.clone();
                    let new_slug = next.slug.clone();

                    if cmd_tx
                        .send(WsCommand::Rotate {
                            unsubscribe: off,
                            subscribe: on,
                        })
                        .await
                        .is_err()
                    {
                        // log::warn!("WS command channel closed");
                        return;
                    }

                    if let Ok(mut m) = meta.write() {
                        replace_coin_meta(&mut m, &c, &new_slug, &next.tokens);
                    }
                    if let Ok(mut st) = state.write() {
                        for tid in prev.tokens.values() {
                            st.quotes.remove(tid);
                        }
                        st.markets.insert(c.clone(), next.clone());
                    }
                    let _ = events.send(CollectorEvent::MarketRotated {
                        coin: c,
                        old_slug: Some(old_slug),
                        new_slug,
                    });
                    history.flush_to_disk();
                }
                (None, Some(next)) => {
                    let ids: Vec<_> = next.tokens.values().cloned().collect();
                    if !ids.is_empty()
                        && cmd_tx.send(WsCommand::Subscribe(ids)).await.is_err()
                    {
                        // log::warn!("WS command channel closed");
                        return;
                    }
                    if let Ok(mut m) = meta.write() {
                        replace_coin_meta(&mut m, &c, &next.slug, &next.tokens);
                    }
                    if let Ok(mut st) = state.write() {
                        st.markets.insert(c.clone(), next.clone());
                    }
                    let _ = events.send(CollectorEvent::DiscoverySync {
                        coins_live: discovered.markets.len(),
                        tokens: discovered.all_token_ids.len(),
                    });
                }
                (Some(prev), None) => {
                    let ids: Vec<_> = prev.tokens.values().cloned().collect();
                    if !ids.is_empty()
                        && cmd_tx.send(WsCommand::Unsubscribe(ids)).await.is_err()
                    {
                        return;
                    }
                    if let Ok(mut m) = meta.write() {
                        m.retain(|_, tm| tm.coin != c);
                    }
                    if let Ok(mut st) = state.write() {
                        for tid in prev.tokens.values() {
                            st.quotes.remove(tid);
                        }
                        st.markets.remove(&c);
                    }
                    history.flush_to_disk();
                }
                _ => {}
            }
        }
    }
}

/// Gamma + WebSocket orchestration for live 5m up/down markets.
pub struct PolymarketCollector {
    config: CollectorConfig,
    gamma: Arc<GammaClient>,
    events: tokio::sync::broadcast::Sender<CollectorEvent>,
    state: Arc<RwLock<CollectorState>>,
}

impl PolymarketCollector {
    pub fn new(config: CollectorConfig) -> Result<Self> {
        let gamma = Arc::new(GammaClient::new(
            &config.gamma_host,
            config.gamma_timeout_secs,
        )?);
        let (events, _) = tokio::sync::broadcast::channel(1024);
        let state = Arc::new(RwLock::new(CollectorState::default()));
        Ok(Self {
            config,
            gamma,
            events,
            state,
        })
    }

    pub fn subscribe_events(&self) -> tokio::sync::broadcast::Receiver<CollectorEvent> {
        self.events.subscribe()
    }

    pub async fn run(self) -> Result<()> {
        let discovery = discover(&self.gamma, &self.config.coins).await?;
        if discovery.all_token_ids.is_empty() {
            // log::warn!("no live markets found for configured coins");
        }

        {
            let mut st = self
                .state
                .write()
                .map_err(|e| anyhow::anyhow!("state lock poisoned: {e}"))?;
            st.markets = discovery.markets.clone();
        }

        let meta = Arc::new(RwLock::new(discovery.token_meta));
        let mut ws = MarketWebSocket::new(self.config.ws_url.clone()).with_timing(
            Duration::from_secs(5),
            Duration::from_secs(20),
            Duration::from_secs(10),
        );
        let cmd_tx = ws.command_sender();
        let history = Arc::new(PolyHistory::from_env());

        let meta_b = Arc::clone(&meta);
        let state_b = Arc::clone(&self.state);
        let ev_b = self.events.clone();
        ws.set_on_book(move |ob| {
            let mg = match meta_b.read() {
                Ok(g) => g,
                Err(_e) => {
                    // log::error!("meta lock: {e}");
                    return;
                }
            };
            let Some(m) = mg.get(&ob.asset_id) else {
                return;
            };
            let m = m.clone();
            drop(mg);
            let q = match state_b.write() {
                Ok(mut st) => merge_book_quotes(&mut st.quotes, &m, &ob),
                Err(_e) => {
                    // log::error!("state lock: {e}");
                    return;
                }
            };
            let _ = ev_b.send(CollectorEvent::Book { quote: q });
        });

        let meta_p = Arc::clone(&meta);
        let state_p = Arc::clone(&self.state);
        let ev_p = self.events.clone();
        let hist_p = Arc::clone(&history);
        ws.set_on_price_change(move |_market, changes| {
            for pc in changes {
                let mg = match meta_p.read() {
                    Ok(g) => g,
                    Err(_) => continue,
                };
                let Some(m) = mg.get(&pc.asset_id) else {
                    continue;
                };
                let m = m.clone();
                drop(mg);
                let (bb, ba) = (pc.best_bid, pc.best_ask);
                let coin = m.coin.clone();
                let slug = m.market_slug.clone();
                let outcome = outcome_label(&m.outcome);
                let quote = match state_p.write() {
                    Ok(mut st) => merge_price_change(&mut st.quotes, &m, &pc),
                    Err(_) => continue,
                };
                hist_p.record_price_change(&coin, &outcome, &slug, bb, ba);
                let _ = ev_p.send(CollectorEvent::PriceChange { quote });
            }
        });

        let meta_t = Arc::clone(&meta);
        let state_t = Arc::clone(&self.state);
        let ev_t = self.events.clone();
        ws.set_on_trade(move |tr| {
            let mg = match meta_t.read() {
                Ok(g) => g,
                Err(_) => return,
            };
            let Some(m) = mg.get(&tr.asset_id) else {
                return;
            };
            let m = m.clone();
            drop(mg);
            let q = match state_t.write() {
                Ok(mut st) => merge_trade(&mut st.quotes, &m, &tr),
                Err(_) => return,
            };
            let _ = ev_t.send(CollectorEvent::Trade { quote: q });
        });

        ws.subscribe(&discovery.all_token_ids, true).await;

        let hist_interval = Arc::clone(&history);
        tokio::spawn(async move {
            let mut ticker =
                tokio::time::interval(Duration::from_secs(FLUSH_INTERVAL_SECS));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                hist_interval.flush_to_disk();
            }
        });

        let gamma_r = Arc::clone(&self.gamma);
        let meta_r = Arc::clone(&meta);
        let state_r = Arc::clone(&self.state);
        let events_r = self.events.clone();
        let hist_r = Arc::clone(&history);
        let coins = self.config.coins.clone();
        let interval = self.config.refresh_interval;
        tokio::spawn(refresh_loop(
            gamma_r,
            meta_r,
            state_r,
            events_r,
            cmd_tx,
            hist_r,
            coins,
            interval,
        ));

        let _ = self.events.send(CollectorEvent::DiscoverySync {
            coins_live: discovery.markets.len(),
            tokens: discovery.all_token_ids.len(),
        });

        ws.run(true).await;
        history.flush_to_disk();
        Ok(())
    }
}
