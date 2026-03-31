mod collector;
mod gamma_client;
mod poly_history;
mod websocket_client;

use anyhow::Result;
use collector::{CollectorConfig, PolymarketCollector};

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let col = PolymarketCollector::new(CollectorConfig::default())?;
    let mut rx = col.subscribe_events();

    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(ev) => match ev {
                    _ => (),
                },
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    log::warn!("event receiver lagged by {n} messages");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    col.run().await
}
