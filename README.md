# Polymarket Rust Collector

This project implements three pieces:

- `src/gamma.rs` — finds the current and next Polymarket Up/Down market for each coin/interval.
- `src/market_ws.rs` — keeps a resilient websocket connection to Polymarket CLOB market data and supports full subscribe, delta subscribe, and unsubscribe commands.
- `src/collector.rs` — maps `asset_id -> coin / side / current|next market`, refreshes current+next market discovery, and prints live prices.

## Run shape

```bash
cargo run
```

## What it prints

```text
[BTC Up | 5m | current] bid=0.4820 ask=0.4890 mid=0.4855 slug=btc-updown-5m-... asset_id=...
[BTC Down | 5m | current] bid=0.5110 ask=0.5180 mid=0.5145 slug=btc-updown-5m-... asset_id=...
[ETH Up | 5m | next] bid=0.5030 ask=0.5090 mid=0.5060 slug=eth-updown-5m-... asset_id=...
```

## Notes

- It tracks both **current** and **next** market, so the next market can already be subscribed before rollover.
- It does **delta subscription updates** instead of tearing down the websocket every time a market changes.
- It reacts to `new_market` and `market_resolved` websocket events by triggering a refresh, and it also runs a periodic refresh loop.
