# syntax=docker/dockerfile:1

# ---- Схема зависимостей (кэш) ----
FROM lukemathwalker/cargo-chef:latest-rust-alpine AS chef
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# ---- Сборка ----
FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
# Сначала собираем только зависимости — кэшируется до изменения Cargo.toml
RUN cargo chef cook --release --recipe-path recipe.json
COPY . .
ENV BINARY_NAME=kostubetai
RUN cargo build --release --bin kostubetai

# ---- Runtime ----
FROM alpine:3.20
RUN addgroup -S bot && adduser -S -G bot -h /app bot
WORKDIR /app

COPY --from=builder /app/target/release/kostubetai /app/kostubetai
COPY --from=builder /app/migrations /app/migrations
COPY --from=builder /app/knowledge /app/knowledge

RUN mkdir -p /app/data && chown -R bot:bot /app
USER bot

ENV DB_PATH=/app/data/kostubetai.db

VOLUME ["/app/data"]
ENTRYPOINT ["/app/kostubetai"]
