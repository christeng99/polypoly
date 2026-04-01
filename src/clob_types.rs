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

/// Full CLOB order payload for `POST /order` (matches docs `Order` schema).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderPayload {
    pub maker: String,
    pub signer: String,
    pub taker: String,
    pub token_id: String,
    pub maker_amount: String,
    pub taker_amount: String,
    pub side: String,
    pub expiration: String,
    pub nonce: String,
    #[serde(rename = "feeRateBps")]
    pub fee_rate_bps: String,
    pub signature: String,
    pub salt: u64,
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
    pub defer_exec: bool,
}

#[derive(Debug, Deserialize)]
pub struct PostOrderResponse {
    pub success: Option<bool>,
    #[serde(rename = "orderId")]
    pub order_id: Option<String>,
    #[serde(rename = "errorMsg")]
    pub error_msg: Option<String>,
}

/// `GET /balance-allowance` (balance in 6-decimal fixed units for conditional tokens).
#[derive(Debug, Deserialize)]
pub struct BalanceAllowanceResponse {
    pub balance: String,
}
