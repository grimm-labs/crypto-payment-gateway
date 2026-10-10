# syntax=docker/dockerfile:1.7
#
# Single multi-stage Dockerfile for every deployable.
#
#   docker build -f docker/Dockerfile --target api .        → Rust gateway (apps/api)
#   docker build -f docker/Dockerfile --target dashboard .  → dashboard SPA behind nginx (apps/dashboard)
#   docker build -f docker/Dockerfile --target all .        → both in one image (Kamal roles api / dashboard)

ARG NODE_VERSION=24
ARG PNPM_VERSION=11.25.0

# ── API: Rust release build ──────────────────────────────────────────────────
# protoc + libprotobuf-dev: the Breez SDK (Lightning) compiles its gRPC protos
# at build time and imports the google/protobuf/*.proto well-known types.
FROM rust:bookworm AS api-build
RUN apt-get update \
  && apt-get install -y --no-install-recommends protobuf-compiler libprotobuf-dev \
  && rm -rf /var/lib/apt/lists/*
WORKDIR /workspace
COPY Cargo.toml Cargo.lock ./
COPY apps/api ./apps/api
COPY packages/rust ./packages/rust
RUN cargo build --release -p gateway

# ── Dashboard: pnpm + Nx build ───────────────────────────────────────────────
FROM node:${NODE_VERSION}-alpine AS dashboard-build
ARG PNPM_VERSION
ENV NX_DAEMON=false CI=true
RUN npm install -g pnpm@${PNPM_VERSION} && pnpm config set store-dir /pnpm/store
WORKDIR /workspace
# Lockfile only: cached until the lockfile changes.
COPY pnpm-lock.yaml pnpm-workspace.yaml ./
RUN pnpm fetch
# Every workspace package.json is needed for a frozen install; only the
# dashboard sources are.
COPY package.json nx.json ./
COPY apps/api/package.json ./apps/api/package.json
COPY packages/rust/package.json ./packages/rust/package.json
COPY apps/dashboard ./apps/dashboard
RUN pnpm install --offline --frozen-lockfile
RUN pnpm nx run dashboard:build

# ── API runtime ──────────────────────────────────────────────────────────────
FROM debian:bookworm-slim AS api
RUN apt-get update \
  && apt-get install -y --no-install-recommends ca-certificates \
  && rm -rf /var/lib/apt/lists/* \
  && useradd -r -u 10001 gateway
WORKDIR /app
COPY --from=api-build /workspace/target/release/gateway /app/gateway
# Breez SDK (Lightning) storage. Mount a persistent volume here; a fresh named
# volume inherits this directory's ownership.
RUN mkdir -p /app/.breez-data && chown gateway:gateway /app/.breez-data
ENV PORT=3000 BREEZ_STORAGE_DIR=/app/.breez-data
USER gateway
EXPOSE 3000
CMD ["/app/gateway"]

# ── Dashboard runtime (nginx) ────────────────────────────────────────────────
FROM nginx:1.27-alpine AS dashboard
COPY docker/dashboard/nginx.conf docker/dashboard/nginx.conf.template docker/dashboard/start.sh /app/dashboard/
COPY --from=dashboard-build /workspace/apps/dashboard/dist /app/dashboard/html
ENV PORT=8080 API_URL=http://api:3000
USER nginx
EXPOSE 8080
CMD ["/app/dashboard/start.sh"]

# ── Everything (one image, Kamal roles `api` and `dashboard` with different `cmd`) ─
FROM debian:bookworm-slim AS all
RUN apt-get update \
  && apt-get install -y --no-install-recommends ca-certificates nginx gettext-base \
  && rm -rf /var/lib/apt/lists/* \
  && useradd -r -u 10001 gateway
WORKDIR /app
COPY --from=api-build /workspace/target/release/gateway /app/gateway
COPY docker/dashboard/nginx.conf docker/dashboard/nginx.conf.template docker/dashboard/start.sh /app/dashboard/
COPY --from=dashboard-build /workspace/apps/dashboard/dist /app/dashboard/html
RUN mkdir -p /app/.breez-data && chown gateway:gateway /app/.breez-data
ENV PORT=3000 BREEZ_STORAGE_DIR=/app/.breez-data
USER gateway
EXPOSE 3000 8080
# Default command: the API. The dashboard role runs /app/dashboard/start.sh.
CMD ["/app/gateway"]
