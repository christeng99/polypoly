//! Per-wallet bot handle: sync `decide`, async `maybe_trade` (non-blocking for collector path).

use anyhow::Result;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

use crate::action_store::ActionStore;
use crate::bot_config::BotConfig;
use crate::clob_client::ClobClient;
use crate::clob_signer::{OrderSigner, MIN_MARKETABLE_BUY_USDC_MICROS};
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
    /// token_id → order_id of a resting GTC sell. While present, no new sell is placed.
    pending_sells: Mutex<HashMap<String, String>>,
    trade_lock: Mutex<()>,
    nonce_seq: AtomicU64,
    actions: Option<Arc<ActionStore>>,
    state: Mutex<BotStateView>,
}

fn short_tok(token_id: &str) -> String {
    if token_id.len() <= 12 {
        token_id.to_string()
    } else {
        format!("…{}", &token_id[token_id.len() - 10..])
    }
}

/// Re-submit a BUY only after `place_order` returns an error — not for soft gate/HTTP read failures.
const BUY_ORDER_RETRY_MAX: u32 = 7;

/// With `BUY_PRICE` set, BUY is allowed only when `best_ask` is in `[MIN_BUY_BEST_ASK, BUY_PRICE]`.
const MIN_BUY_BEST_ASK: f64 = 0.07;

fn clip_log(s: &str, max: usize) -> String {
    let mut it = s.chars();
    let chunk: String = it.by_ref().take(max).collect();
    if it.next().is_some() {
        chunk + "…"
    } else {
        chunk
    }
}

impl BotHandle {
    fn round_start_secs_from_slug(slug: &str) -> Option<u64> {
        let (_, tail) = slug.rsplit_once('-')?;
        tail.parse::<u64>().ok()
    }

    fn now_unix_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    fn next_salt(&self) -> u64 {
        self.nonce_seq.fetch_add(1, Ordering::Relaxed)
    }

    /// Outcome-token step (4 decimals) aligned with CLOB + `clob_signer`.
    const OUTCOME_SHARE_STEP: f64 = 1e-4;

    fn floor_shares_step(shares: f64) -> f64 {
        if !shares.is_finite() || shares <= 0.0 {
            return 0.0;
        }
        (shares / Self::OUTCOME_SHARE_STEP).floor() * Self::OUTCOME_SHARE_STEP
    }

    /// CLOB-style price tick (4 decimal places) — used for internal gate comparisons.
    fn floor_price_4dp(px: f64) -> f64 {
        if !px.is_finite() || px <= 0.0 {
            return 0.0;
        }
        (px * 10_000.0).floor() / 10_000.0
    }

    /// CLOB minimum tick size is **0.01**. All order prices must be rounded to 2 decimal places.
    fn round_price_tick(px: f64) -> f64 {
        if !px.is_finite() || px <= 0.0 {
            return 0.0;
        }
        (px * 100.0).round() / 100.0
    }

    /// `None` = pass. `Some(reason)` = skip BUY for `best_ask` band / legacy cap.
    fn buy_ask_price_gate_msg(cfg: &BotConfig, best_ask: f64, buy_below_th: f64) -> Option<String> {
        const EPS: f64 = 1e-9;
        if let Some(px) = cfg.buy_price {
            let cap = Self::floor_price_4dp(px.clamp(1e-6, 0.9999));
            if best_ask > cap + EPS {
                return Some(format!(
                    "best_ask {:.4} > BUY_PRICE {:.4}",
                    best_ask, cap
                ));
            }
            let floor_ask = Self::floor_price_4dp(MIN_BUY_BEST_ASK);
            if best_ask < floor_ask - EPS {
                return Some(format!(
                    "best_ask {:.4} < {:.2} (min ask with BUY_PRICE)",
                    best_ask, MIN_BUY_BEST_ASK
                ));
            }
        } else if let Some(frac) = cfg.buy_price_frac {
            let max_ask = Self::floor_price_4dp((buy_below_th * frac).clamp(1e-6, 0.9999));
            if best_ask > max_ask + EPS {
                return Some(format!(
                    "best_ask {:.4} > BUY_BELOW*FRAC {:.4}",
                    best_ask, max_ask
                ));
            }
        }
        None
    }

