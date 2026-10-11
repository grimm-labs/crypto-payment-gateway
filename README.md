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
While no staff account exists, it also prints a **one-time setup token** in its logs
(`No staff account yet — complete the dashboard setup with this one-time setup token: …`, or
set `SETUP_TOKEN` yourself): the dashboard's `/setup` wizard asks for it.

Outside `APP_ENV=development` the service refuses to start with the public test mnemonic of
`.env.example`, with plain-text Redis/Postgres (`rediss://` and `sslmode=verify-full` are
required unless `ALLOW_INSECURE_TRANSPORT=true`), or with `http://` provider URLs.

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
(first name, last name, email, password of 12–128 characters, and the one-time setup token
printed in the API logs). Admins can then add staff (role `admin` or `operator`), create API
keys, generate deposit addresses, and monitor/retry webhooks. Enable 2FA (Account →
Security): payouts, payout rights, payout limits and staff management require a fresh code.

### Docker

```bash
docker compose up --build
```

Starts postgres, redis, the API on `:3000` and the dashboard on `:8080`
(nginx serving the built SPA, proxying `/api` to the api service — no CORS needed). Every
port is published on `127.0.0.1` only.
Both images come from `docker/Dockerfile` (targets `api` and `dashboard`).

## Testing

Unit tests live in the `domain`, `wallet`, `config`, `chain`, `lightning-client`, `jobs`, `auth` and `http-api` crates (BIP39 / BIP84 / HD address golden vectors, amount tolerance and per-token decimals, enum roundtrips, Esplora parsing and confirmation math, the Breez SDK wrapper against a mocked `LightningOps`, webhook retry schedule, request validation). None needs a database or network access. CI runs the same suite:

```bash
pnpm nx affected -t lint test build   # what CI runs (only the affected apps)
pnpm nx run api:test                  # cargo test --workspace --locked
pnpm nx run api:lint                  # cargo clippy … --locked -- -D warnings
```

Cargo commands run with `--locked`: after editing a `Cargo.toml`, refresh `Cargo.lock`
first. CI also runs `cargo deny check` (`deny.toml`: advisories, licenses, sources — a new
ignore needs a reason); third-party actions are pinned by commit SHA, Docker base images by
digest, and Dependabot proposes updates weekly.

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

Creation returns `503 "<network> is not configured"` when the network is disabled (missing
provider key, Breez SDK not connected…) and a generic `500` when the Lightning SDK fails
(details are only logged). The invoice expiry equals the payment's `expires_at`. Every
response carries `expected_amount_raw` (base units: sats for BTC, 10⁻⁶ for USDT) next to
`expected_amount`.

Limits: `expected_amount` must be > 0 and ≤ 10¹² USDT; an explicit `expires_at` must fall
between now + 1 min and now + 1440 min (the default comes from Settings). A key may hold at
most 1 000 open (`pending`) payments and create 120 per minute (`429` beyond); every key is
limited to 600 requests per minute, and invalid keys to 30 attempts per minute per IP.

A payment is not expired the second its deadline passes: it keeps being polled for 10 more
minutes (a transfer broadcast just before the deadline is still credited), and a payment
with a `detected` deposit waiting for confirmations is held up to 24 h past its deadline.

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

Each entry in the returned `deposits[]` includes a `webhook` object with the delivery state (`status`: `pending` / `delivered` / `failed`, `attempts`, `last_response`, `updated_at`), or `null` if no delivery was attempted yet. Endpoint URLs and delivery errors are admin data: they only appear on the dashboard (`/api/admin/v1`), never on `/api/v1`.

### Retry a webhook delivery

```bash
curl -X POST http://localhost:3000/api/v1/deposits/<deposit_id>/webhooks/retry
```

`<deposit_id>` is the `id` of an entry in `deposits[]` above. A merchant key only re-queues the deliveries that ended **`failed`** (towards endpoints still active and subscribed) — a delivered webhook is never resent from `/api/v1` — at most once a minute per deposit; the answer carries `retried` (count). Errors: `404` unknown deposit, `400` deposit not confirmed, `409` nothing failed to retry, `429` too soon, `503` no active webhook endpoints configured. (Admins can replay delivered webhooks from the dashboard.)

### Get balances (deposit addresses in database)

