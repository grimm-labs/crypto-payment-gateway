FROM rust:bookworm AS builder
# protoc: the Breez SDK (Lightning) compiles its gRPC protos at build time.
RUN apt-get update \
    && apt-get install -y --no-install-recommends protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release -p gateway

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -r -u 10001 gateway

WORKDIR /app
COPY --from=builder /app/target/release/gateway /app/gateway

# Breez SDK local storage (SQLite). Mount a persistent volume here; a fresh
# named volume inherits this directory's ownership.
RUN mkdir -p /app/.breez-data && chown gateway:gateway /app/.breez-data
ENV BREEZ_STORAGE_DIR=/app/.breez-data

USER gateway

# Default only — overridden at runtime by `.env` / `-e PORT=...` (app also defaults to 3000).
ARG PORT=3000
ENV PORT=${PORT}
EXPOSE ${PORT}

CMD ["/app/gateway"]
