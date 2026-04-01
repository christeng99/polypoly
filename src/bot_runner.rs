//! Per-wallet bot handle: sync `decide`, async `maybe_trade` (non-blocking for collector path).

use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;

use crate::action_store::ActionStore;
use crate::bot_config::BotConfig;
use crate::clob_client::ClobClient;
use crate::clob_signer::OrderSigner;
use crate::collector::TokenQuote;

#[derive(Debug, Clone, Default)]
pub struct BotStateView {
    pub last_decision: String,
    pub last_order_id: Option<String>,
    pub last_error: Option<String>,
}

pub struct BotHandle {
    cfg: BotConfig,
    clob: Arc<ClobClient>,
    signer: OrderSigner,
    last_fire: Mutex<Option<Instant>>,
    positions: Mutex<HashMap<String, f64>>,
    trade_lock: Mutex<()>,
    actions: Option<Arc<ActionStore>>,
    state: Mutex<BotStateView>,
}

impl BotHandle {
    pub async fn from_config(cfg: BotConfig, actions: Option<Arc<ActionStore>>) -> Result<Arc<Self>> {
        anyhow::ensure!(!cfg.id.is_empty(), "bot id must be non-empty");
        let signer = OrderSigner::from_private_key_hex(&cfg.private_key)?.with_chain_id(cfg.chain_id);
        let clob = Arc::new(ClobClient::new(&cfg.clob_host, &cfg.funder)?);
        clob.init_creds(&signer).await?;
        Ok(Arc::new(Self {
            cfg,
            clob,
            signer,
            last_fire: Mutex::new(None),
            positions: Mutex::new(HashMap::new()),
            trade_lock: Mutex::new(()),
            actions,
            state: Mutex::new(BotStateView::default()),
        }))
    }

    fn matches_leg(&self, quote: &TokenQuote) -> bool {
        self.cfg.legs.iter().any(|l| {
            l.coin == quote.coin && l.outcome.eq_ignore_ascii_case(quote.outcome.as_str())
        })
    }

    /// Evaluate quote and submit order asynchronously if rules pass and cooldown elapsed.
    pub async fn maybe_trade(self: &Arc<Self>, quote: TokenQuote) {
        let _guard = self.trade_lock.lock().await;
        if !self.matches_leg(&quote) {
            return;
        }

        {
            let mut lf = self.last_fire.lock().await;
            if let Some(t) = *lf {
                if t.elapsed() < self.cfg.cooldown {
                    return;
                }
            }
            *lf = Some(Instant::now());
        }

        let held = {
            let p = self.positions.lock().await;
            *p.get(&quote.token_id).unwrap_or(&0.0)
        };

        if held <= 0.0 {
            let Some(th) = self.cfg.buy_below else {
                return;
            };
            if !(quote.mid <= th && quote.best_ask > 0.0 && quote.best_ask < 1.0) {
                return;
            }
        
            let mut last_err: Option<String> = None;
            for attempt in 1..=3 {
                println!(
                    "[bot:{}] BUY attempt {attempt}/3 coin={} slug={} token={} px={} size={}",
                    self.cfg.id, quote.coin, quote.market_slug, quote.token_id, quote.best_ask, self.cfg.size
                );
                match self
                    .place_order(&quote.token_id, quote.best_ask, self.cfg.size, "BUY")
                    .await
                {
                    Ok(order_id) => {
                        println!(
                            "[bot:{}] BUY success coin={} slug={} token={} order_id={:?}",
                            self.cfg.id, quote.coin, quote.market_slug, quote.token_id, order_id
                        );
                        {
                            let mut p = self.positions.lock().await;
                            p.insert(quote.token_id.clone(), self.cfg.size);
                        }
                        if let Some(db) = &self.actions {
                            let _ = db.log_action(
                                &quote.coin,
                                &quote.market_slug,
                                self.cfg.size,
                                self.cfg.size * quote.best_ask,
                                "buy",
                            );
                        }
                        let mut st = self.state.lock().await;
                        st.last_decision = format!("BUY ok slug={} mid={:.4}", quote.market_slug, quote.mid);
                        st.last_order_id = order_id;
                        st.last_error = None;
                        return;
                    }
                    Err(e) => {
                        println!(
                            "[bot:{}] BUY failed attempt {attempt}/3 coin={} slug={} token={} err={}",
                            self.cfg.id, quote.coin, quote.market_slug, quote.token_id, e
                        );
                        last_err = Some(e.to_string());
                    }
                }
            }
            let mut st = self.state.lock().await;
            st.last_decision = "BUY failed after 3 retries".to_string();
            st.last_error = last_err;
            return;
        }

        let Some(th) = self.cfg.sell_above else {
            return;
        };
        if !(quote.mid >= th && quote.best_bid > 0.0 && quote.best_bid < 1.0) {
            return;
        }

        println!(
            "[bot:{}] SELL attempt coin={} slug={} token={} px={} size={}",
            self.cfg.id, quote.coin, quote.market_slug, quote.token_id, quote.best_bid, held
        );
        match self.place_order(&quote.token_id, quote.best_bid, held, "SELL").await {
            Ok(order_id) => {
                println!(
                    "[bot:{}] SELL success coin={} slug={} token={} order_id={:?}",
                    self.cfg.id, quote.coin, quote.market_slug, quote.token_id, order_id
                );
                {
                    let mut p = self.positions.lock().await;
                    p.remove(&quote.token_id);
                }
                if let Some(db) = &self.actions {
                    let _ = db.log_action(
                        &quote.coin,
                        &quote.market_slug,
                        held,
                        held * quote.best_bid,
                        "sell",
                    );
                }
                let mut st = self.state.lock().await;
                st.last_decision = format!("SELL ok slug={} mid={:.4}", quote.market_slug, quote.mid);
                st.last_order_id = order_id;
                st.last_error = None;
            }
            Err(e) => {
                println!(
                    "[bot:{}] SELL failed coin={} slug={} token={} err={}",
                    self.cfg.id, quote.coin, quote.market_slug, quote.token_id, e
                );
                let mut st = self.state.lock().await;
                st.last_decision = "SELL retry pending".to_string();
                st.last_error = Some(e.to_string());
            }
        }
    }

    async fn place_order(
        &self,
        token_id: &str,
        price: f64,
        size: f64,
        side: &str,
    ) -> Result<Option<String>> {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let (payload, sig) = self
            .signer
            .sign_limit_order(
                token_id,
                price,
                size,
                side,
                &self.cfg.funder,
                nonce,
                0,
                self.cfg.signature_type,
            )
            .await?;
        let resp = self
            .clob
            .post_order(&self.signer, payload, &sig, &self.cfg.order_type)
            .await?;
        if resp.success.unwrap_or(false) {
            Ok(resp.order_id)
        } else {
            anyhow::bail!(
                "order rejected: {}",
                resp.error_msg.unwrap_or_else(|| "unknown".to_string())
            );
        }
    }
}
