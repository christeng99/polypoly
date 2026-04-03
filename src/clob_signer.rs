//! EIP-712 signing for CLOB L1 auth and orders (matches Polymarket Python reference).

use anyhow::{Context, Result};
use ethers_core::types::transaction::eip712::TypedData;
use ethers_core::types::{Address, U256};
use ethers_core::utils::to_checksum;
use ethers_signers::{LocalWallet, Signer};
use serde_json::json;
use std::str::FromStr;

/// Polymarket rejects marketable BUY orders whose maker USDC leg rounds below this (1e6 = $1).
pub const MIN_MARKETABLE_BUY_USDC_MICROS: u128 = 1_000_000;
const EXCHANGE_137: &str = "0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E";
const EXCHANGE_80002: &str = "0xdFE02Eb6733538f8Ea35D585af8DE5958AD99E40";

pub struct OrderSigner {
    wallet: LocalWallet,
    chain_id: u64,
}

impl OrderSigner {
    /// Integer representations used to compute order amounts without f64 drift.
    /// `size_deci4`: size in 0.0001-share units; `price_cents`: price in 0.01 units.
    /// outcome_micros = size_deci4 * 100, usdc_micros = size_deci4 * price_cents.
    /// The ratio usdc/outcome = price_cents/100 — always on 0.01 tick by construction.
    fn int_amounts(size: f64, price: f64) -> (u128, u128) {
        let price_cents = (price * 100.0).round() as u128;
        let size_deci4 = (size * 10_000.0).round() as u128;
        let outcome_micros = size_deci4 * 100;
        let usdc_micros = size_deci4 * price_cents;
        (outcome_micros, usdc_micros)
    }

    /// BUY maker USDC in micros, matching the integer arithmetic in [`sign_limit_order`].
    pub fn floored_buy_maker_usdc_micros(size: f64, price: f64) -> u128 {
        if !size.is_finite() || !price.is_finite() || size <= 0.0 || price <= 0.0 {
            return 0;
        }
        let (_outcome, usdc) = Self::int_amounts(size, price);
        usdc
    }

    fn exchange_address(&self) -> &'static str {
        match self.chain_id {
            80002 => EXCHANGE_80002,
            _ => EXCHANGE_137,
        }
    }

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

    /// Checksummed signer address (EIP-55).
    pub fn address_checksum(&self) -> String {
        to_checksum(&self.wallet.address(), None)
    }

    /// L1 signature for `GET /auth/derive-api-key` or `POST /auth/api-key`.
    pub async fn sign_auth_message(&self, timestamp: &str, nonce: u64) -> Result<String> {
        let addr = self.address_checksum();
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
    /// `salt` provides per-order uniqueness (official SDK uses `int(time.time())`).
    /// Order `nonce` is always 0 (cancellation group id, not a counter).
    pub async fn sign_limit_order(
        &self,
        token_id: &str,
        price: f64,
        size: f64,
        side: &str,
        maker: &str,
        salt: u64,
        fee_rate_bps: u32,
        signature_type: u8,
    ) -> Result<crate::clob_types::OrderPayload> {
        let is_buy = side.eq_ignore_ascii_case("BUY");
        let side_u = if is_buy { 0u8 } else { 1u8 };

        let (outcome_micros, usdc_micros) = Self::int_amounts(size, price);
        let (maker_amount, taker_amount) = if is_buy {
            (usdc_micros, outcome_micros)
        } else {
            (outcome_micros, usdc_micros)
        };
        anyhow::ensure!(maker_amount > 0, "maker amount rounds to zero");
        anyhow::ensure!(taker_amount > 0, "taker amount rounds to zero");

        let maker_addr = maker
            .parse::<Address>()
            .with_context(|| "invalid maker / funder address")?;
        let maker_cs = to_checksum(&maker_addr, None);
        let signer_addr = self.address_checksum();

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
                "name": "Polymarket CTF Exchange",
                "version": "1",
                "chainId": self.chain_id,
                "verifyingContract": self.exchange_address()
            },
            "message": {
                "salt": salt,
                "maker": maker_cs,
                "signer": signer_addr,
                "taker": "0x0000000000000000000000000000000000000000",
                "tokenId": token_u.to_string(),
                "makerAmount": maker_amount.to_string(),
                "takerAmount": taker_amount.to_string(),
                "expiration": "0",
                "nonce": 0,
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

        let signature = format!("0x{}", hex::encode(sig.to_vec()));
        let order = crate::clob_types::OrderPayload {
            maker: maker_cs,
            signer: signer_addr,
            taker: "0x0000000000000000000000000000000000000000".to_string(),
            token_id: token_id.to_string(),
            maker_amount: maker_amount.to_string(),
            taker_amount: taker_amount.to_string(),
            side: side.to_uppercase(),
            expiration: "0".to_string(),
            nonce: "0".to_string(),
            fee_rate_bps: fee_rate_bps.to_string(),
            signature,
            salt,
            signature_type,
        };
        Ok(order)
    }
}
