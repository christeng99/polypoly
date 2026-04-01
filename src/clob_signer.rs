//! EIP-712 signing for CLOB L1 auth and orders (matches Polymarket Python reference).

use anyhow::{Context, Result};
use ethers_core::types::transaction::eip712::TypedData;
use ethers_core::types::{Address, U256};
use ethers_core::utils::to_checksum;
use ethers_signers::{LocalWallet, Signer};
use serde_json::json;
use std::str::FromStr;

const USDC_DECIMALS: u32 = 6;

pub struct OrderSigner {
    wallet: LocalWallet,
    chain_id: u64,
}

impl OrderSigner {
    pub fn from_private_key_hex(key_hex: &str) -> Result<Self> {
        let s = key_hex.trim().trim_start_matches("0x");
        let wallet = LocalWallet::from_str(&format!("0x{s}"))
            .with_context(|| "invalid POLY private key")?;
        Ok(Self {
            wallet,
            chain_id: 137,
        })
    }

    pub fn with_chain_id(mut self, chain_id: u64) -> Self {
        self.chain_id = chain_id;
        self
    }

    pub fn address(&self) -> Address {
        self.wallet.address()
    }

    /// L1 signature for `GET /auth/derive-api-key` or `POST /auth/api-key`.
    pub async fn sign_auth_message(&self, timestamp: &str, nonce: u64) -> Result<String> {
        let addr = format!("{:?}", self.wallet.address());
        let typed: TypedData = serde_json::from_value(json!({
            "types": {
                "EIP712Domain": [
                    {"name": "name", "type": "string"},
                    {"name": "version", "type": "string"},
                    {"name": "chainId", "type": "uint256"}
                ],
                "ClobAuth": [
                    {"name": "address", "type": "address"},
                    {"name": "timestamp", "type": "string"},
                    {"name": "nonce", "type": "uint256"},
                    {"name": "message", "type": "string"}
                ]
            },
            "primaryType": "ClobAuth",
            "domain": {
                "name": "ClobAuthDomain",
                "version": "1",
                "chainId": self.chain_id
            },
            "message": {
                "address": addr,
                "timestamp": timestamp,
                "nonce": nonce,
                "message": "This message attests that I control the given wallet"
            }
        }))
        .context("typed data auth json")?;

        let sig = self
            .wallet
            .sign_typed_data(&typed)
            .await
            .context("sign auth")?;
        Ok(format!("0x{}", hex::encode(sig.to_vec())))
    }

    /// Sign limit order for CLOB. `maker` is the funder / Safe address (checksummed string).
    pub async fn sign_limit_order(
        &self,
        token_id: &str,
        price: f64,
        size: f64,
        side: &str,
        maker: &str,
        nonce: u64,
        fee_rate_bps: u32,
        signature_type: u8,
    ) -> Result<(crate::clob_types::OrderPayload, String)> {
        let side_u = if side.eq_ignore_ascii_case("BUY") { 0u8 } else { 1u8 };
        let maker_amount =
            (size * price * 10_f64.powi(USDC_DECIMALS as i32)).round() as u128;
        let taker_amount = (size * 10_f64.powi(USDC_DECIMALS as i32)).round() as u128;

        let maker_addr = maker
            .parse::<Address>()
            .with_context(|| "invalid maker / funder address")?;
        let maker_cs = to_checksum(&maker_addr, None);
        let signer_addr = format!("{:?}", self.wallet.address());

        let token_u = U256::from_dec_str(token_id.trim())
            .with_context(|| "token_id must be a decimal numeric string for EIP-712")?;

        let typed: TypedData = serde_json::from_value(json!({
            "types": {
                "EIP712Domain": [
                    {"name": "name", "type": "string"},
                    {"name": "version", "type": "string"},
                    {"name": "chainId", "type": "uint256"}
                ],
                "Order": [
                    {"name": "salt", "type": "uint256"},
                    {"name": "maker", "type": "address"},
                    {"name": "signer", "type": "address"},
                    {"name": "taker", "type": "address"},
                    {"name": "tokenId", "type": "uint256"},
                    {"name": "makerAmount", "type": "uint256"},
                    {"name": "takerAmount", "type": "uint256"},
                    {"name": "expiration", "type": "uint256"},
                    {"name": "nonce", "type": "uint256"},
                    {"name": "feeRateBps", "type": "uint256"},
                    {"name": "side", "type": "uint8"},
                    {"name": "signatureType", "type": "uint8"}
                ]
            },
            "primaryType": "Order",
            "domain": {
                "name": "ClobAuthDomain",
                "version": "1",
                "chainId": self.chain_id
            },
            "message": {
                "salt": "0",
                "maker": maker_cs,
                "signer": signer_addr,
                "taker": "0x0000000000000000000000000000000000000000",
                "tokenId": token_u.to_string(),
                "makerAmount": maker_amount.to_string(),
                "takerAmount": taker_amount.to_string(),
                "expiration": "0",
                "nonce": nonce,
                "feeRateBps": fee_rate_bps,
                "side": side_u,
                "signatureType": signature_type
            }
        }))
        .context("typed data order json")?;

        let sig = self
            .wallet
            .sign_typed_data(&typed)
            .await
            .context("sign order")?;

        let order = crate::clob_types::OrderPayload {
            token_id: token_id.to_string(),
            price,
            size,
            side: side.to_uppercase(),
            maker: maker_cs,
            nonce,
            fee_rate_bps,
            signature_type,
        };
        Ok((order, format!("0x{}", hex::encode(sig.to_vec()))))
    }
}
