# Crypto Payment Gateway

[![CI](https://github.com/nejos97/crypto-payment-gateway/actions/workflows/ci.yml/badge.svg)](https://github.com/nejos97/crypto-payment-gateway/actions/workflows/ci.yml)

Rust service for accepting USDT deposits on **Tron**, **Ethereum**, and **Solana**, and
bitcoin payments **on-chain** and over **Lightning**.

## Features

- Create unique deposit addresses per payment (`POST /api/v1/deposit-addresses`)
- Monitor blockchain transfers and confirm deposits
- Send webhooks for payment lifecycle events — `pending` (address created), `paid`
  (deposit confirmed), `expired` — to every active endpoint **subscribed to that event**;
  endpoints are managed from the dashboard (**Webhooks**, admin), with per-endpoint delivery state
- HD wallet derivation from `WALLET_MNEMONIC`
- Multi-network support: Tron (TRC-20), Ethereum (ERC-20), Solana (SPL), Bitcoin (BIP84
  on-chain) and Lightning (BOLT11 via the Breez SDK) — see [Bitcoin & Lightning](#bitcoin--lightning)
- **Admin dashboard** (`apps/dashboard`, React + shadcn/ui): wallet balances, payments,
  webhook monitoring/retry, API key and staff management — served on its own port
- Merchant API protected by **API keys** (`x-api-key`) created from the dashboard;
  admin API (`/api/admin/v1`) protected by staff **JWT** (roles: admin / operator)

## Repository layout

An [Nx](https://nx.dev) monorepo (pnpm workspaces) with two apps:

```
apps/
  api/              Rust gateway binary (Cargo package `gateway`)
  dashboard/        admin SPA (Vite + React)
packages/
  rust/             Rust library crates: domain, config, wallet, auth, db, chain,
                    lightning, jobs, http-api (one Cargo workspace, root Cargo.toml)
docker/             single multi-stage Dockerfile (targets api, dashboard, all) + nginx config
config/deploy.yml   Kamal deployment (with .kamal/secrets)
compose.yaml        full stack from images · compose.dev.yaml: postgres + redis only
```

## Quick start

Requirements: Rust (stable), Node 24 (`.tool-versions` / `.nvmrc`), pnpm 11.25.0, Docker.

```bash
cp .env.example .env
# Edit .env with your secrets and API keys

pnpm install
pnpm infra:up              # postgres + redis (docker compose -f compose.dev.yaml up -d)
pnpm nx run api:dev        # cargo run -p gateway
```

On first boot the binary applies SQL migrations (`packages/rust/db/migrations`) via sqlx.

API: `http://localhost:3000` — `GET /healthz` (liveness), `GET /readyz` (Postgres + Redis;
also reports the Lightning SDK as `ok`/`disabled`/`error`, informational only)

Building needs `protoc` (the Breez SDK compiles gRPC protos): `brew install protobuf` /
`apt-get install protobuf-compiler libprotobuf-dev` (the latter ships the
`google/protobuf/*.proto` well-known types). The Docker image installs both.

### Dashboard (dev)

```bash
pnpm nx run dashboard:dev   # http://localhost:5173, proxies /api to localhost:3000
pnpm dev                    # api + dashboard together
```

Set `JWT_SECRET` (min 32 chars) in `.env` first — without it the admin API is disabled.
On first visit the dashboard walks you through creating the initial admin account
(first name, last name, email, password). Admins can then add staff (role `admin` or
`operator`), create API keys, generate deposit addresses, and monitor/retry webhooks.

### Docker

```bash
docker compose up --build
```

Starts postgres, redis, the API on `:3000` and the dashboard on `:8080`
(nginx serving the built SPA, proxying `/api` to the api service — no CORS needed).
Both images come from `docker/Dockerfile` (targets `api` and `dashboard`).

## Testing

Unit tests live in the `domain`, `wallet`, `config`, `chain`, `lightning-client`, `jobs`, `auth` and `http-api` crates (BIP39 / BIP84 / HD address golden vectors, amount tolerance and per-token decimals, enum roundtrips, Esplora parsing and confirmation math, the Breez SDK wrapper against a mocked `LightningOps`, webhook retry schedule, request validation). None needs a database or network access. CI runs the same suite:

```bash
pnpm nx affected -t lint test build   # what CI runs (only the affected apps)
pnpm nx run api:test                  # cargo test --workspace
```

There are no integration tests for the API or chain providers yet.

## API

> **Breaking change:** every `/api/v1` endpoint now requires an API key sent as
> `x-api-key: ...`. Create one from the dashboard (**API Keys → New API key**)
> before updating your integrations. `/healthz`, `/readyz` and `/api/admin/v1`
> are not affected. Keys are `[{tag}_]{8}_{40}` — the optional tag (max 5
> alphanumeric chars) is configurable in **Settings → API key prefix**; changing
> it only affects newly created keys, existing keys keep working.

### Create deposit address

```bash
curl -X POST http://localhost:3000/api/v1/deposit-addresses \
  -H "Content-Type: application/json" \
  -H "x-api-key: $API_KEY" \
  -d '{
    "network": "tron",
    "token": "USDT",
    "expected_amount": "25.00"
  }'
```

Bitcoin and Lightning take the amount **in satoshis** through `expected_amount_sats`
(integer string, 1 – 2 100 000 000 000 000); `expected_amount` is rejected on those networks
so `1500` can never be ambiguous between sats and BTC. The token must match the network
(`USDT` on tron/ethereum/solana, `BTC` on bitcoin/lightning) — otherwise `400 "Token not
supported on this network"`.

```bash
# Lightning: returns a BOLT11 invoice in `address` (optional `description`, ≤ 128 chars)
curl -X POST http://localhost:3000/api/v1/deposit-addresses \
  -H "Content-Type: application/json" -H "x-api-key: $API_KEY" \
  -d '{ "network": "lightning", "token": "BTC", "expected_amount_sats": "150000", "description": "order #123" }'
```

Lightning creation returns `503` when the Breez SDK is not configured/connected and a
generic `500 "Failed to create invoice"` when the SDK fails (details are only logged). The
invoice expiry equals the payment's `expires_at`. Every response carries
`expected_amount_raw` (base units: sats for BTC, 10⁻⁶ for USDT) next to `expected_amount`.

### List deposit addresses

```bash
curl "http://localhost:3000/api/v1/deposit-addresses?status=pending&limit=20"
```

Query params (optional):

- `status`: `pending`, `paid`, `expired`
- `network`: `tron`, `solana`, `ethereum`, `bitcoin`, `lightning`
- `limit`: default `50`, max `100`
- `offset`: default `0`

### Get deposit address status

```bash
curl http://localhost:3000/api/v1/deposit-addresses/<id>
```

Each entry in the returned `deposits[]` includes a `webhook` object with the delivery state (`status`: `pending` / `delivered` / `failed`, `attempts`, `last_response`, `last_error`, `updated_at`), or `null` if no delivery was attempted yet.

### Retry a webhook delivery

```bash
curl -X POST http://localhost:3000/api/v1/deposits/<deposit_id>/webhooks/retry
```

`<deposit_id>` is the `id` of an entry in `deposits[]` above. Retry resets the delivery attempts and re-queues the webhook towards **every active endpoint**; the payload is unchanged and the deposit status is unchanged. Errors: `404` unknown deposit, `400` deposit not confirmed, `503` no active webhook endpoints configured.

### Get balances (deposit addresses in database)

```bash
curl http://localhost:3000/api/v1/balances
```

One entry per network. Bitcoin sums the confirmed balance of every issued address; Lightning
reports the Breez/Spark wallet balance (`addresses_checked: 0`). BTC entries use
`token: "BTC"` and are totalled in `total_btc` — never in `total_usdt`. The cached
`/wallet-balances` payload has the same `total_btc` treatment.

### Pay a Lightning address (payout)

```bash
curl -X POST http://localhost:3000/api/v1/lightning/payments \
  -H "Content-Type: application/json" -H "x-api-key: $API_KEY" \
  -H "Idempotency-Key: $(uuidgen)" \
  -d '{ "ln_address": "alice@example.com", "amount_sats": "10000", "comment": "withdrawal" }'
```

Sends from the Breez/Spark wallet to a Lightning address (`user@domain` only, user part
`[a-z0-9-_.]`, public domain — no LNURL strings, invoices, on-chain addresses, IP literals or
local names). Network fees are added on top of `amount_sats`
(the recipient receives the amount) and are **not capped**. There is **no amount limit and
no destination allowlist**.

- **Who**: a merchant key granted *Can send Lightning payments* (Dashboard → API Keys, off
  by default; `403` otherwise), or an **admin** from the dashboard (Lightning page). Operators
  get `403`.
- **Idempotency**: optional `Idempotency-Key` header (UUID, `400` otherwise). Replaying the
  same key returns the original payment instead of paying twice.
- **Response**: `202` with the payment (same shape as the history entries), usually
  `status: "pending"`. Follow it with `GET /lightning/payments/{id}` and the
  `payout_succeeded` / `payout_failed` webhooks.
- **Errors**: `400` invalid input, `503` Lightning not configured, `502 "Payment failed"` for
  every payment failure (unreachable address, amount outside the recipient's bounds,
  insufficient balance, routing failure…). The details are only in the server logs.

⚠️ A key with this permission can drain the whole Lightning wallet (to any address, without
limit), and the wallet may be shared with other Breez clients using the same seed: grant it
sparingly.

### Lightning wallet history

```bash
curl "http://localhost:3000/api/v1/lightning/payments?limit=50&offset=0" -H "x-api-key: $API_KEY"
```

Every payment of the Breez/Spark wallet, newest first — received and sent, completed,
pending or failed, **including funds unrelated to gateway invoices** (anything else using
the same seed shows up too). Each entry carries `direction`, `status`, `amount` (BTC) /
`amount_sats`, `fees_sats`, `method`, the `invoice` (Lightning) or `tx_id` (on-chain
deposit/withdrawal), `created_at`, and `deposit_address_id` when it settled a gateway
invoice. The SDK reports no total: page with `limit` (1–100, default 50) / `offset` and the
returned `has_more`. `503` when Lightning is not configured. The dashboard shows it under
**Lightning**.

### Get HD wallet balances (cached, synced hourly)

```bash
curl http://localhost:3000/api/v1/wallet-balances
```

All the `curl` examples above also need `-H "x-api-key: $API_KEY"`.

## Admin API (`/api/admin/v1`)

Used exclusively by the dashboard; every request (except setup/login/refresh) carries
the acting staff member's `Authorization: Bearer <access_token>`. Requires `JWT_SECRET`.

| Method | Path | Access | Description |
|---|---|---|---|
| GET/POST | `/setup` | public | First-run check / create the initial admin (only while no staff exists) |
| POST | `/auth/login`, `/auth/refresh` | public | Email+password login; refresh-token rotation |
| POST | `/auth/logout` · GET `/auth/me` | JWT | Session management |
| GET/POST/PATCH | `/staff`, `/staff/{id}` | admin | List/create/update staff (roles `admin` / `operator`, activation, password) |
| GET/POST/PATCH/DELETE | `/api-keys`, `/api-keys/{id}` | admin | List/create/revoke merchant API keys (secret shown once; optional `expires_in_days` 30/90/180/365, omit for no expiry — expired keys get status `expired` and stop working); `can_send` (create or PATCH) grants the Lightning payout permission |
| GET/PATCH | `/settings` | admin | Read/update system settings (`deposit_expiry_minutes` 1–1440, `api_key_prefix` ≤ 5 alphanum, `api_traffic_retention_days` 1–3650 or `null` = keep forever; PATCH is partial) |
| GET/POST/PATCH/DELETE | `/webhook-endpoints`, `/webhook-endpoints/{id}` | admin | Manage outgoing webhook endpoints (subscribed `events` — `pending`/`paid`/`expired`/`payout_succeeded`/`payout_failed` — activate/deactivate; delete only when unused) |
| GET | `/webhooks?status=&limit=&offset=` | JWT | Latest delivery per event key, joined with deposit (if any) + address |
| POST | `/webhooks/retry-failed` | JWT | Bulk-requeue all failed deliveries (paid ones only once their deposit is confirmed) |
| POST | `/webhook-deliveries/{id}/retry` | JWT | Replay one delivery row (works for every event type) |
| GET | `/stats` | JWT | Counters (addresses/deposits/webhooks by status) + confirmed volume |
| GET | `/traffic/overview?surface=merchant\|admin\|all&window=15m\|1h\|24h` | JWT | API traffic aggregates: req/min, error rate, p50/p95 latency, per-bucket time series, top routes, top API keys |
| GET | `/traffic/requests?surface=&status=2xx..5xx&route=&method=&api_key_id=&limit=&offset=` | JWT | Paginated request log (method, matched route, status, duration, key/staff identity, IP — no bodies, never the raw key) |
| — | `/deposit-addresses*`, `/deposits/{id}/webhooks/retry`, `/balances`, `/wallet-balances`, `/lightning/payments*` | JWT | Same handlers as `/api/v1`, JWT-authenticated for the dashboard (`POST /lightning/payments` is admin-only) |

## Webhook events & payloads

Webhook endpoints are managed in Dashboard → Webhooks (admin). Each endpoint subscribes to one or more payment events and only receives those:

| Event | Fired when | `event_type` in payload |
|---|---|---|
| `pending` | A deposit address is created | `payment_pending` |
| `paid` | A deposit is confirmed on-chain | `deposit_confirmed` |
| `expired` | A pending address passes its expiry unpaid | `payment_expired` |
| `payout_succeeded` | An outgoing Lightning wallet payment completed — **any** bitcoin send of the wallet, not only the gateway's own (token sends are not reported) | `payout_succeeded` |
| `payout_failed` | An outgoing Lightning wallet payment failed | `payout_failed` |

Payout events are opt-in per endpoint (unchecked by default). Sends that finish while the
gateway is down are caught up at the next boot (first boot only records a starting point).
Payload:

```json
{
  "event_type": "payout_succeeded",
  "data": {
    "payment_id": "019a3c1e-…", "status": "completed", "network": "lightning", "token": "BTC",
    "amount": "0.0001", "amount_sats": "10000", "fees_sats": "3", "method": "lightning",
    "ln_address": "alice@example.com", "comment": "withdrawal", "invoice": "lnbc100u1…",
    "created_at": "2026-10-10T12:00:00+00:00"
  }
}
```

Payload on confirmed deposit (`paid`, unchanged historical shape):

```json
{
  "event_type": "deposit_confirmed",
  "data": {
    "deposit_id": "uuid",
    "address": "T...",
    "network": "tron",
    "token": "USDT",
    "amount": "25.00",
    "amount_raw": "25000000",
    "tx_hash": "abc...",
    "confirmations": 19,
    "status": "confirmed"
  }
}
```

Payload for `pending` / `expired` (no deposit yet — `deposit_id` is the deposit-address id, like above):

```json
{
  "event_type": "payment_pending",
  "data": {
    "deposit_id": "uuid",
    "address": "T...",
    "network": "tron",
    "token": "USDT",
    "expected_amount": "25",
    "status": "pending",
    "expires_at": "2026-08-19T12:00:00+00:00",
    "created_at": "2026-08-19T11:00:00+00:00"
  }
}
```

### Signature (`X-Webhook-Signature`)

Each endpoint can optionally have a **signing secret** (set at creation or rotated later in Dashboard → Webhooks; write-only — the API only ever reports `has_secret`). When set, every delivery carries:

```text
X-Webhook-Signature: sha256=<hex(HMAC-SHA256(secret, raw_body))>
```

Verify it against the **raw request body** (before any JSON parsing), using a constant-time comparison:

```js
const crypto = require("node:crypto")

function verify(rawBody, header, secret) {
  const expected = "sha256=" +
    crypto.createHmac("sha256", secret).update(rawBody).digest("hex")
  return header.length === expected.length &&
    crypto.timingSafeEqual(Buffer.from(header), Buffer.from(expected))
}
```

No secret configured → no header. The signature covers the body only (no timestamp): if replay matters to you, deduplicate on `(event_type, data.deposit_id, data.tx_hash)` on your side.

If no active webhook endpoint is configured, a warning is logged at startup and webhooks are disabled. Each delivery is tracked per (deposit, endpoint) pair for `paid` and per (address, event, endpoint) for `pending`/`expired`; `deposits[].webhooks[]` in the deposit-address detail exposes the per-endpoint state, while `deposits[].webhook` keeps the historical single-object shape (most recent delivery).

Delivery is attempted up to 5 times with increasing gaps — 90s, 4m30, 13m30, 40m30 — so the 5th and final attempt starts ~1h after the first. After that the delivery is marked `failed` and can only be resent via the retry endpoint. Receivers must treat replays as the same event (idempotent on their side).

## Deployment (Kamal)

Deploys use [Kamal 2](https://kamal-deploy.org): `config/deploy.yml` is one file for the
**test**, **staging** and **production** environments, filled from environment variables.
One image (`docker/Dockerfile`, target `all`, pushed to `ghcr.io`) runs as two roles behind
kamal-proxy, with Let's Encrypt TLS:

| Role | Host | Container | Health check |
|---|---|---|---|
| `api` | `API_HOST` | `/app/gateway`, port 3000, Breez volume `crypto-payment-gateway-breez` | `/readyz` |
| `dashboard` | `DASHBOARD_HOST` | nginx (`/app/dashboard/start.sh`), port 8080, `/api` → `https://API_HOST` | `/up` |

PostgreSQL and Redis are **external** services (`DATABASE_URL`, `REDIS_URL`). Migrations run
when the api container boots.

**GitHub Actions → Deploy** (manual, pick the environment) runs `kamal deploy`. Per GitHub
environment:

- Variables: `GATEWAY_HOST` (server, SSH target), `API_HOST`, `DASHBOARD_HOST` (required);
  `APP_ENV` (defaults to the environment name), `BITCOIN_ESPLORA_BASE_URL`, `TRUSTED_PROXIES`
  (defaults to the private docker ranges kamal-proxy lives in).
- Secrets: `SSH_PRIVATE_KEY`, `WALLET_MNEMONIC`, `DATABASE_URL`, `REDIS_URL`, `JWT_SECRET`;
  optional provider keys `TRONGRID_API_KEY`, `ALCHEMY_API_KEY`, `HELIUS_API_KEY`,
  `BREEZ_API_KEY` (empty disables the network).

A brand-new server needs `kamal setup` once (installs Docker and kamal-proxy); from a
workstation, export the same variables and secrets first (`.kamal/secrets` only references
them). Useful aliases: `kamal logs`, `kamal console`.

⚠️ `WALLET_MNEMONIC` — the custodial seed — ends up in the GitHub environment secrets and in
the env file Kamal writes on the server: restrict access to both accordingly. During a
deploy, the old and new api containers overlap for a few seconds on the Breez volume
(processing is idempotent).

## Security considerations

This service uses a **custodial hot wallet**: a single operator `WALLET_MNEMONIC` is loaded at boot and used to derive all deposit addresses (HD paths per network). Compromising the host or the environment secret means compromising the entire derivation chain and any funds sitting on those addresses.

Treat production deployments accordingly:

- Store the mnemonic in a secret manager or HSM/KMS — not in plain files on disk or in git
- Plan key rotation before you accumulate significant balances
- Never log the mnemonic; keep `.env` out of version control

[`.env.example`](.env.example) uses only the well-known BIP39 test vector (`abandon … about`). Do not reuse it with real funds.

Scope today: inbound USDT / BTC / Lightning deposit detection and webhooks. This is not a multi-tenant or non-custodial vault. See the [custody note](#custody-note) for Lightning funds.

To report a vulnerability, use [GitHub Security Advisories](https://github.com/nejos97/crypto-payment-gateway/security/advisories/new) rather than a public issue when possible.

## Environment variables

| Variable | Required | Default | Description |
|---|---|---|---|
| `WALLET_MNEMONIC` | Yes | — | BIP39 mnemonic (12 or 24 words) |
| `DATABASE_URL` | Yes | — | PostgreSQL connection string |
| `REDIS_URL` | Yes | — | Redis connection string |
| `TRONGRID_API_KEY` | No* | — | TronGrid API key |
| `ALCHEMY_API_KEY` | No* | — | Alchemy API key (Ethereum) |
| `HELIUS_API_KEY` | No* | — | Helius API key (Solana) |
| `TRONGRID_BASE_URL` | No | `https://api.trongrid.io` | TronGrid API base URL |
| `ETH_RPC_BASE_URL` | No | `https://eth-mainnet.g.alchemy.com/v2` | Ethereum RPC base (API key appended) |
| `HELIUS_RPC_BASE_URL` | No | `https://mainnet.helius-rpc.com` | Helius Solana RPC base |
| `HELIUS_API_BASE_URL` | No | `https://api.helius.xyz` | Helius REST API base |
| `APP_ENV` | No | `development` | Redis key prefix / environment tag |
| `PORT` | No | `3000` | HTTP listen port |
| `LOG_LEVEL` | No | `info` | Tracing log level |
| `JWT_SECRET` | No** | — | Secret for staff JWTs (min 32 chars); unset disables `/api/admin/v1` |
| `ACCESS_TOKEN_TTL_MINUTES` | No | `15` | Staff access-token lifetime |
| `REFRESH_TOKEN_TTL_DAYS` | No | `30` | Staff refresh-token lifetime (stored in Redis, rotated) |
| `DASHBOARD_PORT` | No | `8080` | Host port of the dashboard container (docker compose) |
| `CORS_ALLOWED_ORIGINS` | No | — | Comma-separated origins; only needed if the dashboard is not same-origin |
| `API_TRAFFIC_ENABLED` | No | `true` | Record every `/api/*` request into the `api_requests` log shown on Dashboard → Traffic (retention is a dashboard setting, 2 years by default) |
| `TRUSTED_PROXIES` | No | — | Comma-separated CIDRs of reverse proxies whose `X-Forwarded-For` is trusted for the traffic log's client IP (the **last** hop is used). Empty = trust none: the socket peer is recorded and forwarding headers are ignored. Behind the bundled dashboard nginx, set the docker network's subnet. An invalid CIDR fails startup |
| `BITCOIN_ESPLORA_BASE_URL` | No* | — | Esplora-compatible REST base (`https://mempool.space/api`, `https://blockstream.info/api`, or self-hosted). Unset disables Bitcoin |
| `BTC_CONFIRMATIONS` | No | `3` | Confirmations before a Bitcoin deposit is confirmed / paid |
| `BREEZ_API_KEY` | No* | — | Breez SDK Spark API key ([breez.technology](https://breez.technology)). Unset disables Lightning |
| `BREEZ_STORAGE_DIR` | No | `./.breez-data` | Breez SDK local storage (SQLite), resolved to an absolute path at startup. Must persist across restarts (docker compose mounts the `breez_data` volume) |

\* Required for monitoring on the respective network.
\** Required for the admin API and the dashboard.

Advanced overrides (code defaults if unset): `POLLING_INTERVAL_SECONDS` (10), `ETH_CONFIRMATIONS` (12), `TRON_CONFIRMATIONS` (19), `AMOUNT_TOLERANCE_PERCENT` (1).

Webhook endpoints (Dashboard → Webhooks) and the default payment expiry (60 min, max 24 h; Dashboard → Settings) are **not** environment variables — they are stored in the database and managed from the dashboard (admin only).

## Bitcoin & Lightning

**Bitcoin (on-chain, `bitcoin`).** Each payment gets a fresh BIP84 native-SegWit address
(`bc1q…`, path `m/84'/0'/0'/0/{index}`) derived from `WALLET_MNEMONIC`. A monitor loop polls
the Esplora API (tip height + the address's latest transactions: mempool and last 25
confirmed); the amount of a transaction is the sum of its outputs to the address. Below
`BTC_CONFIRMATIONS` (3) the deposit is `detected`; at 3 confirmations it is `confirmed`, the
payment flips to `paid` and the `paid` webhook fires. One transaction paying several gateway
addresses yields one deposit per address. No reorg rollback (3 confirmations is the
mitigation) and no RBF tracking — a replaced mempool transaction never confirms and the
payment expires normally.

**Lightning (`lightning`).** Each payment is a fixed-amount BOLT11 invoice issued through the
[Breez SDK Spark](https://sdk-doc-spark.breez.technology/) (seeded with `WALLET_MNEMONIC`);
`address` holds the payment request. A settled invoice is final: the payment flips to `paid`
immediately (`confirmations: 1`, `tx_hash` = the SDK payment id). Detection is pushed by SDK
events, with a reconciliation poll as safety net for events missed across restarts. The SDK
is disconnected cleanly on SIGINT/SIGTERM; a connection failure at boot only disables
Lightning.

Both reuse the existing `pending` / `paid` / `expired` webhooks unchanged (`network`:
`bitcoin`/`lightning`, `token`: `BTC`, `amount` in BTC, `amount_raw` in sats).

### Custody note

On-chain BTC stays on addresses derived from `WALLET_MNEMONIC` — recoverable from the
mnemonic alone, like USDT. Lightning funds live in the Breez/Spark wallet: recoverable from
the same mnemonic **through the Spark SDK/protocol**, which is a different trust model
(Spark L2). Enabling Lightning (`BREEZ_API_KEY`) must be an explicit, conscious operator
choice.

## USDT contracts

| Network | Contract |
|---|---|
| Ethereum | `0xdAC17F958D2ee523a2206206994597C13D831ec7` |
| Tron | `TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t` |
| Solana | `Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB` |
