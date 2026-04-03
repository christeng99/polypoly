mod action_store;
mod bot_config;
mod bot_runner;
mod clob_client;
mod clob_signer;
mod clob_types;
mod collector;
mod gamma_client;
mod poly_history;
mod websocket_client;

use anyhow::Result;
use action_store::ActionStore;
use bot_runner::BotHandle;
use collector::{CollectorConfig, CollectorEvent, PolymarketCollector};
use std::sync::Arc;
use tokio::sync::broadcast::error::RecvError;

#[tokio::main]
async fn main() -> Result<()> {
    // env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let _ = dotenvy::dotenv();

    let mut cfg = CollectorConfig::default();
    let bot_defs = bot_config::load_bots_from_env()?;
    let action_store = if bot_defs.is_empty() {
        None
    } else {
        Some(Arc::new(ActionStore::from_env()?))
    };
    let bots: Vec<Arc<BotHandle>> = if bot_defs.is_empty() {
        vec![]
    } else {
        // Always track the full hardcoded coin set from `CollectorConfig::default`,
        // regardless of which coins the bots trade.
        let mut handles = Vec::with_capacity(bot_defs.len());
        for def in bot_defs {
            handles.push(BotHandle::from_config(def, action_store.clone()).await?);
        }
        handles
    };

    let col = PolymarketCollector::new(cfg)?;
    let mut rx = col.subscribe_events();

    let bots_for_feed = bots.clone();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(ev) => match ev {
                    CollectorEvent::Book { quote }
                    | CollectorEvent::PriceChange { quote }
                    | CollectorEvent::Trade { quote } => {
                        for b in &bots_for_feed {
                            let h = b.clone();
                            let q = quote.clone();
                            tokio::spawn(async move {
                                h.maybe_trade(q).await;
                            });
                        }
                    }
                    _ => {}
                },
                Err(RecvError::Lagged(_n)) => {}
                Err(RecvError::Closed) => break,
            }
        }
    });

    col.run().await
}
