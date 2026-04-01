//! Polymarket CLOB REST client: L1 API key derivation + L2 HMAC + order placement.

use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD, engine::general_purpose::URL_SAFE, Engine as _};
use ethers_core::types::Address;
use hmac::{Hmac, Mac};
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE};
use reqwest::{Client, Url};
use sha2::Sha256;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

use crate::clob_signer::OrderSigner;
use crate::clob_types::{
    ApiCredentials, BalanceAllowanceResponse, OrderPayload, PostOrderBody, PostOrderResponse,
};
use serde_json::Value;

type HmacSha256 = Hmac<Sha256>;

/// L2 HMAC message uses this path only (no query string), matching py-clob `RequestArgs.request_path`.
const PATH_BALANCE_ALLOWANCE: &str = "/balance-allowance";
const PATH_UPDATE_BALANCE_ALLOWANCE: &str = "/balance-allowance/update";

pub struct ClobClient {
    http: Client,
    pub host: String,
    creds: Mutex<Option<ApiCredentials>>,
}

impl ClobClient {
    pub fn new(host: impl Into<String>, funder: impl Into<String>) -> Result<Self> {
        let http = Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .context("reqwest client")?;
        let funder_raw: String = funder.into();
        let _funder_addr: Address = funder_raw
            .trim()
            .parse()
            .with_context(|| format!("invalid funder address: {funder_raw}"))?;
        Ok(Self {
            http,
            host: host.into().trim_end_matches('/').to_string(),
            creds: Mutex::new(None),
        })
    }

