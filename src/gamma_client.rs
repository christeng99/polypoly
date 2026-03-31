use anyhow::{bail, Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub struct GammaClient {
    client: Client,
    host: String,
    timeout: Duration,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Market {
    pub slug: String,
    #[serde(rename = "clobTokenIds")]
    pub clob_token_ids: Option<String>,
    pub outcomes: Option<String>,
    #[serde(rename = "acceptingOrders")]
    pub accepting_orders: Option<bool>,
    pub question: Option<String>,
}

impl GammaClient {
    pub fn new(host: &str, timeout_secs: u64) -> Result<Self> {
        let host = host.trim_end_matches('/').to_string();
        let client = Client::builder()
            .timeout(Duration::from_secs(timeout_secs))
            .build()
            .context("failed to build reqwest client")?;

        Ok(GammaClient {
            client,
            host,
            timeout: Duration::from_secs(timeout_secs),
        })
    }

    pub async fn get_market_by_slug(&self, slug: &str) -> Result<Option<Market>> {
        let url = format!("{}/markets/slug/{}", self.host, slug);
        let resp = self.client.get(url).timeout(self.timeout).send().await?;

        if resp.status().is_success() {
            Ok(Some(resp.json::<Market>().await?))
        } else if resp.status().as_u16() == 404 {
            Ok(None)
        } else {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("gamma returned {}: {}", status, body);
        }
    }

    pub async fn get_current_5m_market(&self, coin: &str) -> Result<Option<Market>> {
        self.get_current_interval_market(coin, 5, &[
            ("BTC", "btc-updown-5m"),
            ("ETH", "eth-updown-5m"),
            ("SOL", "sol-updown-5m"),
            ("XRP", "xrp-updown-5m"),
        ])
        .await
    }

    async fn get_current_interval_market(
        &self,
        coin: &str,
        interval_min: u64,
        prefixes: &[(&str, &str)],
    ) -> Result<Option<Market>> {
        let coin = coin.to_ascii_uppercase();
        let slug_prefix = prefixes
            .iter()
            .find(|(c, _)| *c == coin.as_str())
            .map(|(_, p)| *p)
            .with_context(|| format!("unsupported coin: {coin}"))?;

        let window_secs = interval_min * 60;
        let current_start = Self::window_start_unix_secs(window_secs);
        let next_start = current_start + window_secs;
        let prev_start = current_start.saturating_sub(window_secs);

        for ts in [current_start, next_start, prev_start] {
            let slug = format!("{slug_prefix}-{ts}");
            if let Some(market) = self.get_market_by_slug(&slug).await? {
                if market.accepting_orders.unwrap_or(false) {
                    return Ok(Some(market));
                }
            }
        }

        Ok(None)
    }

    fn window_start_unix_secs(window_secs: u64) -> u64 {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        now - now % window_secs
    }

    pub async fn parse_token_ids(&self, market: &Market) -> HashMap<String, String> {
        let clob_token_ids = market
            .clob_token_ids
            .clone()
            .unwrap_or_else(|| "[]".to_string());
        let token_ids: Vec<String> = serde_json::from_str(&clob_token_ids).unwrap_or_default();

        let outcomes = market
            .outcomes
            .clone()
            .unwrap_or_else(|| r#"["Up", "Down"]"#.to_string());
        let outcomes: Vec<String> =
            serde_json::from_str(&outcomes).unwrap_or_else(|_| vec!["Up".into(), "Down".into()]);

        map_outcomes(outcomes, token_ids)
    }
}

fn map_outcomes(outcomes: Vec<String>, values: Vec<String>) -> HashMap<String, String> {
    let mut result = HashMap::new();
    for (i, outcome) in outcomes.iter().enumerate() {
        if i < values.len() {
            result.insert(outcome.to_lowercase(), values[i].clone());
        }
    }
    result
}