```bash
curl http://localhost:3000/api/v1/balances
```

One entry per network. Bitcoin sums the confirmed balance of every issued address; Lightning
reports the Breez/Spark wallet balance (`addresses_checked: 0`). BTC entries use
`token: "BTC"` and are totalled in `total_btc` — never in `total_usdt`. The cached
`/wallet-balances` payload has the same `total_btc` treatment.

Querying every issued address costs one provider call each, so the result is computed in
the background and cached for 5 minutes (`updated_at`): when no fresh figure exists the call
answers `503 "Balances are being computed — retry shortly"` with `Retry-After: 15`.

### Pay a Lightning address (payout)

```bash
curl -X POST http://localhost:3000/api/v1/lightning/payments \
  -H "Content-Type: application/json" -H "x-api-key: $API_KEY" \
  -H "Idempotency-Key: $(uuidgen)" \
  -d '{ "ln_address": "alice@example.com", "amount_sats": "10000", "comment": "withdrawal" }'
```

Sends from the Breez/Spark wallet to a Lightning address (`user@domain` only, user part
`[a-z0-9-_.]`, public domain resolving to public addresses — no LNURL strings, invoices,
on-chain addresses, IP literals or local names). Network fees are added on top of
`amount_sats` (the recipient receives the amount).

- **Who**: a merchant key granted *Can send Lightning payments* (Dashboard → API Keys, off
  by default; `403` otherwise — such a key must expire), or an **admin** from the dashboard
  (Lightning page) with a **fresh TOTP code** (`totp_code` in the body; admins without 2FA
  get `403`). Operators get `403`.
- **Limits** (Dashboard → Settings, changing them needs a TOTP code): at most
  `payout_max_sats` per payout (default 1 000 000) and `payout_daily_max_sats` over a rolling
  24 h (default 10 000 000), counted over every gateway payout; `0` disables payouts.
  Beyond: `422`. Plus 10 payout/quote calls per minute per caller.
- **Idempotency**: the `Idempotency-Key` header (UUID) is **required**. It is bound to the
  caller and to the parameters: replaying it with the same parameters answers `200` with
  the original payment (nothing is sent again); with other parameters (or by another
  caller) `422`; while the first request is still sending `409`. Keep the same key when you
  retry after a network error.
- **Response**: `202` with the payment (same shape as the history entries), usually
  `status: "pending"`. Follow it with `GET /lightning/payments/{id}` and the
  `payout_succeeded` / `payout_failed` webhooks.
