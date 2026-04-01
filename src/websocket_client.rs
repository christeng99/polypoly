//! WebSocket client for Polymarket CLOB market stream (orderbook, price changes, trades).

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

/// Market data WebSocket (default).
pub const WSS_MARKET_URL: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/market";

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;
type WsWrite = futures_util::stream::SplitSink<Ws, Message>;
type WsRead = futures_util::stream::SplitStream<Ws>;

type BoxBookCb = Arc<dyn Fn(OrderbookSnapshot) + Send + Sync>;
type BoxPriceCb = Arc<dyn Fn(String, Vec<PriceChange>) + Send + Sync>;
type BoxTradeCb = Arc<dyn Fn(LastTradePrice) + Send + Sync>;

fn json_f64(v: &Value) -> f64 {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or(0.0)
}

fn json_i64(v: &Value) -> i64 {
    v.as_i64()
        .or_else(|| v.as_u64().map(|u| u as i64))
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or(0)
}

fn json_string(v: &Value) -> String {
    v.as_str().unwrap_or("").to_string()
}

/// Single orderbook level (price only; size is ignored).
#[derive(Debug, Clone)]
pub struct OrderbookLevel {
    pub price: f64,
}

/// Full orderbook snapshot from a `book` event.
#[derive(Debug, Clone)]
pub struct OrderbookSnapshot {
    pub asset_id: String,
    pub timestamp: i64,
    pub bids: Vec<OrderbookLevel>,
    pub asks: Vec<OrderbookLevel>,
}

