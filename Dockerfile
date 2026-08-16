# ---------- Этап сборки ----------
FROM rust:1-slim-bookworm AS builder
WORKDIR /build

# Сначала только манифесты: слой с зависимостями кэшируется,
# пока не меняются Cargo.toml / Cargo.lock.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
 && cargo build --release \
 && rm -rf src target/release/deps/kostubetai*

COPY src ./src
RUN touch src/main.rs && cargo build --release

# ---------- Рабочий образ ----------
FROM debian:bookworm-slim AS runtime

# ca-certificates нужны reqwest (rustls) для HTTPS к Telegram и API модели.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --create-home --home-dir /app bot

WORKDIR /app
COPY --from=builder /build/target/release/kostubetai /usr/local/bin/kostubetai

# База SQLite по умолчанию лежит в data/memory.db — монтируется томом.
RUN mkdir -p /app/data && chown bot:bot /app/data
USER bot

CMD ["kostubetai"]
