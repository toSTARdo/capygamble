# --- Build Stage ---
FROM rust:latest AS builder
WORKDIR /app
COPY . .
RUN cargo build --release

# --- Runtime Stage ---
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/rust-telegram-bot /usr/local/bin/rust-telegram-bot
CMD ["rust-telegram-bot"]