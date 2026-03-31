FROM rust:1.94 as builder
WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
WORKDIR /app

COPY --from=builder /app/target/release/polymarket_collector_rust /app/polymarket_collector_rust

ENV POLY_HISTORY_DIR=/workspace/poly_history

CMD ["/app/polymarket_collector_rust"]