- **Errors**: `400` invalid input, `422` limits / key reuse, `503` Lightning not
  configured, `502 "Payment failed"` for every payment failure (unreachable address, amount
  outside the recipient's bounds, insufficient balance, routing failure…). The details are
  only in the server logs. Every payout is recorded in the audit log.

`POST /lightning/payments/quote` (same body without the key, same callers) returns what the
payout would cost without paying: `{ ln_address, amount_sats, fees_sats, total_sats }`
(`502 "Could not get a quote"` on failure).

⚠️ The Lightning wallet may be shared with other Breez clients using the same seed: grant
the payout permission sparingly.

### Lightning wallet history

```bash
curl "http://localhost:3000/api/v1/lightning/payments?limit=50&offset=0" -H "x-api-key: $API_KEY"
```

Requires a key with the payout permission (`403` otherwise; admin-only on the dashboard).
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

Used exclusively by the dashboard; every request (except setup/login/refresh/logout)
carries the acting staff member's `Authorization: Bearer <access_token>`. Requires
`JWT_SECRET`.

**Sessions.** A login opens a session whose refresh token lives in an **HttpOnly cookie**
(`cpg_refresh`, `SameSite=Strict`, `Path=/api/admin/v1/auth`, `Secure` outside
development) — never in JavaScript. `/auth/refresh` and `/auth/logout` read it and require the
`X-CSRF-Protection: 1` header. Each refresh rotates the token atomically; replaying a rotated
token (outside a 30 s grace for concurrent tabs) revokes the whole session. A session lives at
most `REFRESH_TOKEN_TTL_DAYS` from the login (never extended). Access tokens are bound to
their session: logout, password change, 2FA reset and deactivation revoke them at once.

**Step-up.** Sensitive admin actions need a fresh TOTP code in the body (`totp_code`): payouts,
creating a key with `can_send` or granting it, changing the payout limits, creating staff,
changing a member's role / activation / password, resetting their 2FA. An admin without 2FA
gets `403` on those (the rest of the dashboard works).

**Rate limits** (`429` + `Retry-After`): login 20/15 min per IP and 10/15 min per account,
setup 10/h per IP, refresh 60/min per IP, 10 wrong TOTP codes and 5 wrong current passwords
per account per 15 min.

| Method | Path | Access | Description |
|---|---|---|---|
| GET/POST | `/setup` | public | First-run check / create the initial admin (only while no staff exists; needs `setup_token`, printed in the API logs at boot or set with `SETUP_TOKEN`) |
| POST | `/auth/login`, `/auth/login/totp` | public | Email+password login (+ TOTP when enabled); sets the refresh cookie |
| POST | `/auth/refresh`, `/auth/logout` | public (cookie + CSRF header) | Rotate the session / end it (always `200`, clears the cookie) |
| POST | `/auth/logout-all` · `/auth/password` · GET `/auth/me` | JWT | Revoke every own session; change own password (`current_password`, `new_password`, `totp_code` when 2FA is on — revokes every session); profile |
| POST | `/auth/totp/setup` (`password`), `/auth/totp/enable` (`code`), `/auth/totp/disable` (`password` + `code`) | JWT | Own 2FA management |
| GET/POST/PATCH | `/staff`, `/staff/{id}`, `/staff/{id}/totp/reset` | admin | List/create/update staff (roles `admin` / `operator`, activation, password — 12 to 128 characters); the last active admin cannot be demoted |
| GET/POST/PATCH/DELETE | `/api-keys`, `/api-keys/{id}` | admin | List/create/revoke merchant API keys (secret shown once; optional `expires_in_days` 30/90/180/365, omit for no expiry — expired keys get status `expired` and stop working); `can_send` (create or PATCH) grants the Lightning payout permission — only to keys that expire |
| GET/PATCH | `/settings` | admin | Read/update system settings (`deposit_expiry_minutes` 1–1440, `api_key_prefix` ≤ 5 alphanum, `api_traffic_retention_days` 1–3650 or `null` = keep forever, `payout_max_sats` / `payout_daily_max_sats` (0 = payouts disabled); PATCH is partial) |
| GET/POST/PATCH/DELETE | `/webhook-endpoints`, `/webhook-endpoints/{id}` | admin | Manage outgoing webhook endpoints (subscribed `events` — `pending`/`paid`/`expired`/`payout_succeeded`/`payout_failed` — required signing `secret`, activate/deactivate; delete only when unused) |
| GET | `/webhooks?status=&limit=&offset=` | JWT | Latest delivery per event key, joined with deposit (if any) + address |
| POST | `/webhooks/retry-failed`, `/webhook-deliveries/{id}/retry`, `/deposits/{id}/webhooks/retry` | admin | Bulk-requeue failed deliveries (still subscribed endpoints only); replay one delivery row; replay a deposit's `paid` webhook |
| GET | `/audit-log?action=&limit=&offset=` | admin | Append-only audit trail (actor, action, target, details, IP): setup, sessions, 2FA, staff, API keys, settings, webhook endpoints, payouts, webhook replays |
| GET | `/stats` | JWT | Counters (addresses/deposits/webhooks by status) + confirmed volume |
| GET | `/traffic/overview?surface=merchant\|admin\|all&window=15m\|1h\|24h` | JWT | API traffic aggregates: req/min, error rate, p50/p95 latency, per-bucket time series, top routes, top API keys |
| GET | `/traffic/requests?surface=&status=2xx..5xx&route=&method=&api_key_id=&limit=&offset=` | JWT | Paginated request log (method, matched route, status, duration, key/staff identity, IP — no bodies, never the raw key) |
| — | `/deposit-addresses*`, `/balances`, `/wallet-balances` | JWT | Same handlers as `/api/v1`, JWT-authenticated for the dashboard |
| — | `/lightning/payments*` (history, quote, payout) | admin | Same handlers as `/api/v1`; the payout needs `totp_code` |

## Webhook events & payloads

Webhook endpoints are managed in Dashboard → Webhooks (admin). Each endpoint subscribes to one or more payment events and only receives those:

| Event | Fired when | `event_type` in payload |
|---|---|---|
| `pending` | A deposit address is created | `payment_pending` |
| `paid` | A deposit is confirmed on-chain | `deposit_confirmed` |
| `expired` | A pending address passes its expiry unpaid | `payment_expired` |
| `payout_succeeded` | An outgoing Lightning wallet payment completed — **any** bitcoin send of the wallet, not only the gateway's own (token sends are not reported) | `payout_succeeded` |
| `payout_failed` | An outgoing Lightning wallet payment failed | `payout_failed` |

Every payload also carries a top-level `event_id` (the delivery id — stable across retries
of that delivery, identical to the `X-Webhook-Id` header; deduplicate on it) and
`occurred_at` (when the event happened — deliveries are not ordered).

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

### Signature (`X-Webhook-Signature-V2`)

Every endpoint has a **signing secret** (required at creation, rotated in Dashboard →
Webhooks, never removable; write-only — the API only ever reports `has_secret`). Endpoints
created before secrets became mandatory deliver unsigned until one is set (the dashboard flags
them). Every delivery carries:

```text
X-Webhook-Id: <delivery id>                 (= payload event_id)
X-Webhook-Timestamp: <unix seconds>
X-Webhook-Signature-V2: t=<timestamp>,v1=<hex(HMAC-SHA256(secret, "<timestamp>.<raw_body>"))>
X-Webhook-Signature: sha256=<hex(HMAC-SHA256(secret, raw_body))>   (legacy, body only)
```

Verify the V2 signature against the **raw request body** (before any JSON parsing), with a
constant-time comparison, and reject timestamps older than your tolerance (e.g. 5 minutes) to
defeat replays:

```js
const crypto = require("node:crypto")

function verify(rawBody, header, secret, toleranceSecs = 300) {
  const { t, v1 } = Object.fromEntries(header.split(",").map((kv) => kv.split("=")))
  if (Math.abs(Date.now() / 1000 - Number(t)) > toleranceSecs) return false
  const expected = crypto.createHmac("sha256", secret).update(`${t}.${rawBody}`).digest("hex")
  return v1.length === expected.length &&
    crypto.timingSafeEqual(Buffer.from(v1), Buffer.from(expected))
}
```

**Endpoint rules** (outside `APP_ENV=development`): `https://` only, no `user:password@`, a
public host (private, loopback, link-local and other reserved addresses are refused — at
creation and again at every connection, after DNS resolution). Redirects are never followed:
a `3xx` answer counts as a failed attempt. Plain `http://` and local receivers are accepted in
development only.

If no active webhook endpoint is configured, a warning is logged at startup and webhooks are disabled. Each delivery is tracked per (deposit, endpoint) pair for `paid` and per (address, event, endpoint) for `pending`/`expired`; `deposits[].webhooks[]` in the deposit-address detail exposes the per-endpoint state, while `deposits[].webhook` keeps the historical single-object shape (most recent delivery).

Delivery is attempted up to 5 times with increasing gaps — 90s, 4m30, 13m30, 40m30 — so the 5th and final attempt starts ~1h after the first. After that the delivery is marked `failed` and can only be resent via the retry endpoint. Delivery is **at least once**: a worker stopped mid-delivery (deploy, crash) leaves a 60 s lease after which the delivery is retried, so receivers must treat replays as the same event (deduplicate on `event_id`). Up to 16 deliveries run concurrently.

## Deployment (Kamal)

Deploys use [Kamal 2](https://kamal-deploy.org): `config/deploy.yml` is one file for the
**test**, **staging** and **production** environments, filled from environment variables.
One image (`docker/Dockerfile`, target `all`, pushed to `ghcr.io`) runs as two roles behind
kamal-proxy, with Let's Encrypt TLS:

| Role | Host | Container | Health check |
|---|---|---|---|
| `api` | `API_HOST` | `/app/gateway`, port 3000, Breez volume `crypto-payment-gateway-breez` | `/readyz` |
| `dashboard` | `DASHBOARD_HOST` | nginx (`/app/dashboard/start.sh`), port 8080, `/api` → `https://API_HOST` | `/up` |

PostgreSQL and Redis are **external** services (`DATABASE_URL`, `REDIS_URL`) and must be
reached over verified TLS (`rediss://`, `sslmode=verify-full`) unless
`ALLOW_INSECURE_TRANSPORT=true` (private network only). Migrations run when the api container
boots. On a fresh database, read the one-time setup token with `kamal app logs -r api` (or set
the `SETUP_TOKEN` secret).

**GitHub Actions → Deploy** (manual, pick the environment) runs `kamal deploy` — only from
`main` or a `v*` tag, for a commit on `main` whose CI run succeeded (a `guard` job checks it
before any secret is exposed). Per GitHub environment:

- Variables: `GATEWAY_HOST` (server, SSH target), `API_HOST`, `DASHBOARD_HOST`,
  `SSH_KNOWN_HOSTS` (the server's pinned host key, e.g. from `ssh-keyscan -t ed25519
  <GATEWAY_HOST>` checked against the server console — required: host keys are verified)
  (required); `APP_ENV` (defaults to the environment name), `SSH_USER` (default `root`; use a
  dedicated deploy user in the `docker` group), `BITCOIN_ESPLORA_BASE_URL`,
  `TRUSTED_PROXIES` (defaults to the private docker ranges kamal-proxy lives in — a custom
  value must also list the server's public IP, which the dashboard's nginx reaches the API
  through), `ALLOW_INSECURE_TRANSPORT`.
- Secrets: `SSH_PRIVATE_KEY`, `WALLET_MNEMONIC`, `DATABASE_URL`, `REDIS_URL`, `JWT_SECRET`;
  optional `SETUP_TOKEN` and provider keys `TRONGRID_API_KEY`, `ALCHEMY_API_KEY`,
  `HELIUS_API_KEY`, `BREEZ_API_KEY` (empty disables the network).

Settings to apply by hand on GitHub: restrict each environment's deployment branches/tags to
`main` and `v*` and require reviewers on production (the workflow guards cannot stop a
modified workflow on another branch); a ruleset on `main` (PR + required checks, bypass for
GitHub Actions so releases can push) and on `v*` tags; "require actions pinned to a full-length
SHA"; read-only default `GITHUB_TOKEN`; Dependabot alerts.

Both roles run with `no-new-privileges` and every Linux capability dropped; the dashboard is
read-only (tmpfs `/tmp`). nginx verifies the API certificate (`proxy_ssl_verify`) and sends
strict security headers (CSP, HSTS, `X-Frame-Options`, …, `docker/dashboard/security-headers.conf`
— any location adding a header must include it; a new external origin or inline script needs
a CSP change). Local Kamal must be ≥ 2.12.

A brand-new server needs `kamal setup` once (installs Docker and kamal-proxy); from a
workstation, export the same variables and secrets first (`.kamal/secrets` only references
them). Useful aliases: `kamal logs`, `kamal console`.

⚠️ `WALLET_MNEMONIC` — the custodial seed — ends up in the GitHub environment secrets and in
the env file Kamal writes on the server: restrict access to both accordingly (after moving
from `root` to a deploy user, delete `/root/.kamal/apps`, which still holds old env files). During a
deploy, the old and new api containers overlap for a few seconds on the Breez volume
(processing is idempotent).

## Security considerations

This service uses a **custodial hot wallet**: a single operator `WALLET_MNEMONIC` is loaded at boot and used to derive all deposit addresses (HD paths per network). Compromising the host or the environment secret means compromising the entire derivation chain and any funds sitting on those addresses.

Treat production deployments accordingly:

- Store the mnemonic in a secret manager or HSM/KMS — not in plain files on disk or in git
- Plan key rotation before you accumulate significant balances
- Never log the mnemonic; keep `.env` out of version control
- Enable 2FA for every admin: payouts, payout rights, payout limits and staff management
  require a fresh TOTP code
- Keep the payout limits (Settings) as low as your use allows, and review the audit log

[`.env.example`](.env.example) uses only the well-known BIP39 test vector (`abandon … about`): the service refuses it outside `APP_ENV=development`, and never connects the (mainnet) Lightning wallet with it.

Hardening in place: TLS required to Redis/Postgres outside development, provider endpoints
checked against mainnet at boot (a network whose provider answers for another chain stays
disabled), session cookies + CSRF header, step-up TOTP, rate limits, SSRF-safe webhook
client, generic error messages (details only in the logs), security headers, an
append-only audit log. Known limits: no per-merchant data isolation (every key sees every
payment), no reorg handling beyond confirmation thresholds, secrets (TOTP, webhook signing)
stored in clear in the database, a single seed for on-chain and Lightning funds.

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
| `JWT_SECRET` | No** | — | Secret for staff JWTs (min 32 chars — shorter fails startup); unset disables `/api/admin/v1` |
| `ACCESS_TOKEN_TTL_MINUTES` | No | `15` | Staff access-token lifetime (1–60) |
| `REFRESH_TOKEN_TTL_DAYS` | No | `30` | Absolute staff session lifetime from the login (1–90; refresh token stored hashed in Redis, rotated) |
| `SETUP_TOKEN` | No | random, logged | One-time token for the first-admin `/setup` (≥ 16 chars). Unset: generated at boot and printed in the logs while no staff exists |
| `ALLOW_INSECURE_TRANSPORT` | No | `false` | Outside development, start even if `REDIS_URL` is not `rediss://` or `DATABASE_URL` lacks `sslmode=verify-full` (loud warning). Only for a private network |
| `DASHBOARD_PORT` | No | `8080` | Host port of the dashboard container (docker compose) |
| `CORS_ALLOWED_ORIGINS` | No | — | Comma-separated origins (`scheme://host[:port]`, never `*` — an invalid entry fails startup); only needed if the dashboard is not same-origin |
| `API_TRAFFIC_ENABLED` | No | `true` | Record every `/api/*` request into the `api_requests` log shown on Dashboard → Traffic (retention is a dashboard setting, 90 days by default) |
| `TRUSTED_PROXIES` | No | — | Comma-separated CIDRs of reverse proxies whose `X-Forwarded-For` is trusted for the client IP (traffic log, rate limits, audit log). The header is walked from the right and the first hop **not** in this list is the client, so list every proxy of the chain (e.g. the docker network and, when the dashboard reaches the API through its public host, the server's own public IP). Empty = trust none: the socket peer is used and forwarding headers are ignored. An invalid CIDR fails startup |
| `BITCOIN_ESPLORA_BASE_URL` | No* | — | Esplora-compatible REST base (`https://mempool.space/api`, `https://blockstream.info/api`, or self-hosted). Unset disables Bitcoin |
| `BTC_CONFIRMATIONS` | No | `3` | Confirmations before a Bitcoin deposit is confirmed / paid (≥ 1; a warning below 3) |
| `BREEZ_API_KEY` | No* | — | Breez SDK Spark API key ([breez.technology](https://breez.technology)). Unset disables Lightning |
| `BREEZ_STORAGE_DIR` | No | `./.breez-data` | Breez SDK local storage (SQLite), resolved to an absolute path at startup. Must persist across restarts (docker compose mounts the `breez_data` volume) |

\* Required for monitoring on the respective network.
\** Required for the admin API and the dashboard.

Advanced overrides (code defaults if unset): `POLLING_INTERVAL_SECONDS` (10), `ETH_CONFIRMATIONS` (12, minimum 12), `TRON_CONFIRMATIONS` (19, minimum 19), `AMOUNT_TOLERANCE_PERCENT` (1, between 0 and 5). An unparsable or out-of-range value fails startup instead of falling back to the default. Provider base URLs must use `https://` outside development.

Webhook endpoints (Dashboard → Webhooks) and the default payment expiry (60 min, max 24 h; Dashboard → Settings) are **not** environment variables — they are stored in the database and managed from the dashboard (admin only).

## Bitcoin & Lightning

**Bitcoin (on-chain, `bitcoin`).** Each payment gets a fresh BIP84 native-SegWit address
(`bc1q…`, path `m/84'/0'/0'/0/{index}`) derived from `WALLET_MNEMONIC`. A monitor loop polls
the Esplora API (tip height + the address's latest transactions: mempool and last 25
confirmed); the amount of a transaction is the sum of its outputs to the address. A
`detected` deposit is not final (a replaced mempool transaction never confirms). Below
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