    fn l1_headers(signer: &OrderSigner, timestamp: &str, nonce: u64, sig: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            "POLY_ADDRESS",
            HeaderValue::from_str(&signer.address_checksum()).unwrap(),
        );
        h.insert("POLY_SIGNATURE", HeaderValue::from_str(sig).unwrap());
        h.insert("POLY_TIMESTAMP", HeaderValue::from_str(timestamp).unwrap());
        h.insert("POLY_NONCE", HeaderValue::from_str(&nonce.to_string()).unwrap());
        h
    }

    /// Try `POST /auth/api-key`, then `GET /auth/derive-api-key` (same as Python `create_or_derive_api_key`).
    pub async fn create_or_derive_api_key(&self, signer: &OrderSigner) -> Result<ApiCredentials> {
        let nonce = 0u64;
        let timestamp = unix_secs_string();
        let sig = signer.sign_auth_message(&timestamp, nonce).await?;

        let url_create = format!("{}/auth/api-key", self.host);
        let mut headers = Self::l1_headers(signer, &timestamp, nonce, &sig);
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

        let r = self
            .http
            .post(&url_create)
            .headers(headers.clone())
            .send()
            .await;

        let parsed = match r {
            Ok(resp) if resp.status().is_success() => {
                let v: serde_json::Value = resp.json().await.context("api-key json")?;
                Self::parse_creds(&v)?
            }
            _ => {
                let url_derive = format!("{}/auth/derive-api-key", self.host);
                let resp = self
                    .http
                    .get(&url_derive)
                    .headers(Self::l1_headers(signer, &timestamp, nonce, &sig))
                    .send()
                    .await
                    .context("derive-api-key request")?;
                if !resp.status().is_success() {
                    let body = resp.text().await.unwrap_or_default();
                    anyhow::bail!("derive-api-key failed: {body}");
                }
                let v: serde_json::Value = resp.json().await.context("derive json")?;
                Self::parse_creds(&v)?
            }
        };

        *self.creds.lock().await = Some(parsed.clone());
        Ok(parsed)
    }

    fn parse_creds(v: &serde_json::Value) -> Result<ApiCredentials> {
        Ok(ApiCredentials {
            api_key: v
                .get("apiKey")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            secret: v
                .get("secret")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            passphrase: v
                .get("passphrase")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
        })
    }

    pub async fn init_creds(&self, signer: &OrderSigner) -> Result<()> {
        let _ = self.ensure_creds(signer).await?;
        Ok(())
    }

    async fn ensure_creds(&self, signer: &OrderSigner) -> Result<ApiCredentials> {
        {
            let g = self.creds.lock().await;
            if let Some(c) = g.as_ref() {
                if c.is_valid() {
                    return Ok(c.clone());
                }
            }
        }
        self.create_or_derive_api_key(signer).await
    }

    fn build_l2(creds: &ApiCredentials, signer_address: &str, method: &str, path: &str, body: &str) -> Result<HeaderMap> {
        let timestamp = unix_secs_string();
        let mut msg = format!("{timestamp}{method}{path}");
        if !body.is_empty() {
            msg.push_str(body);
        }
        let sig_b64 = poly_l2_signature(&creds.secret, &msg)?;
        let mut h = HeaderMap::new();
        h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        h.insert(
            "POLY_ADDRESS",
            HeaderValue::from_str(signer_address).context("POLY_ADDRESS")?,
        );
        h.insert("POLY_API_KEY", HeaderValue::from_str(&creds.api_key)?);
        h.insert("POLY_TIMESTAMP", HeaderValue::from_str(&timestamp)?);
        h.insert("POLY_PASSPHRASE", HeaderValue::from_str(&creds.passphrase)?);
        h.insert("POLY_SIGNATURE", HeaderValue::from_str(&sig_b64)?);
        Ok(h)
    }

    pub async fn post_order(
        self: &Arc<Self>,
        signer: &OrderSigner,
        order: OrderPayload,
        order_type: &str,
    ) -> Result<PostOrderResponse> {
        let creds = self.ensure_creds(signer).await?;

        let body_struct = PostOrderBody {
            order,
            owner: creds.api_key.clone(),
            order_type: order_type.to_string(),
            defer_exec: false,
        };
        let body = serde_json::to_string(&body_struct).context("serialize order")?;

        let path = "/order";
        let headers = Self::build_l2(&creds, &signer.address_checksum(), "POST", path, &body)?;

        let url = format!("{}{}", self.host, path);
        let resp = self
            .http
            .post(url)
            .headers(headers)
            .body(body)
            .send()
            .await
            .context("POST /order")?;

        let status = resp.status();
        let code = status.as_u16();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            println!(
                "[clob] POST /order -> {} {}",
                code,
                clip_one_line(&text, 180)
            );
            anyhow::bail!("order HTTP {}: {}", status, text);
        }
        let v: PostOrderResponse = serde_json::from_str(&text).unwrap_or(PostOrderResponse {
            success: Some(false),
            order_id: None,
            error_msg: Some(text.clone()),
        });
        let line = if v.success.unwrap_or(false) {
            format!(
                "[clob] POST /order -> {} ok oid={}",
                code,
                v.order_id
                    .as_deref()
                    .map(|o| clip_one_line(o, 32))
                    .unwrap_or_else(|| "-".into())
            )
        } else {
            format!(
                "[clob] POST /order -> {} reject {}",
                code,
                clip_one_line(
                    v.error_msg.as_deref().unwrap_or(&text),
                    140,
                )
            )
        };
        println!("{line}");
        Ok(v)
    }

    /// Best bid for `side=BUY`, best ask for `side=SELL` (per CLOB `/price` docs).
    pub async fn get_market_price(&self, token_id: &str, side: &str) -> Result<f64> {
        let mut u = Url::parse(&format!("{}/price", self.host)).context("price url")?;
        u.query_pairs_mut()
            .append_pair("token_id", token_id)
            .append_pair("side", side);
        let resp = self.http.get(u).send().await.context("GET /price")?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("price HTTP {}: {}", status, text);
        }
        let v: Value = serde_json::from_str(&text).context("price json")?;
        json_f64(&v, "price").with_context(|| format!("price field missing: {text}"))
    }

    pub async fn get_midpoint(&self, token_id: &str) -> Result<f64> {
        let mut u = Url::parse(&format!("{}/midpoint", self.host)).context("midpoint url")?;
        u.query_pairs_mut().append_pair("token_id", token_id);
        let resp = self.http.get(u).send().await.context("GET /midpoint")?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("midpoint HTTP {}: {}", status, text);
        }
        let v: Value = serde_json::from_str(&text).context("midpoint json")?;
        json_f64(&v, "mid_price")
            .or_else(|| json_f64(&v, "mid"))
            .with_context(|| format!("mid_price field missing: {text}"))
    }

    /// Refresh balance cache, then return sellable conditional token size in **shares**.
    /// Raw balance is floored to 4-decimal outcome steps (100 raw = 0.0001 shares) to match `clob_signer`.
    pub async fn conditional_token_sellable_shares(
        &self,
        signer: &OrderSigner,
        token_id: &str,
        signature_type: u8,
    ) -> Result<f64> {
        let _ = self
            .l2_get_balance_allowance_refresh(signer, token_id, signature_type)
            .await;
        let b = self
            .l2_get_balance_allowance(signer, token_id, signature_type)
            .await?;
        let raw: u128 = b.balance.parse().context("balance parse")?;
        const OUTCOME_STEP_RAW: u128 = 100; // matches `clob_signer::OUTCOME_SHARE_4DP_STEP`
        if raw < OUTCOME_STEP_RAW {
            return Ok(0.0);
        }
        let adj = (raw / OUTCOME_STEP_RAW) * OUTCOME_STEP_RAW;
        Ok(adj as f64 / 1_000_000.0)
    }

    async fn l2_get_balance_allowance_refresh(
        &self,
        signer: &OrderSigner,
        token_id: &str,
        signature_type: u8,
    ) -> Result<()> {
        let creds = self.ensure_creds(signer).await?;
        let st = signature_type.to_string();
        let mut u = Url::parse(&format!("{}{}", self.host, PATH_UPDATE_BALANCE_ALLOWANCE))
            .context("balance update url")?;
        u.query_pairs_mut()
            .append_pair("asset_type", "CONDITIONAL")
            .append_pair("token_id", token_id)
            .append_pair("signature_type", st.as_str());
        let headers = Self::build_l2(
            &creds,
            &signer.address_checksum(),
            "GET",
            PATH_UPDATE_BALANCE_ALLOWANCE,
            "",
        )?;
        let resp = self
            .http
            .get(u)
            .headers(headers)
            .send()
            .await
            .context("GET balance-allowance/update")?;
        let status = resp.status();
        if !status.is_success() {
            let t = resp.text().await.unwrap_or_default();
            anyhow::bail!("balance-allowance/update HTTP {}: {}", status, t);
        }
        Ok(())
    }

    async fn l2_get_balance_allowance(
        &self,
        signer: &OrderSigner,
        token_id: &str,
        signature_type: u8,
    ) -> Result<BalanceAllowanceResponse> {
        let creds = self.ensure_creds(signer).await?;
        let st = signature_type.to_string();
        let mut u =
            Url::parse(&format!("{}{}", self.host, PATH_BALANCE_ALLOWANCE)).context("balance url")?;
        u.query_pairs_mut()
            .append_pair("asset_type", "CONDITIONAL")
            .append_pair("token_id", token_id)
            .append_pair("signature_type", st.as_str());
        let headers = Self::build_l2(
            &creds,
            &signer.address_checksum(),
            "GET",
            PATH_BALANCE_ALLOWANCE,
            "",
        )?;
        let resp = self
            .http
            .get(u)
            .headers(headers)
            .send()
            .await
            .context("GET balance-allowance")?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("balance-allowance HTTP {}: {}", status, text);
        }
        serde_json::from_str(&text).context("balance-allowance json")
    }
}

