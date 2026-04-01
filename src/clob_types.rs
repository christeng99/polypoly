//! JSON types for Polymarket CLOB REST API.

use serde::{Deserialize, Serialize};

/// L2 API credentials from `derive-api-key` / `api-key`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiCredentials {
    #[serde(rename = "apiKey")]
    pub api_key: String,
    pub secret: String,
    pub passphrase: String,
}

impl ApiCredentials {
    pub fn is_valid(&self) -> bool {
        !self.api_key.is_empty() && !self.secret.is_empty() && !self.passphrase.is_empty()
    }
}

/// Submitted order fields (REST), after signing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderPayload {
    pub token_id: String,
    pub price: f64,
    pub size: f64,
    pub side: String,
    pub maker: String,
    pub nonce: u64,
    #[serde(rename = "feeRateBps")]
    pub fee_rate_bps: u32,
    #[serde(rename = "signatureType")]
    pub signature_type: u8,
}

/// Body for `POST /order`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PostOrderBody {
    pub order: OrderPayload,
    pub owner: String,
    pub order_type: String,
    pub signature: String,
}

#[derive(Debug, Deserialize)]
pub struct PostOrderResponse {
    pub success: Option<bool>,
    #[serde(rename = "orderId")]
    pub order_id: Option<String>,
    #[serde(rename = "errorMsg")]
    pub error_msg: Option<String>,
}