impl OrderbookSnapshot {
    pub fn from_message(msg: &Value) -> Self {
        let bids: Vec<OrderbookLevel> = msg
            .get("bids")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|b| {
                        Some(OrderbookLevel {
                            price: json_f64(b.get("price")?),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        let asks: Vec<OrderbookLevel> = msg
            .get("asks")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|a| {
                        Some(OrderbookLevel {
                            price: json_f64(a.get("price")?),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        let mut bids = bids;
        let mut asks = asks;
        bids.sort_by(|a, b| b.price.partial_cmp(&a.price).unwrap_or(std::cmp::Ordering::Equal));
        asks.sort_by(|a, b| a.price.partial_cmp(&b.price).unwrap_or(std::cmp::Ordering::Equal));

        Self {
            asset_id: msg
                .get("asset_id")
                .map(json_string)
                .unwrap_or_default(),
            timestamp: msg.get("timestamp").map(json_i64).unwrap_or(0),
            bids,
            asks,
        }
    }

    pub fn best_bid(&self) -> f64 {
        self.bids.first().map(|l| l.price).unwrap_or(0.0)
    }

    pub fn best_ask(&self) -> f64 {
        self.asks.first().map(|l| l.price).unwrap_or(1.0)
    }

    pub fn mid_price(&self) -> f64 {
        let bb = self.best_bid();
        let ba = self.best_ask();
        if bb > 0.0 && ba < 1.0 {
            (bb + ba) / 2.0
        } else if bb > 0.0 {
            bb
        } else if ba < 1.0 {
            ba
        } else {
            0.5
        }
    }
}

/// One leg inside a `price_change` event.
#[derive(Debug, Clone)]
pub struct PriceChange {
    pub asset_id: String,
    pub best_bid: f64,
    pub best_ask: f64,
}

impl PriceChange {
    pub fn from_dict(data: &Value) -> Self {
        Self {
            asset_id: data.get("asset_id").map(json_string).unwrap_or_default(),
            best_bid: data.get("best_bid").map(json_f64).unwrap_or(0.0),
            best_ask: data.get("best_ask").map(json_f64).unwrap_or(1.0),
        }
    }
}

/// `last_trade_price` event payload.
#[derive(Debug, Clone)]
pub struct LastTradePrice {
    pub asset_id: String,
    pub price: f64,
    pub side: String,
    pub timestamp: i64,
}

impl LastTradePrice {
    pub fn from_message(msg: &Value) -> Self {
        Self {
            asset_id: msg.get("asset_id").map(json_string).unwrap_or_default(),
            price: msg.get("price").map(json_f64).unwrap_or(0.0),
            side: msg.get("side").map(json_string).unwrap_or_default(),
            timestamp: msg.get("timestamp").map(json_i64).unwrap_or(0),
        }
    }
}

/// Commands processed from [`MarketWebSocket::command_sender`] inside the client run loop.
#[derive(Debug, Clone)]
pub enum WsCommand {
    /// Incremental subscribe (`operation: subscribe`).
    Subscribe(Vec<String>),
    Unsubscribe(Vec<String>),
    /// Unsubscribe old CLOB tokens, drop local books, then subscribe new tokens.
    Rotate {
        unsubscribe: Vec<String>,
        subscribe: Vec<String>,
    },
}

/// WebSocket client for Polymarket market data.
pub struct MarketWebSocket {
    url: String,
    reconnect_interval: Duration,
    recv_timeout: Duration,
    write: Option<WsWrite>,
    read: Option<WsRead>,
    running: bool,
    subscribed_assets: HashSet<String>,
    orderbooks: HashMap<String, OrderbookSnapshot>,
    on_book: Option<BoxBookCb>,
    on_price_change: Option<BoxPriceCb>,
    on_trade: Option<BoxTradeCb>,
    cmd_tx: mpsc::Sender<WsCommand>,
    cmd_rx: mpsc::Receiver<WsCommand>,
}

impl MarketWebSocket {
    pub fn new(url: impl Into<String>) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel(256);
        Self {
            url: url.into(),
            reconnect_interval: Duration::from_secs(5),
            recv_timeout: Duration::from_secs(25),
            write: None,
            read: None,
            running: false,
            subscribed_assets: HashSet::new(),
            orderbooks: HashMap::new(),
            on_book: None,
            on_price_change: None,
            on_trade: None,
            cmd_tx,
            cmd_rx,
        }
    }

    /// Sender for [`WsCommand`] while the client is running (same process as [`MarketWebSocket::run`]).
    pub fn command_sender(&self) -> mpsc::Sender<WsCommand> {
        self.cmd_tx.clone()
    }

    pub fn with_timing(
        mut self,
        reconnect_interval: Duration,
        ping_interval: Duration,
        _ping_timeout: Duration,
    ) -> Self {
        self.reconnect_interval = reconnect_interval;
        // Match Python: recv timeout ~= ping_interval + 5
        self.recv_timeout = ping_interval + Duration::from_secs(5);
        self
    }

    pub fn is_connected(&self) -> bool {
        self.write.is_some() && self.read.is_some()
    }

    pub fn set_on_book(&mut self, f: impl Fn(OrderbookSnapshot) + Send + Sync + 'static) {
        self.on_book = Some(Arc::new(f));
    }

    pub fn set_on_price_change(
        &mut self,
        f: impl Fn(String, Vec<PriceChange>) + Send + Sync + 'static,
    ) {
        self.on_price_change = Some(Arc::new(f));
    }

    pub fn set_on_trade(&mut self, f: impl Fn(LastTradePrice) + Send + Sync + 'static) {
        self.on_trade = Some(Arc::new(f));
    }

    pub async fn connect(&mut self) -> bool {
        match connect_async(&self.url).await {
            Ok((stream, _)) => {
                let (w, r) = stream.split();
                self.write = Some(w);
                self.read = Some(r);
                // log::info!("WebSocket connected to {}", self.url);
                true
            }
            Err(_e) => {
                // log::error!("WebSocket connection failed: {e}");
                false
            }
        }
    }

    /// Subscribe to assets. Uses initial `MARKET` payload when connected (same as Python).
    pub async fn subscribe(&mut self, asset_ids: &[String], replace: bool) -> bool {
        if asset_ids.is_empty() {
            return false;
        }

        if replace {
            self.subscribed_assets.clear();
            self.orderbooks.clear();
        }
        for id in asset_ids {
            self.subscribed_assets.insert(id.clone());
        }

        // log::info!(
        //     "subscribe() {} assets, connected={}",
        //     asset_ids.len(),
        //     self.is_connected()
        // );

        if !self.is_connected() {
            // log::info!("not connected yet; will subscribe after connect");
            return true;
        }

        let subscribe_msg = serde_json::json!({
            "assets_ids": asset_ids,
            "type": "MARKET",
        });

        self.send_json(&subscribe_msg).await
    }

    pub async fn subscribe_more(&mut self, asset_ids: &[String]) -> bool {
        if asset_ids.is_empty() {
            return false;
        }
        for id in asset_ids {
            self.subscribed_assets.insert(id.clone());
        }

        if !self.is_connected() {
            return true;
        }

        let subscribe_msg = serde_json::json!({
            "assets_ids": asset_ids,
            "operation": "subscribe",
        });

        self.send_json(&subscribe_msg).await
    }

    pub async fn unsubscribe(&mut self, asset_ids: &[String]) -> bool {
        if !self.is_connected() || asset_ids.is_empty() {
            return false;
        }
        for id in asset_ids {
            self.subscribed_assets.remove(id);
        }

        let msg = serde_json::json!({
            "assets_ids": asset_ids,
            "operation": "unsubscribe",
        });

        self.send_json(&msg).await
    }

    async fn send_json(&mut self, v: &Value) -> bool {
        let Some(w) = self.write.as_mut() else {
            return false;
        };
        let text = match serde_json::to_string(v) {
            Ok(s) => s,
            Err(_e) => {
                // log::error!("serialize subscribe: {e}");
                return false;
            }
        };
        match w.send(Message::Text(text.into())).await {
            Ok(()) => true,
            Err(_e) => {
                // log::error!("send failed: {e}");
                false
            }
        }
    }

    async fn handle_message(&mut self, data: &Value) {
        let event_type = data
            .get("event_type")
            .and_then(Value::as_str)
            .unwrap_or("");
        // log::debug!("event {event_type}");

        match event_type {
            "book" => {
                let snapshot = OrderbookSnapshot::from_message(data);
                let aid = snapshot.asset_id.clone();
                self.orderbooks.insert(aid, snapshot.clone());
                if let Some(cb) = &self.on_book {
                    cb(snapshot);
                }
            }
            "price_change" => {
                let market = data.get("market").map(json_string).unwrap_or_default();
                let changes: Vec<PriceChange> = data
                    .get("price_changes")
                    .and_then(Value::as_array)
                    .map(|arr| arr.iter().map(PriceChange::from_dict).collect())
                    .unwrap_or_default();
                if let Some(cb) = &self.on_price_change {
                    cb(market, changes);
                }
            }
            "last_trade_price" => {
                let trade = LastTradePrice::from_message(data);
                if let Some(cb) = &self.on_trade {
                    cb(trade);
                }
            }
            "tick_size_change" => {
                // log::debug!("tick_size_change: {data}");
            }
            _ => {
                // log::debug!("unknown event_type: {event_type}");
            }
        }
    }

    async fn process_text(&mut self, text: &str) {
        let data: Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(_e) => {
                // log::error!("json parse: {e}");
                return;
            }
        };

        if let Some(arr) = data.as_array() {
            for item in arr {
                self.handle_message(item).await;
            }
        } else {
            self.handle_message(&data).await;
        }
    }

    async fn drain_commands(&mut self) {
        while let Ok(cmd) = self.cmd_rx.try_recv() {
            self.apply_ws_command(cmd).await;
        }
    }

    async fn apply_ws_command(&mut self, cmd: WsCommand) {
        match cmd {
            WsCommand::Subscribe(ids) => {
                if ids.is_empty() {
                    return;
                }
                // log::info!("WS command: subscribe {} token(s)", ids.len());
                let _ = self.subscribe_more(&ids).await;
            }
            WsCommand::Unsubscribe(ids) => {
                if ids.is_empty() {
                    return;
                }
                // log::info!("WS command: unsubscribe {} token(s)", ids.len());
                let _ = self.unsubscribe(&ids).await;
                for id in &ids {
                    self.orderbooks.remove(id);
                }
            }
            WsCommand::Rotate {
                unsubscribe,
                subscribe,
            } => {
                // log::info!(
                //     "WS command: rotate off {} on {} token(s)",
                //     unsubscribe.len(),
                //     subscribe.len()
                // );
                if !unsubscribe.is_empty() {
                    let _ = self.unsubscribe(&unsubscribe).await;
                    for id in &unsubscribe {
                        self.orderbooks.remove(id);
                    }
                }
                if !subscribe.is_empty() {
                    let _ = self.subscribe_more(&subscribe).await;
                }
            }
        }
    }

    async fn run_loop(&mut self) {
        while self.running && self.is_connected() {
            self.drain_commands().await;
            let next = {
                let Some(read) = self.read.as_mut() else { break };
                tokio::time::timeout(self.recv_timeout, read.next())
            };

            match next.await {
                Err(_) => {
                    // log::warn!("WebSocket receive timeout");
                    continue;
                }
                Ok(None) => {
                    // log::warn!("WebSocket stream ended");
                    break;
                }
                Ok(Some(Err(_e))) => {
                    // log::warn!("WebSocket read error: {e}");
                    break;
                }
                Ok(Some(Ok(Message::Text(t)))) => {
                    self.process_text(&t).await;
                    self.drain_commands().await;
                }
                Ok(Some(Ok(Message::Ping(_)))) | Ok(Some(Ok(Message::Pong(_)))) => {}
                Ok(Some(Ok(Message::Binary(b)))) => {
                    if let Ok(s) = std::str::from_utf8(&b) {
                        self.process_text(s).await;
                        self.drain_commands().await;
                    }
                }
                Ok(Some(Ok(Message::Close(_)))) => {
                    // log::warn!("WebSocket close frame");
                    break;
                }
                Ok(Some(Ok(Message::Frame(_)))) => {}
            }
        }

        self.read.take();
        if let Some(mut w) = self.write.take() {
            let _ = w.close().await;
        }
    }

    pub async fn run(&mut self, auto_reconnect: bool) {
        self.running = true;

        while self.running {
            if !self.connect().await {
                if auto_reconnect {
                    // log::info!(
                    //     "reconnecting in {:?}...",
                    //     self.reconnect_interval
                    // );
                    tokio::time::sleep(self.reconnect_interval).await;
                    continue;
                }
                break;
            }

            if !self.subscribed_assets.is_empty() {
                let ids: Vec<String> = self.subscribed_assets.iter().cloned().collect();
                // log::info!("subscribing {} assets after connect", ids.len());
                let _ = self.subscribe(&ids, false).await;
            }

            self.run_loop().await;

            if !self.running {
                break;
            }

            if auto_reconnect {
                // log::info!("reconnecting in {:?}...", self.reconnect_interval);
                tokio::time::sleep(self.reconnect_interval).await;
            } else {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mid_price_empty_book() {
        let ob = OrderbookSnapshot::from_message(&serde_json::json!({
            "asset_id": "x",
            "bids": [],
            "asks": []
        }));
        assert!((ob.mid_price() - 0.5).abs() < 1e-9);
    }
}