fn clip_one_line(s: &str, max_chars: usize) -> String {
    let t: String = s.chars().filter(|c| !c.is_control()).collect();
    if t.chars().count() <= max_chars {
        t
    } else {
        t.chars().take(max_chars).collect::<String>() + "…"
    }
}

fn json_f64(v: &Value, key: &str) -> Option<f64> {
    let x = v.get(key)?;
    if let Some(f) = x.as_f64() {
        return Some(f);
    }
    x.as_str()?.parse().ok()
}

fn unix_secs_string() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

fn poly_l2_signature(secret: &str, message: &str) -> Result<String> {
    let key = decode_secret(secret)?;
    let mut mac = HmacSha256::new_from_slice(&key).context("hmac key")?;
    mac.update(message.as_bytes());
    let out = mac.finalize().into_bytes();
    Ok(URL_SAFE.encode(out))
}

fn decode_secret(secret: &str) -> Result<Vec<u8>> {
    let t = secret.trim();
    if let Ok(raw) = URL_SAFE.decode(t) {
        if !raw.is_empty() {
            return Ok(raw);
        }
    }
    if let Ok(raw) = STANDARD.decode(t) {
        if !raw.is_empty() {
            return Ok(raw);
        }
    }
    Ok(t.as_bytes().to_vec())
}