    /// Target ~`usd` maker notional at order limit price `px` (must match signed BUY price).
    fn buy_shares_for_usd(px: f64, usd: f64) -> anyhow::Result<f64> {
        anyhow::ensure!(px > 0.0 && px < 1.0, "invalid buy price {px}");
        let target = usd.max(1.0);
        let min_for_dollar = ((1.0 / px) / Self::OUTCOME_SHARE_STEP).ceil() * Self::OUTCOME_SHARE_STEP;
        let want = ((target / px) / Self::OUTCOME_SHARE_STEP).floor() * Self::OUTCOME_SHARE_STEP;
        let s = want.max(min_for_dollar);
        anyhow::ensure!(s > 0.0 && s.is_finite(), "buy size rounds to zero");
        Ok(s)
    }

    /// Raise `size` in share steps until CLOB maker USDC (after 1¢ floors) is at least $1.
    fn bump_buy_shares_for_clob_min(size: &mut f64, limit_px: f64) -> anyhow::Result<()> {
        let mut guard = 0u32;
        while OrderSigner::floored_buy_maker_usdc_micros(*size, limit_px) < MIN_MARKETABLE_BUY_USDC_MICROS {
            *size += Self::OUTCOME_SHARE_STEP;
            guard += 1;
            anyhow::ensure!(guard <= 50_000 && *size < 1e12, "buy size cannot satisfy CLOB $1 maker minimum");
        }
        Ok(())
    }

