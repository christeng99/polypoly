//! Polymarket CLOB REST client: L1 API key derivation + L2 HMAC + order placement.

use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD, engine::general_purpose::URL_SAFE, Engine as _};
use hmac::{Hmac, Mac};
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE};
use reqwest::Client;
use sha2::Sha256;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

use crate::clob_signer::OrderSigner;
use crate::clob_types::{ApiCredentials, OrderPayload, PostOrderBody, PostOrderResponse};

type HmacSha256 = Hmac<Sha256>;

pub struct ClobClient {
    http: Client,
    pub host: String,
    pub funder: String,
    creds: Mutex<Option<ApiCredentials>>,
}

impl ClobClient {
    pub fn new(host: impl Into<String>, funder: impl Into<String>) -> Result<Self> {
        let http = Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .context("reqwest client")?;
        Ok(Self {
            http,
            host: host.into().trim_end_matches('/').to_string(),
            funder: funder.into(),
            creds: Mutex::new(None),
        })
    }

    fn l1_headers(signer: &OrderSigner, timestamp: &str, nonce: u64, sig: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            "POLY_ADDRESS",
            HeaderValue::from_str(&format!("{:?}", signer.address())).unwrap(),
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

    fn build_l2(creds: &ApiCredentials, funder: &str, method: &str, path: &str, body: &str) -> Result<HeaderMap> {
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
            HeaderValue::from_str(funder).context("POLY_ADDRESS")?,
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
        signature_hex: &str,
        order_type: &str,
    ) -> Result<PostOrderResponse> {
        let creds = self.ensure_creds(signer).await?;

        let body_struct = PostOrderBody {
            order,
            owner: self.funder.clone(),
            order_type: order_type.to_string(),
            signature: signature_hex.to_string(),
        };
        let body = serde_json::to_string(&body_struct).context("serialize order")?;
        // println!("[clob] POST /order body={body}");
        let path = "/order";
        let headers = Self::build_l2(&creds, &self.funder, "POST", path, &body)?;

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
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("order HTTP {}: {}", status, text);
        }
        let v: PostOrderResponse = serde_json::from_str(&text).unwrap_or(PostOrderResponse {
            success: Some(false),
            order_id: None,
            error_msg: Some(text),
        });
        Ok(v)
    }
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
