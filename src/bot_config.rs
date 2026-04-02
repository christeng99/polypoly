//! Load multi-bot configuration from environment (`POLY_ACTIVE_BOTS`, `POLY_BOT_<ID>_*`).

use anyhow::{bail, Context, Result};
use std::collections::HashSet;
use std::env;
use std::time::Duration;

/// One tradable leg, e.g. `btc_up` → coin `BTC`, outcome `up`.
#[derive(Debug, Clone)]
pub struct BotMarketLeg {
    pub coin: String,
    pub outcome: String,
}

#[derive(Debug, Clone)]
pub struct BotConfig {
    pub id: String,
    pub legs: Vec<BotMarketLeg>,
    /// Fire BUY when `mid <= buy_below` (and book has a valid ask).
    pub buy_below: Option<f64>,
    /// Optional ceiling: require `best_ask <= buy_price` (order limit is still live `best_ask`).
    pub buy_price: Option<f64>,
    /// With `buy_price`: skip if `best_ask < buy_price * buy_price_frac`. Without `buy_price` (legacy): skip if `best_ask > buy_below * buy_price_frac`.
    pub buy_price_frac: Option<f64>,
    /// Fire SELL when `mid >= sell_above` (and book has a valid bid).
    pub sell_above: Option<f64>,
    /// Target USDC notional per BUY (`POLY_BOT_<ID>_ONTIME_AMOUNT`). Min $1 enforced at runtime.
    pub ontime_amount_usd: f64,
    pub private_key: String,
    pub funder: String,
    pub clob_host: String,
    pub chain_id: u64,
    /// BUY order type: `FOK`, `GTC`, or `GTD` per Polymarket CLOB.
    pub order_type: String,
    /// SELL order type (default `GTC`). GTC rests on book; bot polls balance to detect full exit.
    pub sell_order_type: String,
    pub signature_type: u8,
    pub fee_rate_bps: u32,
    /// If set (>0), BUY is only allowed from round start until this many seconds.
    pub buy_limit_secs: Option<u64>,
    pub cooldown: Duration,
}

fn bot_env_key(bot_id: &str, field: &str) -> String {
    let id = bot_id.to_ascii_uppercase().replace('-', "_");
    format!("POLY_BOT_{id}_{field}")
}

fn env_trim_opt(name: &str) -> Option<String> {
    env::var(name).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Parse `btc_up`, `eth_down`, etc.
pub fn parse_leg(token: &str) -> Result<BotMarketLeg> {
    let t = token.trim().to_lowercase();
    let (coin, outcome) = t
        .rsplit_once('_')
        .filter(|(c, o)| !c.is_empty() && !o.is_empty())
        .ok_or_else(|| anyhow::anyhow!("market leg must be like btc_up, got {token:?}"))?;
    Ok(BotMarketLeg {
        coin: coin.to_ascii_uppercase(),
        outcome: outcome.to_string(),
    })
}

pub fn load_bots_from_env() -> Result<Vec<BotConfig>> {
    let list = env::var("POLY_ACTIVE_BOTS").unwrap_or_default();
    if list.trim().is_empty() {
        return Ok(vec![]);
    }

    let global_host = env_trim_opt("POLY_CLOB_HOST")
        .unwrap_or_else(|| "https://clob.polymarket.com".to_string());
    let chain_id: u64 = env::var("POLY_CHAIN_ID")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(137);

    let mut out = Vec::new();
    for id in list.split(',') {
        let id = id.trim();
        if id.is_empty() {
            continue;
        }

        let markets_k = bot_env_key(id, "MARKETS");
        let markets_s = env::var(&markets_k)
            .with_context(|| format!("{markets_k} required for bot {id}"))?;

        let mut legs = Vec::new();
        for part in markets_s.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            legs.push(parse_leg(part)?);
        }
        if legs.is_empty() {
            bail!("{markets_k}: no legs parsed");
        }

        let pk_k = bot_env_key(id, "PRIVATE_KEY");
        let private_key = env::var(&pk_k)
            .with_context(|| format!("{pk_k} required for bot {id}"))?
            .trim()
            .to_string();

        let funder_k = bot_env_key(id, "FUNDER");
        let funder = env::var(&funder_k)
            .with_context(|| format!("{funder_k} required for bot {id}"))?
            .trim()
            .to_string();

        let buy_below = env_trim_opt(&bot_env_key(id, "BUY_BELOW")).and_then(|s| s.parse().ok());
        let buy_price = env_trim_opt(&bot_env_key(id, "BUY_PRICE")).and_then(|s| s.parse().ok());
        let buy_price_frac = env_trim_opt(&bot_env_key(id, "BUY_PRICE_FRAC")).and_then(|s| s.parse().ok());
        let sell_above =
            env_trim_opt(&bot_env_key(id, "SELL_ABOVE")).and_then(|s| s.parse().ok());

        let ontime_amount_usd = env_trim_opt(&bot_env_key(id, "ONTIME_AMOUNT"))
            .and_then(|s| s.parse().ok())
            .unwrap_or(5.0);

        let order_type = env_trim_opt(&bot_env_key(id, "ORDER_TYPE")).unwrap_or_else(|| "FOK".to_string());
        let sell_order_type = env_trim_opt(&bot_env_key(id, "SELL_ORDER_TYPE")).unwrap_or_else(|| "GTC".to_string());

        let signature_type = env_trim_opt(&bot_env_key(id, "SIGNATURE_TYPE"))
            .and_then(|s| s.parse().ok())
            .unwrap_or(2u8);

        let fee_rate_bps = env_trim_opt(&bot_env_key(id, "FEE_RATE_BPS"))
            .and_then(|s| s.parse().ok())
            .unwrap_or(1000u32);

        let buy_limit_secs = env_trim_opt(&bot_env_key(id, "BUY_LIMIT_SECS"))
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|&s| s > 0);

        let cooldown_secs: u64 = env_trim_opt(&bot_env_key(id, "COOLDOWN_SECS"))
            .and_then(|s| s.parse().ok())
            .unwrap_or(30);

        out.push(BotConfig {
            id: id.to_string(),
            legs,
            buy_below,
            buy_price,
            buy_price_frac,
            sell_above,
            ontime_amount_usd,
            private_key,
            funder,
            clob_host: global_host.clone(),
            chain_id,
            order_type,
            sell_order_type,
            signature_type,
            fee_rate_bps,
            buy_limit_secs,
            cooldown: Duration::from_secs(cooldown_secs),
        });
    }

    Ok(out)
}

/// Union of configured coins for [`CollectorConfig::coins`].
pub fn collector_coins_from_bots(bots: &[BotConfig]) -> Vec<String> {
    let mut s: HashSet<String> = HashSet::new();
    for b in bots {
        for leg in &b.legs {
            s.insert(leg.coin.clone());
        }
    }
    let mut v: Vec<_> = s.into_iter().collect();
    v.sort();
    v
}