    pub async fn from_config(cfg: BotConfig, actions: Option<Arc<ActionStore>>) -> Result<Arc<Self>> {
        anyhow::ensure!(!cfg.id.is_empty(), "bot id must be non-empty");
        let signer = OrderSigner::from_private_key_hex(&cfg.private_key)?.with_chain_id(cfg.chain_id);
        println!(
            "[bot:{}] signer={} funder={}",
            cfg.id,
            signer.address_checksum(),
            cfg.funder
        );
        let clob = Arc::new(ClobClient::new(&cfg.clob_host, &cfg.funder)?);
        clob.init_creds(&signer).await?;
        Ok(Arc::new(Self {
            cfg,
            clob,
            signer,
            last_fire: Mutex::new(None),
            positions: Mutex::new(HashMap::new()),
            pending_sells: Mutex::new(HashMap::new()),
            trade_lock: Mutex::new(()),
            nonce_seq: AtomicU64::new(Self::now_unix_secs()),
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

        let held_mem = {
            let p = self.positions.lock().await;
            *p.get(&quote.token_id).unwrap_or(&0.0)
        };
        let mut held = held_mem;
        if held <= Self::OUTCOME_SHARE_STEP {
            if let Ok(ch) = self
                .clob
                .conditional_token_sellable_shares(
                    &self.signer,
                    &quote.token_id,
                    self.cfg.signature_type,
                )
                .await
            {
                let ch = Self::floor_shares_step(ch);
                if ch > Self::OUTCOME_SHARE_STEP {
                    let mut p = self.positions.lock().await;
                    p.insert(quote.token_id.clone(), ch);
                    held = ch;
                }
            }
        }

        if held <= 0.0 {
            let Some(th) = self.cfg.buy_below else {
                return;
            };
            if !(quote.mid <= th && quote.best_ask > 0.0 && quote.best_ask < 1.0) {
                return;
            }
            if Self::buy_ask_price_gate_msg(&self.cfg, quote.best_ask, th).is_some() {
                return;
            }
            if let Some(limit_secs) = self.cfg.buy_limit_secs {
                if let Some(round_start_secs) = Self::round_start_secs_from_slug(&quote.market_slug) {
                    let elapsed_secs = Self::now_unix_secs().saturating_sub(round_start_secs);
                    if elapsed_secs > limit_secs {
                        return;
                    }
                }
            }

            let mut last_err: Option<String> = None;
            let mut order_try: u32 = 0;
            loop {
                order_try += 1;
                if order_try > BUY_ORDER_RETRY_MAX {
                    break;
                }
                if order_try > 1 {
                    tokio::time::sleep(Duration::from_millis(120 * order_try as u64)).await;
                }

                let mid = self
                    .clob
                    .get_midpoint(&quote.token_id)
                    .await
                    .unwrap_or(quote.mid);
                let best_ask = match self.clob.get_market_price(&quote.token_id, "SELL").await {
                    Ok(px) => Self::round_price_tick(px),
                    Err(e) => {
                        last_err = Some(e.to_string());
                        println!(
                            "[bot:{}] BUY skip (no ask) {} tok={} {}",
                            self.cfg.id,
                            quote.coin,
                            short_tok(&quote.token_id),
                            e
                        );
                        break;
                    }
                };

                if !(mid <= th && best_ask > 0.0 && best_ask < 1.0) {
                    last_err = Some(format!(
                        "buy gate failed mid={mid:.4} ask={best_ask:.4} (threshold {th})"
                    ));
                    if order_try > 1 {
                        println!(
                            "[bot:{}] BUY retry {} stopped (live gate) {} tok={}",
                            self.cfg.id,
                            order_try,
                            quote.coin,
                            short_tok(&quote.token_id)
                        );
                    }
                    break;
                }

                if let Some(msg) = Self::buy_ask_price_gate_msg(&self.cfg, best_ask, th) {
                    last_err = Some(msg);
                    if order_try > 1 {
                        println!(
                            "[bot:{}] BUY retry {} stopped (ask band) {} tok={}",
                            self.cfg.id,
                            order_try,
                            quote.coin,
                            short_tok(&quote.token_id)
                        );
                    }
                    break;
                }

                let mut buy_size = match Self::buy_shares_for_usd(best_ask, self.cfg.ontime_amount_usd) {
                    Ok(s) => s,
                    Err(e) => {
                        last_err = Some(e.to_string());
                        break;
                    }
                };
                if let Err(e) = Self::bump_buy_shares_for_clob_min(&mut buy_size, best_ask) {
                    last_err = Some(e.to_string());
                    break;
                }
                let maker_usd = OrderSigner::floored_buy_maker_usdc_micros(buy_size, best_ask) as f64
                    / 1_000_000.0;

                println!(
                    "[bot:{}] BUY order {}/{} {} …{} mid={:.3} ask={:.3} sz={:.4} maker~${:.2} (ONTIME=${:.2})",
                    self.cfg.id,
                    order_try,
                    BUY_ORDER_RETRY_MAX,
                    quote.coin,
                    short_tok(&quote.token_id),
                    mid,
                    best_ask,
                    buy_size,
                    maker_usd,
                    self.cfg.ontime_amount_usd
                );
                match self
                    .place_order(&quote.token_id, best_ask, buy_size, "BUY", &self.cfg.order_type)
                    .await
                {
                    Ok(order_id) => {
                        let mut tracked = self
                            .clob
                            .conditional_token_sellable_shares(
                                &self.signer,
                                &quote.token_id,
                                self.cfg.signature_type,
                            )
                            .await
                            .unwrap_or(buy_size);
                        if tracked <= Self::OUTCOME_SHARE_STEP {
                            tracked = buy_size;
                        }
                        println!(
                            "[bot:{}] BUY ok {} tok={} pos={:.4} oid={:?}",
                            self.cfg.id,
                            quote.coin,
                            short_tok(&quote.token_id),
                            tracked,
                            order_id
                        );
                        {
                            let mut p = self.positions.lock().await;
                            p.insert(quote.token_id.clone(), tracked);
                        }
                        if let Some(db) = &self.actions {
                            let _ = db.log_action(
                                &quote.coin,
                                &quote.market_slug,
                                tracked,
                                tracked * best_ask,
                                "buy",
                            );
                        }
                        let mut st = self.state.lock().await;
                        st.last_decision = format!("BUY ok slug={} mid={:.4}", quote.market_slug, mid);
                        st.last_order_id = order_id;
                        st.last_error = None;
                        drop(st);
                        self.place_post_buy_gtc_sell(&quote, tracked).await;
                        return;
                    }
                    Err(e) => {
                        println!(
                            "[bot:{}] BUY order {}/{} fail {} tok={} | {}",
                            self.cfg.id,
                            order_try,
                            BUY_ORDER_RETRY_MAX,
                            quote.coin,
                            short_tok(&quote.token_id),
                            clip_log(&e.to_string(), 90)
                        );
                        last_err = Some(e.to_string());
                        if let Ok(bal) = self
                            .clob
                            .conditional_token_sellable_shares(
                                &self.signer,
                                &quote.token_id,
                                self.cfg.signature_type,
                            )
                            .await
                        {
                            let bal = Self::floor_shares_step(bal);
                            if bal > Self::OUTCOME_SHARE_STEP {
                                let tracked = bal;
                                println!(
                                    "[bot:{}] BUY filled despite error {} tok={} pos={:.4}",
                                    self.cfg.id,
                                    quote.coin,
                                    short_tok(&quote.token_id),
                                    tracked
                                );
                                {
                                    let mut p = self.positions.lock().await;
                                    p.insert(quote.token_id.clone(), tracked);
                                }
                                if let Some(db) = &self.actions {
                                    let _ = db.log_action(
                                        &quote.coin,
                                        &quote.market_slug,
                                        tracked,
                                        tracked * best_ask,
                                        "buy",
                                    );
                                }
                                let mut st = self.state.lock().await;
                                st.last_decision = format!(
                                    "BUY ok (balance) slug={} mid={:.4}",
                                    quote.market_slug, mid
                                );
                                st.last_order_id = None;
                                st.last_error = None;
                                drop(st);
                                self.place_post_buy_gtc_sell(&quote, tracked).await;
                                return;
                            }
                        }
                    }
                }
            }
            let mut st = self.state.lock().await;
            st.last_decision =
                format!("BUY incomplete (max {BUY_ORDER_RETRY_MAX} order retries on submit errors)");
            st.last_error = last_err;
            return;
        }

        // ── SELL SIDE: GTC with balance tracking ──────────────────────────
        let has_pending = {
            let ps = self.pending_sells.lock().await;
            ps.contains_key(&quote.token_id)
        };

        if has_pending {
            let balance = self
                .clob
                .conditional_token_sellable_shares(
                    &self.signer,
                    &quote.token_id,
                    self.cfg.signature_type,
                )
                .await
                .map(Self::floor_shares_step)
                .unwrap_or(held);

            if balance <= Self::OUTCOME_SHARE_STEP {
                println!(
                    "[bot:{}] SELL complete {} tok={} (balance ≈ 0)",
                    self.cfg.id, quote.coin, short_tok(&quote.token_id)
                );
                {
                    let mut p = self.positions.lock().await;
                    p.remove(&quote.token_id);
                }
                {
                    let mut ps = self.pending_sells.lock().await;
                    ps.remove(&quote.token_id);
                }
                let mut st = self.state.lock().await;
                st.last_decision = format!("SELL filled slug={}", quote.market_slug);
                st.last_order_id = None;
                st.last_error = None;
            } else {
                {
                    let mut p = self.positions.lock().await;
                    p.insert(quote.token_id.clone(), balance);
                }
                println!(
                    "[bot:{}] SELL pending {} tok={} bal={:.4}",
                    self.cfg.id, quote.coin, short_tok(&quote.token_id), balance
                );
            }
            return;
        }

        let Some(th) = self.cfg.sell_above else {
            return;
        };
        if !(quote.mid >= th && quote.best_bid > 0.0 && quote.best_bid < 1.0) {
            return;
        }

        let mid = self
            .clob
            .get_midpoint(&quote.token_id)
            .await
            .unwrap_or(quote.mid);
        let best_bid = match self.clob.get_market_price(&quote.token_id, "BUY").await {
            Ok(px) => Self::round_price_tick(px),
            Err(e) => {
                println!(
                    "[bot:{}] SELL skip bid {} tok={} {}",
                    self.cfg.id, quote.coin, short_tok(&quote.token_id), e
                );
                let mut st = self.state.lock().await;
                st.last_error = Some(e.to_string());
                return;
            }
        };

        if !(mid >= th && best_bid > 0.0 && best_bid < 1.0) {
            return;
        }

        let sellable = match self
            .clob
            .conditional_token_sellable_shares(
                &self.signer,
                &quote.token_id,
                self.cfg.signature_type,
            )
            .await
        {
            Ok(s) => Self::floor_shares_step(s),
            Err(e) => {
                println!(
                    "[bot:{}] SELL bal? {} tok={} fallback×0.999 ({})",
                    self.cfg.id, quote.coin, short_tok(&quote.token_id), e
                );
                Self::floor_shares_step(held * 0.999)
            }
        };

        if sellable <= Self::OUTCOME_SHARE_STEP {
            println!(
                "[bot:{}] SELL skip 0 {} tok={} trk={:.4}",
                self.cfg.id, quote.coin, short_tok(&quote.token_id), held
            );
            let mut p = self.positions.lock().await;
            p.remove(&quote.token_id);
            return;
        }

        println!(
            "[bot:{}] SELL {} …{} mid={:.3} bid={:.3} sz={:.4} type={} trk={:.4}",
            self.cfg.id,
            quote.coin,
            short_tok(&quote.token_id),
            mid,
            best_bid,
            sellable,
            self.cfg.sell_order_type,
            held
        );
        match self
            .place_order(&quote.token_id, best_bid, sellable, "SELL", &self.cfg.sell_order_type)
            .await
        {
            Ok(order_id) => {
                println!(
                    "[bot:{}] SELL placed {} tok={} oid={:?} type={}",
                    self.cfg.id, quote.coin, short_tok(&quote.token_id), order_id, self.cfg.sell_order_type
                );
                if let Some(db) = &self.actions {
                    let _ = db.log_action(
                        &quote.coin,
                        &quote.market_slug,
                        sellable,
                        sellable * best_bid,
                        "sell",
                    );
                }
                {
                    let mut ps = self.pending_sells.lock().await;
                    ps.insert(
                        quote.token_id.clone(),
                        order_id.clone().unwrap_or_default(),
                    );
                }
                let mut st = self.state.lock().await;
                st.last_decision = format!(
                    "SELL placed slug={} mid={:.4} type={}",
                    quote.market_slug, mid, self.cfg.sell_order_type
                );
                st.last_order_id = order_id;
                st.last_error = None;
            }
            Err(e) => {
                println!(
                    "[bot:{}] SELL fail {} tok={} | {}",
                    self.cfg.id,
                    quote.coin,
                    short_tok(&quote.token_id),
                    clip_log(&e.to_string(), 90)
                );
                let mut st = self.state.lock().await;
                st.last_decision = "SELL submit error".to_string();
                st.last_error = Some(e.to_string());
            }
        }
    }

    /// If `SELL_PRICE` is set, rest a maker GTC sell right after a BUY (same size as filled balance).
    async fn place_post_buy_gtc_sell(&self, quote: &TokenQuote, sellable_shares: f64) {
        let Some(limit_px_raw) = self.cfg.sell_price else {
            return;
        };
        let sell_px = Self::round_price_tick(limit_px_raw);
        if sell_px <= 0.0 || sell_px >= 1.0 {
            println!(
                "[bot:{}] post-BUY GTC SELL skip: invalid SELL_PRICE {}",
                self.cfg.id, limit_px_raw
            );
            return;
        }
        let sz = Self::floor_shares_step(sellable_shares);
        if sz <= Self::OUTCOME_SHARE_STEP {
            return;
        }
        println!(
            "[bot:{}] post-BUY GTC SELL {} tok={} px={:.3} sz={:.4}",
            self.cfg.id,
            quote.coin,
            short_tok(&quote.token_id),
            sell_px,
            sz
        );
        match self
            .place_order(&quote.token_id, sell_px, sz, "SELL", "GTC")
            .await
        {
            Ok(order_id) => {
                println!(
                    "[bot:{}] post-BUY GTC SELL placed {} tok={} oid={:?}",
                    self.cfg.id,
                    quote.coin,
                    short_tok(&quote.token_id),
                    order_id
                );
                if let Some(db) = &self.actions {
                    let _ = db.log_action(
                        &quote.coin,
                        &quote.market_slug,
                        sz,
                        sz * sell_px,
                        "sell",
                    );
                }
                {
                    let mut ps = self.pending_sells.lock().await;
                    ps.insert(
                        quote.token_id.clone(),
                        order_id.clone().unwrap_or_default(),
                    );
                }
                let mut st = self.state.lock().await;
                st.last_decision = format!(
                    "BUY ok + GTC SELL@{} slug={}",
                    sell_px, quote.market_slug
                );
                st.last_order_id = order_id;
                st.last_error = None;
            }
            Err(e) => {
                println!(
                    "[bot:{}] post-BUY GTC SELL fail {} tok={} | {}",
                    self.cfg.id,
                    quote.coin,
                    short_tok(&quote.token_id),
                    clip_log(&e.to_string(), 90)
                );
                let mut st = self.state.lock().await;
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
        order_type: &str,
    ) -> Result<Option<String>> {
        let salt = self.next_salt();
        let payload = self
            .signer
            .sign_limit_order(
                token_id,
                price,
                size,
                side,
                &self.cfg.funder,
                salt,
                self.cfg.fee_rate_bps,
                self.cfg.signature_type,
            )
            .await?;
        let resp = self
            .clob
            .post_order(&self.signer, payload, order_type)
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
