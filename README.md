# Crypto Payment Gateway

[![CI](https://github.com/nejos97/crypto-payment-gateway/actions/workflows/ci.yml/badge.svg)](https://github.com/nejos97/crypto-payment-gateway/actions/workflows/ci.yml)

Rust service for accepting USDT deposits on **Tron**, **Ethereum**, and **Solana**, native
**ETH** on Ethereum, and bitcoin payments **on-chain** and over **Lightning** — and for
sending funds back out (**withdrawals**) on every one of them. One instance serves several
**projects** (the merchant's applications), each with its own API keys, payments, webhooks
and a per-asset balance kept as a ledger over the pooled treasuries.

## Features

- Create unique deposit addresses per payment (`POST /api/v1/deposit-addresses`)
- Monitor blockchain transfers and confirm deposits
- **Projects**: several applications on one instance. Every API key, payment, webhook
  endpoint and address-book entry belongs to a project (endpoints, entries and admin
  withdrawals may belong to the **merchant funds** instead); a key only sees its project, a
  project balance is derived per asset from its deposits, sweeps, withdrawals and transfers,
  and a key cannot withdraw beyond it. Admins move funds between projects with **internal
  transfers** and reconcile the balances against the holdings — see [Projects](#projects)
- Send webhooks for payment lifecycle events — `pending` (address created), `paid`
  (deposit confirmed), `expired` — to every active endpoint **subscribed to that event** and
  **owned by the payment's project** (alerts and Lightning payouts go to the merchant's
  endpoints); endpoints are managed from the dashboard (**Webhooks**, admin), with
  per-endpoint delivery state
- HD wallet derivation from `WALLET_MNEMONIC`
- Multi-network support: Tron (TRC-20), Ethereum (ERC-20 and native ETH), Solana (SPL),
  Bitcoin (BIP84 on-chain) and Lightning (BOLT11 via the Breez SDK) — see
  [Bitcoin & Lightning](#bitcoin--lightning)
- **Withdrawals** of every asset the gateway holds (USDT, ETH, BTC on-chain and over
  Lightning, and the TRX/SOL gas reserves), from one **treasury** per network fed by
  automatic **sweeps** — with limits, an address book, approval by a second admin, step-up
  TOTP, idempotency, an audit trail and an emergency **outflow freeze** — see
  [Withdrawals](#withdrawals) and the [operator runbook](#treasury-sweeps--withdrawals-operator-runbook)
- **Admin dashboard** (`apps/dashboard`, React + shadcn/ui): projects and their balances,
  wallet balances, payments, withdrawals and approvals, internal transfers, treasury, sweeps
  and the ledger, address book, webhook monitoring/retry, API key and staff management —
  served on its own port
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
While no staff account exists, the dashboard's `/setup` wizard is **open to anyone** who
reaches it: create the first admin right after the first start. The first sign-in then
blocks on **Create your first project** (admins only; operators see a waiting screen) —
on an instance upgraded with existing keys and payments the step offers to attach them to
that project, see [Projects](#projects).

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
(first name, last name, email, password of 12–128 characters), then the first project.
Admins can then add staff (role `admin` or `operator`), create projects and API keys,
generate deposit addresses, and monitor/retry webhooks. Enable 2FA (Account → Security):
withdrawals, internal transfers, withdrawal grants, limits, the address book and staff
management require a fresh code.

### Docker

```bash
docker compose up --build
```

Starts postgres, redis, the API on `:3000` and the dashboard on `:8080`
(nginx serving the built SPA, proxying `/api` to the api service — no CORS needed). Every
port is published on `127.0.0.1` only.
Both images come from `docker/Dockerfile` (targets `api` and `dashboard`).

## Testing

Unit tests live in the `domain`, `wallet`, `config`, `chain`, `lightning-client`, `jobs`, `auth` and `http-api` crates (BIP39 / BIP84 / HD address golden vectors, amount tolerance and per-token decimals, enum roundtrips, project validators, the webhook owner rule and the legacy-endpoint split, project-balance arithmetic, Esplora parsing and confirmation math, the Breez SDK wrapper against a mocked `LightningOps`, webhook retry schedule, request validation, and the router's surfaces through `tower::ServiceExt::oneshot` against a stub Redis). None needs a database or network access. CI runs the same suite:

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

## Projects

One gateway instance serves several **projects** — the applications of a single merchant
(the operator). The design is a **ledger over pooled custody**
([ADR 0004](docs/adr/0004-projects-ledger-over-pooled-custody.md),
[spec](docs/projects-spec.md)): funds still sit on the shared treasuries and deposit
addresses of one seed, and each project's balance is recomputed from source rows, never
stored. Projects are not tenants and not separate wallets.

- **Owners.** An API key, a payment (deposit address) and the withdrawals a key makes belong
  to **one project, forever**. A webhook endpoint, an address-book entry and an admin
  withdrawal belong to a project or to the **merchant funds** (`project_id: "merchant"`),
  chosen explicitly at creation. Treasuries, sweeps, gas reserves, the freeze, asset
  settings, alerts, staff and the audit log stay instance-level.
- **Project balance**, per (network, token): confirmed deposits + the unattributed funds
  swept from its addresses (on Bitcoin, never swept, found by an address scan) − withdrawn
  amounts − their network fees when paid in the withdrawn asset (BTC, ETH) + transfers in −
  transfers out. `available` also subtracts what its open withdrawals reserve. A **key
  withdrawal** must fit in `available` — amount plus the fee cap when the fee is paid in the
  same asset (`422 project_insufficient_funds`, re-checked at approval); an admin's may
  exceed it.
- **Merchant funds** = what the gateway holds of an asset (treasury + unswept deposit
  addresses) − the sum of the project balances: gas reserves, sweep fees and whatever no
  project is charged for. TRX and SOL are always merchant funds.
- **Internal transfers** move an amount between two owners of the same (network, token)
  (project ↔ project, project ↔ merchant funds) without touching the chain: admin + TOTP,
  idempotent, append-only, allowed during the outflow freeze; a mistake is undone with the
  reverse transfer. Both sides are notified with `transfer_completed`.
- **Reconciliation.** Dashboard → Treasury → **Ledger** compares the holdings with the
  project balances per asset (`unknown` for a disabled network, a provider error or a pair
  whose sweeps are off). A shortfall — project balances above the holdings — on three
  consecutive 5-minute checks raises the `ledger_shortfall` alert.
- **Visibility.** A key sees only its project (another project's object answers `404`, like
  a missing one) and never a custody figure. Staff see every project; the header selector
  only filters the view, and instance-level pages show "Instance" instead.
- **Key tag.** A project may carry its own key tag (`[{tag}_]…`), falling back to the global
  one of Settings; it only affects keys created afterwards.

**First run.** After `/setup`, the dashboard blocks on **Create your first project** (admin
only; operators see a waiting screen). On an instance upgraded with existing data the step
offers to **attach** that data to the new project (checked by default): keys, payments, key
withdrawals and address-book entries move to it, and each webhook endpoint goes to the owner
its events imply — one subscribed to both project events (payments, withdrawals, transfers)
and merchant events (alerts, payouts) is split into two endpoints with the same URL and
secret, withdrawals and transfers kept on both. Unchecked, the legacy keys are **revoked** (a
key must belong to a project), payments and key withdrawals count in the merchant funds, and
endpoints and entries become the merchant's. Until the first project exists the instance
behaves as before projects: legacy keys work unscoped, webhooks are not owner-filtered, and
every route that creates an owned object answers `409 "Create a project first"`.

## API

> **Breaking change:** every `/api/v1` endpoint now requires an API key sent as
> `x-api-key: ...`. Create one from the dashboard (**API Keys → New API key**)
> before updating your integrations. `/healthz`, `/readyz` and `/api/admin/v1`
> are not affected. Keys are `[{tag}_]{8}_{40}` — the optional tag (max 5
> alphanumeric chars) is configurable in **Settings → API key prefix**; changing
> it only affects newly created keys, existing keys keep working.

> **Breaking change (projects):** `/api/v1` is **scoped to the key's project**. Lists and
> lookups only return that project's payments, withdrawals, address-book entries and
> transfers (another project's object is a `404`), responses carry `project_id`, and the
> custody routes left the merchant surface: `GET /balances`, `GET /wallet-balances` and the
> Lightning wallet history `GET /lightning/payments` answer `404` (they remain on the admin
> API). New: `GET /project` (the key's project and balances) and the read-only
> `GET /transfers`.

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
so `1500` can never be ambiguous between sats and BTC. The token must be one the
network receives (`USDT` on tron/ethereum/solana, native `ETH` on ethereum, `BTC` on
bitcoin/lightning) — otherwise `400 "Token not supported on this network"`. TRX and SOL are
gas assets, never accepted as payments.

```bash
# Native ETH: a decimal amount, up to 18 decimals (> 0, ≤ 100 000 ETH)
curl -X POST http://localhost:3000/api/v1/deposit-addresses \
  -H "Content-Type: application/json" -H "x-api-key: $API_KEY" \
  -d '{ "network": "ethereum", "token": "ETH", "expected_amount": "0.05" }'
```

ETH payments are detected with Alchemy's `alchemy_getAssetTransfers` (external **and**
internal transfers — a contract forwarding ETH counts; the transfers of one transaction to
the address are summed) and confirmed at `ETH_CONFIRMATIONS` (12) like USDT. The amount comes
from the exact `rawContract.value` (wei), never from a float. Webhooks keep their shape:
`token: "ETH"`, `amount` rendered with 18 decimals, `amount_raw` in wei.

```bash
# Lightning: returns a BOLT11 invoice in `address` (optional `description`, ≤ 128 chars)
curl -X POST http://localhost:3000/api/v1/deposit-addresses \
  -H "Content-Type: application/json" -H "x-api-key: $API_KEY" \
  -d '{ "network": "lightning", "token": "BTC", "expected_amount_sats": "150000", "description": "order #123" }'
```

Creation returns `503 "<network> is not configured"` when the network is disabled (missing
provider key, Breez SDK not connected…) and a generic `500` when the Lightning SDK fails
(details are only logged). The invoice expiry equals the payment's `expires_at`. Every
response carries `expected_amount_raw` (base units: sats for BTC, 10⁻⁶ for USDT, wei for
ETH) next to `expected_amount`, and `project_id`: the payment belongs to the key's project.

Limits: `expected_amount` must be > 0 and ≤ 10¹² USDT (10⁵ ETH); an explicit `expires_at` must fall
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

The project's payments only (every key of the project sees them), each with `project_id`.
Query params (optional):

- `status`: `pending`, `paid`, `expired`
- `network`: `tron`, `solana`, `ethereum`, `bitcoin`, `lightning`
- `limit`: default `50`, max `100`
- `offset`: default `0`

### Get deposit address status

```bash
curl http://localhost:3000/api/v1/deposit-addresses/<id>
```

`404` for another project's payment. Each entry in the returned `deposits[]` includes a `webhook` object with the delivery state (`status`: `pending` / `delivered` / `failed`, `attempts`, `last_response`, `updated_at`), or `null` if no delivery was attempted yet. Endpoint URLs and delivery errors are admin data: they only appear on the dashboard (`/api/admin/v1`), never on `/api/v1`.

### Retry a webhook delivery

```bash
curl -X POST http://localhost:3000/api/v1/deposits/<deposit_id>/webhooks/retry
```

`<deposit_id>` is the `id` of an entry in `deposits[]` above. A merchant key only re-queues the deliveries that ended **`failed`** (towards endpoints still active and subscribed) — a delivered webhook is never resent from `/api/v1` — at most once a minute per deposit; the answer carries `retried` (count). Errors: `404` unknown deposit (another project's included), `400` deposit not confirmed, `409` nothing failed to retry, `429` too soon, `503` no active webhook endpoints configured. (Admins can replay delivered webhooks from the dashboard.)

### Your project

```bash
curl http://localhost:3000/api/v1/project -H "x-api-key: $API_KEY"
```

`{ id, name, color, key_tag, balances: [...] }` — one entry per payment (network, token) the
project touched, each with `deposits`, `unattributed`, `withdrawn`, `fees`, `transfers_in`,
`transfers_out`, `balance`, `reserved`, `available`, `pending` (decimal strings, signed, with
`_raw` variants in base units). Nothing about the treasuries, the merchant funds or other
projects is in here. A legacy key that belongs to no project answers `409 "This API key
belongs to no project"`.

### Internal transfers (read-only)

```bash
curl "http://localhost:3000/api/v1/transfers?network=bitcoin&token=BTC&limit=50" -H "x-api-key: $API_KEY"
```

The transfers where the project is `from` or `to` (`GET /transfers/{id}` for one, `404` for
another project's), each with `transfer_id`, `network`, `token`, `amount` / `amount_raw`,
`from` and `to` (`{ "type": "project" | "merchant", "id": <uuid> | null }`), `note`,
`created_at`. Admins create them from the dashboard; the project is notified with
`transfer_completed`.

### Withdrawals

```bash
curl -X POST http://localhost:3000/api/v1/withdrawals \
  -H "Content-Type: application/json" -H "x-api-key: $API_KEY" \
  -H "Idempotency-Key: $(uuidgen)" \
  -d '{ "network": "tron", "token": "USDT", "amount": "250.00",
        "destination": "TJDENsfBJs4RFETt1X1W8wMDc8M5XnJhCe", "reference": "payout-42" }'
```

One API for every outgoing transfer: USDT (tron, ethereum, solana), ETH (ethereum), BTC
on-chain (bitcoin) and over Lightning (lightning). The body takes `network`, `token`, an
`amount` (decimal string, at most the token's decimals) — or `amount_sats` on
bitcoin/lightning — and **exactly one** of `destination` / `address_book_entry_id`; optional
`reference` (≤ 128 chars), `comment` (Lightning-address comment, ≤ 255 chars) and `metadata`
(object, ≤ 4 KB). Network fees come on top, paid by the gateway in the network's gas asset
(BTC on bitcoin/lightning): the recipient receives `amount`.

| Network | Destination |
|---|---|
| ethereum | a 20-byte address (lowercase or valid EIP-55); never the zero address or the USDT contract |
| tron | a `T…` base58check address |
| solana | a wallet (its USDT token account is created if missing; never a program-derived address) or an existing USDT token account |
| bitcoin | any mainnet address type (P2WPKH, P2WSH, P2TR, P2SH, P2PKH), amount above its dust limit |
| lightning | a Lightning address `user@domain` (public domain) or a BOLT11 invoice for exactly the amount, not expired (never one of the gateway's own) |

Gateway-owned addresses (deposit addresses, treasuries, their token accounts) are refused.

- **Who**: a merchant key whose *withdrawal policy* allows the token (and network) — set by
  an admin in Dashboard → API Keys, for keys that expire — or an **admin** from the
  dashboard with a fresh TOTP code. Operators never withdraw; TRX and SOL (gas reserves) are
  admin-only. A key without a policy gets `403`.
- **Owner**: a key's withdrawal belongs to the key's project and must fit in the project's
  available balance — `amount` plus the fee cap when the fee is paid in the withdrawn asset
  (`422 project_insufficient_funds`, checked inside the admission transaction and again at
  approval). An admin names the owner (`project_id`: a UUID or `"merchant"`, no default) and
  may exceed a project's balance; TRX and SOL withdrawals are always merchant funds.
- **Destination policy**: an `open` key may send anywhere within its own per-token limits; a
  `book` key only to active address-book entries of its project (`GET /api/v1/address-book`). At or above
  the asset's **approval threshold** any withdrawal waits for a second admin
  (`pending_approval`, 72 h); an admin's must then go to an active address-book entry
  (below it, anywhere).
- **Limits**: per withdrawal and over a rolling 24 h, per asset (all networks together) and
  per key; `0` disables. Funds: the treasury's confirmed balance minus what other
  withdrawals reserve (and, for a gas asset, its reserve floor). Refusals are `422` with a
  code first in the message: `limit_exceeded`, `key_limit_exceeded`,
  `withdrawals_disabled`, `insufficient_funds`, `approval_threshold_requires_address_book`,
  `destination_requires_address_book`, …
- **Idempotency**: the `Idempotency-Key` header (UUID) is required and bound to the caller
  and the parameters: a replay answers `200` with the stored withdrawal (nothing is sent
  again), other parameters `422`.
- **Response**: `202` with the withdrawal (`status` `queued` or `pending_approval`,
  `project_id`). It then moves through `signed` → `broadcast` → `confirmed` (or `failed`, `cancelled`, `stuck`):
  follow it with `GET /withdrawals/{id}` and the `withdrawal_*` webhooks. `503 "Outflows are
  frozen"` while the outflow freeze holds, `503` when the network is not configured, `502`
  when a provider cannot be reached (details only in the logs).

| Method | Path | |
|---|---|---|
| POST | `/withdrawals` | Create (above) |
| POST | `/withdrawals/quote` | Same body, no `Idempotency-Key`: `fee_estimate` / `fee_token`, `approval_required`, `destination_acceptable` for this key, `project_available` and `project_check_ok` — nothing is reserved. `available` / `fee_available` (the instance's custody) are `null` for a key: staff only |
| GET | `/withdrawals?status=&network=&token=&limit=&offset=` | The project's withdrawals (every key of the project, admin withdrawals attributed to it included) |
| GET | `/withdrawals/{id}` | `404` for another project's |
| POST | `/withdrawals/{id}/cancel` | A withdrawal of the project, while `pending_approval` / `queued` |
| GET | `/address-book` | Active entries of the project (`id`, `network`, `destination`, `label`) |

Creation and quotes share a limit of 10 calls per minute per caller.

**Deprecated: `POST /lightning/payments`** (and `/lightning/payments/quote`) keeps working for
one release: it creates a Lightning withdrawal of the key's project to `ln_address`
(`amount_sats`, `comment`) under the same rules and answers the former shape plus `withdrawal_id`, with
`Deprecation: true` and a `Sunset` header. Keys that could send payouts were migrated to an
`open` policy for BTC on Lightning only, with the former payout limits as their key limits. An
`Idempotency-Key` used before the upgrade is answered from the wallet, never paid again.
Sends of the Lightning wallet the gateway did not make still fire `payout_succeeded` /
`payout_failed`; its own fire `withdrawal_*`.

### Removed from the merchant surface

`GET /balances`, `GET /wallet-balances` and the Lightning wallet history
`GET /lightning/payments` are custody figures of the whole instance: they answer `404` on
`/api/v1` and are served on the admin API only (see [Balances and the Lightning wallet
history](#balances-and-the-lightning-wallet-history-staff-only)).

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

**Step-up.** Sensitive admin actions need a fresh TOTP code in the body (`totp_code`):
creating, approving, rejecting, cancelling, bumping or abandoning a withdrawal; creating an
internal transfer; granting a key a withdrawal policy; the asset settings (limits, thresholds, caps, sweeps); address-book
changes; lifting the outflow freeze; running sweeps; creating staff, changing a member's role
/ activation / password, resetting their 2FA. An admin without 2FA gets `403` on those (the
rest of the dashboard works).

**Rate limits** (`429` + `Retry-After`): login 20/15 min per IP and 10/15 min per account,
setup 10/h per IP, refresh 60/min per IP, internal transfers 10/min per staff member, 10
wrong TOTP codes and 5 wrong current passwords per account per 15 min.

**Owners.** Routes that create an owned object take `project_id`: a project UUID for API
keys and payments; a UUID or `"merchant"` for webhook endpoints, address-book entries and
admin withdrawals. List routes filter with `?project_id=<uuid>` (and `?project_id=merchant`
where the merchant can own rows: withdrawals, endpoints, deliveries, address book), and
responses carry `project_id` (UUID or `null`). Before the first project exists every such
creation answers `409 "Create a project first"`.

| Method | Path | Access | Description |
|---|---|---|---|
| GET/POST | `/setup` | public | First-run check / create the initial admin (only while no staff exists; open to anyone until then) |
| POST | `/auth/login`, `/auth/login/totp` | public | Email+password login (+ TOTP when enabled); sets the refresh cookie |
| POST | `/auth/refresh`, `/auth/logout` | public (cookie + CSRF header) | Rotate the session / end it (always `200`, clears the cookie) |
| POST | `/auth/logout-all` · `/auth/password` · GET `/auth/me` | JWT | Revoke every own session; change own password (`current_password`, `new_password`, `totp_code` when 2FA is on — revokes every session); profile |
| POST | `/auth/totp/setup` (`password`), `/auth/totp/enable` (`code`), `/auth/totp/disable` (`password` + `code`) | JWT | Own 2FA management |
| GET/POST/PATCH | `/staff`, `/staff/{id}`, `/staff/{id}/totp/reset` | admin | List/create/update staff (roles `admin` / `operator`, activation, password — 12 to 128 characters); the last active admin cannot be demoted |
| GET · POST/PATCH/DELETE | `/projects` · `/projects`, `/projects/{id}` | JWT · admin | List (`{ projects: [{ id, name, color, key_tag, counts: { api_keys, deposit_addresses, withdrawals, webhook_endpoints, address_book_entries } }], unassigned }`, `unassigned` = the legacy rows while no project exists) · create (`name` 1–60, optional `color` from the 8-colour palette, `key_tag` ≤ 5 alphanumerics, `attach_existing` on the first project only — see [Projects](#projects)), rename / recolour / retag (`key_tag: null` clears it), delete (`409` while anything references it, or for the last project). No TOTP, audited |
| GET | `/projects/{id}/balance` | JWT | The project's balances, same shape as `GET /api/v1/project` |
| GET | `/ledger` | JWT | Per payment (network, token): `holdings`, `projects: [{ project_id, balance, available }]`, `project_total`, `merchant_funds`, `status` (`ok` / `shortfall` / `unknown`), each amount with `_raw`; cached 30 s |
| GET · POST | `/transfers`, `/transfers/{id}` · `/transfers` | JWT · admin | List (`project_id` UUID or `merchant`, matched on either side; `network`, `token`) and detail · create: `from_project_id`, `to_project_id` (UUID or `merchant`), `network`, `token`, `amount` or `amount_sats`, `note` (1–200), `totp_code`, header `Idempotency-Key` (a replay by the same staff member with the same parameters answers the stored transfer, other reuse `422`). `422 project_insufficient_funds` / `merchant_insufficient_funds`, `503 "Holdings unavailable, retry later"` while the pair's holdings are unknown; allowed during the freeze |
| GET/POST/PATCH/DELETE | `/api-keys`, `/api-keys/{id}` | admin | List (`?project_id`) / create (`project_id` required — the key's tag comes from the project, else from Settings) / revoke merchant API keys (secret shown once; optional `expires_in_days` 30/90/180/365, omit for no expiry — expired keys get status `expired` and stop working); PATCH sets the withdrawal grant: `withdrawal_policy` (`none` / `book` / `open`), `withdrawal_tokens` (`USDT` / `BTC` / `ETH`), `withdrawal_networks` (`null` = all of them), `withdrawal_limits` (per token, required for `open`) — only on keys that expire, TOTP unless `none` |
| GET/PATCH | `/settings` | admin | Read/update system settings (`deposit_expiry_minutes` 1–1440, `api_key_prefix` ≤ 5 alphanum, `api_traffic_retention_days` 1–3650 or `null` = keep forever; PATCH is partial) |
| GET · PATCH | `/settings/assets` | JWT · admin | Per-asset limits (per withdrawal, rolling 24 h), approval threshold, hot-wallet cap, gas reserve floor and alert threshold, and per-(network, token) sweep minimums and switches — decimal token amounts, `0` disables |
| GET/POST/PATCH/DELETE | `/webhook-endpoints`, `/webhook-endpoints/{id}` | admin | Manage outgoing webhook endpoints: owner `project_id` (UUID or `merchant`, fixed at creation — PATCH refuses it), subscribed `events` validated per owner — see [Webhook events](#webhook-events--payloads) — required signing `secret`, activate/deactivate; delete only when unused; list filter `?project_id` |
| GET | `/webhooks?status=&project_id=&limit=&offset=` | JWT | Latest delivery per event key, joined with deposit (if any) + address; `project_id` (through the endpoint's owner) and `transfer_id` on each row |
| POST | `/webhooks/retry-failed`, `/webhook-deliveries/{id}/retry`, `/deposits/{id}/webhooks/retry` | admin | Bulk-requeue failed deliveries (still subscribed endpoints only); replay one delivery row; replay a deposit's `paid` webhook |
| GET | `/audit-log?action=&limit=&offset=` | admin | Append-only audit trail (actor, action, target, details, IP): setup, sessions, 2FA, staff, projects, API keys and their withdrawal grants, settings, webhook endpoints, internal transfers, withdrawals (requested → approved / rejected / cancelled → signed → broadcast → confirmed / failed / stuck, bumps, abandons), address book, sweeps and gas top-ups, outflow freeze, webhook replays |
| GET · POST | `/withdrawals`, `/withdrawals/{id}` · `/withdrawals`, `/withdrawals/quote` | JWT · admin | List (`status`, `network`, `token`, `api_key_id`, `project_id` UUID or `merchant`) and detail with every signed transaction (fee bumps included) · create (`totp_code`, `project_id` UUID or `merchant` — gas assets merchant only) and quote — the merchant body, gas assets included |
| POST | `/withdrawals/{id}/approve`, `/reject`, `/cancel`, `/bump`, `/abandon` | admin | Approve as an admin other than the requester (a single active admin may approve their own 24 h after creation); cancel while `pending_approval` / `queued`; on a `stuck` one, bump (doubles its fee cap) or abandon (only once the network proves none of its transactions can land) |
| GET · POST/DELETE | `/address-book` · `/address-book`, `/address-book/{id}` | JWT · admin | List (`?project_id`) · add (`project_id` UUID or `merchant`, `network`, `destination`, `label`, `is_cold_storage`; usable 24 h later; a destination is allowed once per owner) / remove |
| GET | `/treasury` | JWT | Per (network, token): treasury address, confirmed, reserved, available, unswept, unattributed, gas reserve, and the hot-wallet cap status — per asset: `asset_holdings` sums the asset's treasuries over its networks and `cap_exceeded` compares that sum to the cap, on every row of the asset — plus `outflows_frozen` |
| GET · POST | `/sweeps` · `/sweeps/run` | JWT · admin | Sweep history · plan sweeps now (optional `network`) |
| GET · POST · POST | `/outflows` · `/outflows/freeze` · `/outflows/unfreeze` | JWT · JWT · admin | Freeze state (`frozen`, `forced_by_environment`, and `chain_mode` — the dashboard picks its block explorers from it) · freeze every outflow (any staff, no TOTP) · lift it (TOTP; impossible while `OUTFLOWS_FROZEN=true`) |
| GET | `/alerts` | JWT | Gas reserve low, treasury cap exceeded, address-book entry added, withdrawal stuck, ledger shortfall |
| GET | `/payout-requests` | admin | Former Lightning payout requests (read-only history) |
| GET | `/stats?project_id=` · `/stats/deposits-timeseries?days=&project_id=` | JWT | Counters (addresses/deposits/webhooks by status) + confirmed volume · daily series |
| GET | `/traffic/overview?surface=merchant\|admin\|all&window=15m\|1h\|24h&project_id=` | JWT | API traffic aggregates: req/min, error rate, p50/p95 latency, per-bucket time series, top routes, top API keys (`project_id` through the key's project; staff requests have none) |
| GET | `/traffic/requests?surface=&status=2xx..5xx&route=&method=&api_key_id=&project_id=&limit=&offset=` | JWT | Paginated request log (method, matched route, status, duration, key/staff identity, IP — no bodies, never the raw key) |
| — | `/deposit-addresses*` | JWT | Same handlers as `/api/v1`, unscoped: creation takes `project_id` (required), the list `?project_id` |
| GET | `/balances`, `/wallet-balances` | JWT | The instance's holdings (below) — staff only since projects |
| — | `/lightning/payments*` (history, deprecated payout alias and quote) | admin | The wallet history (below, staff only since projects) and the alias, which needs `totp_code` |

### Balances and the Lightning wallet history (staff only)

`GET /balances`: one entry **per (network, token)** — Ethereum appears twice (`USDT`, `ETH`),
and the gas reserves show up as `TRX` (tron) and `SOL` (solana). Each entry is the
**treasury plus the unswept deposit addresses**, broken out in `treasury_amount` /
`deposits_amount` (raw variants alongside). Lightning reports the Breez/Spark wallet balance
(`addresses_checked: 0`). Totals never mix assets: `total_usdt`, `total_btc`, `total_eth`,
and `total_trx` / `total_sol` for the gas reserves. The cached `/wallet-balances` payload
(every derived index, synced hourly) has the same shape. Querying every issued address costs
one provider call each, so the result is computed in the background and cached for 5 minutes
(`updated_at`): when no fresh figure exists the call answers `503 "Balances are being
computed — retry shortly"` with `Retry-After: 15`. Project balances are the ledger view of the
same funds: `GET /projects/{id}/balance` and `GET /ledger`.

`GET /lightning/payments` (admin): every payment of the Breez/Spark wallet, newest first —
received and sent, completed, pending or failed, **including funds unrelated to gateway
invoices** (anything else using the same seed shows up too). Each entry carries `direction`,
`status`, `amount` (BTC) / `amount_sats`, `fees_sats`, `method`, the `invoice` (Lightning) or
`tx_id` (on-chain deposit/withdrawal), `created_at`, and `deposit_address_id` when it settled a
gateway invoice. The SDK reports no total: page with `limit` (1–100, default 50) / `offset`
and the returned `has_more`. `503` when Lightning is not configured. The dashboard shows it
under **Lightning**.

## Webhook events & payloads

Webhook endpoints are managed in Dashboard → Webhooks (admin). Each endpoint belongs to an
**owner** — a project or the merchant — fixed at creation, subscribes to one or more events
and only receives those **about its owner**:

| Event | Fired when | `event_type` in payload |
|---|---|---|
| `pending` | A deposit address is created | `payment_pending` |
| `paid` | A deposit is confirmed on-chain | `deposit_confirmed` |
| `expired` | A pending address passes its expiry unpaid | `payment_expired` |
| `payout_succeeded` | An outgoing Lightning wallet payment the gateway did **not** make as a withdrawal completed (the wallet may be shared; token sends are not reported) | `payout_succeeded` |
| `payout_failed` | Such a payment failed | `payout_failed` |
| `withdrawal_pending_approval` | A withdrawal waits for a second admin | `withdrawal_pending_approval` |
| `withdrawal_broadcast` | Its transaction reached the network (Lightning: the wallet accepted the send) | `withdrawal_broadcast` |
| `withdrawal_confirmed` | It is confirmed (deposit thresholds; Lightning: settled) | `withdrawal_confirmed` |
| `withdrawal_failed` | It failed (reverted, refused, abandoned…) | `withdrawal_failed` |
| `withdrawal_cancelled` | It was cancelled or rejected, or its approval expired | `withdrawal_cancelled` |
| `gas_reserve_low` | A treasury's gas asset fell below its alert threshold | `gas_reserve_low` |
| `treasury_cap_exceeded` | An asset's treasuries hold more than its hot-wallet cap | `treasury_cap_exceeded` |
| `address_book_entry_added` | A destination was added to the address book | `address_book_entry_added` |
| `withdrawal_stuck` | A withdrawal reached its fee cap (or its expiry cannot be proven) | `withdrawal_stuck` |
| `transfer_completed` | An internal transfer was recorded (both owners are notified) | `transfer_completed` |
| `ledger_shortfall` | An asset's project balances exceeded the holdings on three consecutive checks | `ledger_shortfall` |

Sweeps and gas top-ups are not notified. Alerts are deduplicated: once a day per condition
(gas reserve, cap, shortfall) while it holds, once per subject for the others.

**Owner rule.** Events follow the owner of their subject: payment events go to the
endpoints of the payment's project; withdrawal events to the endpoints of the withdrawal's
owner (its project, or the merchant's endpoints for a merchant-funds withdrawal);
`transfer_completed` to the endpoints of both sides; alerts and `payout_*` to the merchant's
endpoints only. Hence the events an endpoint may subscribe to depend on its owner:

| Endpoint owner | Allowed events |
|---|---|
| project | `pending`, `paid`, `expired`, the five `withdrawal_*`, `transfer_completed` |
| merchant | the five `withdrawal_*` (merchant-funds withdrawals only), `transfer_completed`, `payout_succeeded`, `payout_failed`, `gas_reserve_low`, `treasury_cap_exceeded`, `address_book_entry_added`, `withdrawal_stuck`, `ledger_shortfall` |

Other events are refused with `422`. Before the first project exists (an upgraded instance)
every active subscribed endpoint receives every event, as before projects.

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

Withdrawal payload (the withdrawal's current state; `fee` / `fee_raw` once known;
`project_id` is `null` for a merchant-funds withdrawal):

```json
{
  "event_type": "withdrawal_confirmed",
  "data": {
    "withdrawal_id": "uuid", "project_id": "uuid", "status": "confirmed", "network": "tron", "token": "USDT",
    "amount": "250.000000", "amount_raw": "250000000", "fee": "6.773500", "fee_raw": "6773500",
    "fee_token": "TRX", "destination": "TJDENsf…", "address_book_entry_id": null,
    "tx_hash": "98f8aa28…", "confirmations": 19, "reference": "payout-42",
    "requested_by": { "type": "api_key", "id": "uuid" }, "failure_reason": null
  }
}
```

Alert payload: `{ "event_type": "gas_reserve_low", "data": { "alert_id", "type", "network",
"token", "details": { … } } }`. For `ledger_shortfall`, `details` holds `holdings_raw`,
`project_balances_raw`, `shortfall_raw` and `consecutive_checks` for the alert's (network,
token).

Internal transfer payload (`from` / `to`: `{ "type": "project", "id": "uuid" }` or
`{ "type": "merchant", "id": null }`):

```json
{
  "event_type": "transfer_completed",
  "data": {
    "transfer_id": "uuid", "network": "bitcoin", "token": "BTC",
    "amount": "0.0001", "amount_raw": "10000",
    "from": { "type": "merchant", "id": null }, "to": { "type": "project", "id": "uuid" },
    "note": "refund of the sweep fee", "created_at": "2026-10-17T12:00:00+00:00"
  }
}
```

Payload on confirmed deposit (`paid`, historical shape plus `project_id`):

```json
{
  "event_type": "deposit_confirmed",
  "data": {
    "deposit_id": "uuid",
    "project_id": "uuid",
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
    "project_id": "uuid",
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

If no active webhook endpoint is configured, a warning is logged at startup and webhooks are disabled. Each delivery is tracked per (deposit, endpoint) pair for `paid`, per (address, event, endpoint) for `pending`/`expired`, and per (subject, event, endpoint) for the withdrawal, transfer, payout and alert events; before every attempt the endpoint must still be active, subscribed and the owner of the subject, otherwise the delivery is closed as `failed`; `deposits[].webhooks[]` in the deposit-address detail exposes the per-endpoint state, while `deposits[].webhook` keeps the historical single-object shape (most recent delivery).

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
| accessory `postgres` | — (docker network only) | PostgreSQL 16, volume `crypto-payment-gateway-postgres` | `pg_isready` |
| accessory `redis` | — (docker network only) | Redis 7.4 (AOF, password), volume `crypto-payment-gateway-redis` | `redis-cli ping` |

PostgreSQL and Redis run as **Kamal accessories** on the environment's server (one server per
environment). The API reaches them by container name over the `kamal` docker network, with no
published port; `.kamal/secrets` builds `DATABASE_URL` and `REDIS_URL` from the
`POSTGRES_PASSWORD` and `REDIS_PASSWORD` secrets. That traffic never leaves the host, so the
api role sets `ALLOW_INSECURE_TRANSPORT=true` (no TLS between containers; the boot warning is
expected). Migrations run when the api container boots. On a fresh database, the dashboard's
`/setup` page lets **anyone** create the first admin: open it right after the first deploy. If
it already says the setup is done and you did not do it, someone else claimed the instance —
wipe the database before any deposit (`docker volume rm crypto-payment-gateway-postgres`, with
the accessory stopped) and deploy again.

**GitHub Actions → Deploy** (manual, pick the environment) runs `kamal deploy` — only from
`main` or a `v*` tag, for a commit on `main` whose CI run succeeded (a `guard` job checks it
before any secret is exposed). Per GitHub environment:

- Variables: `GATEWAY_HOST` (server, SSH target), `API_HOST`, `DASHBOARD_HOST`,
  `SSH_KNOWN_HOSTS` (the server's pinned host key, e.g. from `ssh-keyscan -t ed25519
  <GATEWAY_HOST>` checked against the server console — required: host keys are verified)
  (required); `APP_ENV` (defaults to the environment name), `SSH_USER` (default `deploy`: a
  dedicated user in the `docker` group — create it before the first deploy, or set the variable
  to `root` to keep deploying as root), `BITCOIN_ESPLORA_BASE_URL`,
  `TRUSTED_PROXIES` (defaults to the private docker ranges kamal-proxy lives in — a custom
  value must also list the server's public IP, which the dashboard's nginx reaches the API
  through), `OUTFLOWS_FROZEN` (`true` holds the outflow freeze).
- Secrets: `SSH_PRIVATE_KEY`, `WALLET_MNEMONIC`, `JWT_SECRET`, `POSTGRES_PASSWORD`,
  `REDIS_PASSWORD` (at least 32 letters, digits, `-` or `_`, checked before the deploy — they
  go unencoded into the URLs: `openssl rand -hex 32`); optional provider keys
  `TRONGRID_API_KEY`, `ALCHEMY_API_KEY`, `HELIUS_API_KEY`, `BREEZ_API_KEY` (empty disables
  the network).

Each run boots the missing accessories (`kamal accessory boot all`, a no-op once they run),
then `kamal deploy`. Its `reboot-accessory` input recreates `postgres` or `redis` first, on the
same volume: that is how a new image or setting from `config/deploy.yml` is applied (a few
seconds without it; the API reconnects).

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

A brand-new server only needs Docker (installed by hand, or `kamal server bootstrap` once as
root) and the deploy user (`adduser deploy && usermod -aG docker deploy`, with the workflow's
public key in its `authorized_keys`); the Deploy workflow boots the accessories and kamal-proxy
itself. To run kamal from a
workstation, export the same variables and secrets first (`.kamal/secrets` only references
them). Useful aliases: `kamal logs`, `kamal console`.

### Database

⚠️ **No automated backup.** The database lives only in the server's
`crypto-payment-gateway-postgres` volume: losing the server loses the payment, withdrawal and
webhook history (funds stay recoverable from `WALLET_MNEMONIC`). Dump it regularly and keep the
dumps off the server. Named volumes survive `kamal accessory remove` and `kamal remove`;
`docker volume rm` deletes them for good.

```bash
# Dump to your machine (pg_dump connects over the container's socket: no password)
ssh <SSH_USER>@<GATEWAY_HOST> docker exec crypto-payment-gateway-postgres \
  pg_dump -U gateway -d crypto_payment -Fc > crypto_payment-$(date +%F).dump

# Restore (stop the api first: kamal app stop -r api; redeploy afterwards)
ssh <SSH_USER>@<GATEWAY_HOST> docker exec -i crypto-payment-gateway-postgres \
  pg_restore -U gateway -d crypto_payment --clean --if-exists --no-owner < crypto_payment-YYYY-MM-DD.dump

# psql
ssh -t <SSH_USER>@<GATEWAY_HOST> docker exec -it crypto-payment-gateway-postgres psql -U gateway -d crypto_payment
```

Redis persists to its volume (append-only file). Losing it drops the queued webhook deliveries
(their rows stay `pending` in Postgres), the staff sessions (everyone signs in again), the
balance cache and the Lightning payout watermark.

- **Rotating `POSTGRES_PASSWORD`**: the image only reads it when the volume is initialized.
  Change it in the database first (`psql … -c '\password gateway'`), then update the secret
  and deploy.
- **Rotating `REDIS_PASSWORD`**: update the secret and deploy with `reboot-accessory: redis`.
- **New image**: update the digest in `config/deploy.yml` (Dependabot does not read it; keep
  the majors of `compose.yaml`), then deploy with `reboot-accessory`. A new PostgreSQL
  **major** needs a dump and a restore into a fresh volume.

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
- Enable 2FA for at least two admins: withdrawals, approvals, withdrawal grants, limits, the
  address book and staff management require a fresh TOTP code, and approvals need a second
  admin
- Keep the withdrawal limits and hot-wallet caps (Settings → Assets) as low as your use
  allows, move the excess to cold storage, and review the audit log
- Give each application its own **project** and key: a key only sees and spends its
  project, so a leaked key is bounded by that project's balance and limits
- Know where the **Freeze** button is: any staff member stops every outflow at once

[`.env.example`](.env.example) uses only the well-known BIP39 test vector (`abandon … about`): the service refuses it outside `APP_ENV=development`, and never connects the (mainnet) Lightning wallet with it.

Hardening in place: TLS required to Redis/Postgres outside development, provider endpoints
checked against mainnet at boot (a network whose provider answers for another chain stays
disabled), session cookies + CSRF header, step-up TOTP, rate limits, SSRF-safe webhook
client, generic error messages (details only in the logs), security headers, an
append-only audit log, a merchant API scoped to the key's project in SQL (another project's
object is a `404`, never a `403`, and no custody figure reaches a key or a project endpoint).
Known limits: projects are a ledger over **pooled** custody, not isolated wallets (an admin
may spend beyond a project's balance; a shortfall is detected, not prevented), no reorg
handling beyond confirmation thresholds, secrets (TOTP, webhook signing) stored in clear in
the database, a single seed for on-chain and Lightning funds.

Scope today: inbound USDT / ETH / BTC / Lightning deposit detection and webhooks, and
withdrawals from hot treasuries signed in-process, for several projects of one merchant.
This is not a multi-tenant or non-custodial vault: compromising the API process means
compromising every hot fund, bounded only by the guard rails above. See the
[custody note](#custody-note) for Lightning funds.

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
| `ALLOW_INSECURE_TRANSPORT` | No | `false` | Outside development, start even if `REDIS_URL` is not `rediss://` or `DATABASE_URL` lacks `sslmode=verify-full` (loud warning). Only for a private network |
| `DASHBOARD_PORT` | No | `8080` | Host port of the dashboard container (docker compose) |
| `CORS_ALLOWED_ORIGINS` | No | — | Comma-separated origins (`scheme://host[:port]`, never `*` — an invalid entry fails startup); only needed if the dashboard is not same-origin |
| `API_TRAFFIC_ENABLED` | No | `true` | Record every `/api/*` request into the `api_requests` log shown on Dashboard → Traffic (retention is a dashboard setting, 90 days by default) |
| `TRUSTED_PROXIES` | No | — | Comma-separated CIDRs of reverse proxies whose `X-Forwarded-For` is trusted for the client IP (traffic log, rate limits, audit log). The header is walked from the right and the first hop **not** in this list is the client, so list every proxy of the chain (e.g. the docker network and, when the dashboard reaches the API through its public host, the server's own public IP). Empty = trust none: the socket peer is used and forwarding headers are ignored. An invalid CIDR fails startup |
| `BITCOIN_ESPLORA_BASE_URL` | No* | — | Esplora-compatible REST base (`https://mempool.space/api`, `https://blockstream.info/api`, or self-hosted). Unset disables Bitcoin |
| `BTC_CONFIRMATIONS` | No | `3` | Confirmations before a Bitcoin deposit is confirmed / paid (≥ 1; a warning below 3) |
| `BREEZ_API_KEY` | No* | — | Breez SDK Spark API key ([breez.technology](https://breez.technology)). Unset disables Lightning |
| `BREEZ_STORAGE_DIR` | No | `./.breez-data` | Breez SDK local storage (SQLite), resolved to an absolute path at startup. Must persist across restarts (docker compose mounts the `breez_data` volume) |
| `OUTFLOWS_FROZEN` | No | `false` | `true` forces the [outflow freeze](#treasury-sweeps--withdrawals-operator-runbook): no withdrawal, sweep or gas top-up leaves the gateway, and the dashboard cannot lift it while set |
| `CHAIN_MODE` | No | `mainnet` | `testnet` (development only) moves every network to its test chain — see [Testnet mode](#testnet-mode) |
| `TESTNET_USDT_ETH_CONTRACT`, `TESTNET_USDT_TRON_CONTRACT`, `TESTNET_USDT_SOLANA_MINT` | With `CHAIN_MODE=testnet` | — | Test tokens standing in for USDT |

\* Required for monitoring on the respective network.
\** Required for the admin API and the dashboard.

Advanced overrides (code defaults if unset): `POLLING_INTERVAL_SECONDS` (10), `ETH_CONFIRMATIONS` (12, minimum 12), `TRON_CONFIRMATIONS` (19, minimum 19), `AMOUNT_TOLERANCE_PERCENT` (1, between 0 and 5). An unparsable or out-of-range value fails startup instead of falling back to the default. Provider base URLs must use `https://` outside development.

Projects (Dashboard → Projects), webhook endpoints (Dashboard → Webhooks), the default
payment expiry (60 min, max 24 h; Dashboard → Settings) and every withdrawal guard rail
(limits, approval thresholds, caps, gas reserve thresholds, sweep minimums — Dashboard →
Settings → Assets) are **not** environment variables — they are stored in the database and
managed from the dashboard (admin only). The ledger tuning is fixed in code: balances cached
30 s, holdings 5 min, the shortfall alert after 3 consecutive checks of the 5-minute monitor.

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
payment expires normally. Deposit addresses are never swept: Bitcoin withdrawals spend their
confirmed UTXOs (and the treasury's change) directly.

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
mnemonic alone, like USDT, ETH and the treasuries. Lightning funds live in the Breez/Spark wallet: recoverable from
the same mnemonic **through the Spark SDK/protocol**, which is a different trust model
(Spark L2). Enabling Lightning (`BREEZ_API_KEY`) must be an explicit, conscious operator
choice.

## Treasury, sweeps & withdrawals (operator runbook)

Every on-chain withdrawal leaves from one **treasury** per network, derived from
`WALLET_MNEMONIC` on paths no deposit index reaches: Ethereum and Tron `m/44'/60'/1'/0/0`
(one key, two encodings), Bitcoin `m/84'/0'/1'/0/0` (also the change address), Solana
`m/44'/501'/0'/1'` (its USDT sits on that owner's token account). The addresses are printed
at boot and shown on Dashboard → Treasury. Transactions are built and signed inside the
gateway, and each one is stored before it is broadcast: a restart re-broadcasts the same
bytes, never a second payment.

**Sweeps** (every 5 min, plus a daily rescan of every finished payment's address): the
confirmed funds of paid or expired payments move to the treasury once above the per-(network,
token) minimum (ETH 0.01 · USDT-ERC20 50 · USDT-TRC20 20 · USDT-SPL 1 by default). A USDT
deposit address on Ethereum or Tron first receives an exact **gas top-up** from the treasury;
on Solana the treasury pays the fee and the emptied token account is closed (its rent comes
back; the first sweep creates the treasury's own USDT token account). Bitcoin is never swept. Funds that settled no payment are swept too and reported as
*unattributed*.

Before going live:

1. **Fund the gas reserves** — ETH on the Ethereum treasury, TRX on the Tron treasury (its
   first TRX transfer **activates** the account), SOL on the Solana treasury — above the
   floors of Settings → Assets (defaults 0.05 ETH, 50 TRX, 0.1 SOL; alerts below 0.1 ETH,
   200 TRX, 0.5 SOL). Under its floor a network stops top-ups and gas-asset withdrawals.
2. Add a **cold-storage** entry to the address book for every owner that should use one —
   each project, and the merchant funds — each usable 24 h later. When an asset's treasuries
   exceed its hot-wallet cap an alert fires, and Dashboard → Treasury offers to send the
   excess to cold storage — a normal withdrawal (TOTP, approval above the threshold) debited
   from the owner you pick (see *Projects and cold storage* below).
3. Enable **2FA for at least two admins** (approvals need a second admin; alone, the
   requester may approve after 24 h).
4. Review the default limits — per withdrawal / 24 h / approval at / cap: USDT 10 000 /
   50 000 / 5 000 / 100 000 · ETH 5 / 20 / 2 / 50 · BTC 0.01 / 0.1 / 0.005 / 1 (the former
   payout limits when they had been changed) · TRX 10 000 / 50 000 / 5 000 · SOL 50 / 200 /
   20.
5. Know the **Freeze** button (dashboard header): any staff member stops every withdrawal,
   sweep and top-up at once; only an admin with a TOTP code lifts it, and
   `OUTFLOWS_FROZEN=true` holds it from the environment. Quotes, approvals and cancellations
   keep working while frozen; nothing is signed or broadcast.

Sending uses the monitoring providers (TronGrid, Alchemy — on a plan serving
`alchemy_getAssetTransfers` and wide `eth_getLogs` ranges —, Helius, Esplora): a network
without its provider has its withdrawals and sweeps disabled.

**Projects and cold storage.** Funds sent to cold storage are a withdrawal, so they are
debited from an owner: the dialog requires one like any admin withdrawal and lists only that
owner's `is_cold_storage` entries. When no single project's available balance covers the
excess, send several withdrawals, one per project (the dialog shows each project's available
balance), or debit the merchant funds. To **bring funds back**, send them to the treasury
address — a treasury deposit raises the merchant funds — then make an internal transfer from
the merchant funds to the project (Treasury → Ledger → *New internal transfer*). The
**Ledger** card of the Treasury page is the reconciliation: holdings (confirmed treasury
balance plus the unswept deposit addresses at the confirmation depth) against the sum of the
project balances, per asset; the difference is the merchant funds. A negative difference is a
**shortfall** — the project balances promise more than the gateway holds (an admin spent
beyond a project, an unswept address was emptied by hand…): after three consecutive 5-minute
checks the `ledger_shortfall` alert fires, and the fix is an internal transfer from the
merchant funds (once they exist again) or a treasury deposit. `unknown` means the pair cannot
be reconciled for now: network disabled, provider error, or sweeping switched off for it
(unswept addresses are then not counted).

**Execution.** Withdrawals run asynchronously. Workers claim them, re-check the approval, the
destination rules and the requesting key right before signing, and track each transaction to
the deposit confirmation thresholds (Tron 19, Ethereum 12, Bitcoin 3, Solana finalized,
Lightning settled). A stuck Ethereum or Bitcoin transaction is replaced with higher fees
(+25 %, RBF on Bitcoin) up to 3 × its quote; beyond that the withdrawal is `stuck` (alert)
and an admin bumps it (doubles the cap) or abandons it once the network proves it cannot
land. Tron and Solana transactions are only signed again once their reference block or
blockhash provably expired (a Tron one the node still knows 10 min later is `stuck`). A
Lightning withdrawal is `broadcast` from the moment it is handed to the wallet and can no
longer be cancelled. Tron transactions are built by TronGrid, then decoded and checked
(owner, contract, recipient, amount, fee limit) before anything is signed.

## Testnet mode

`CHAIN_MODE=testnet` (refused unless `APP_ENV=development`) moves the provider defaults to
Sepolia, Nile and Solana devnet — point `BITCOIN_ESPLORA_BASE_URL` at a testnet4 Esplora
(`https://mempool.space/testnet4/api`) — checks those chains' identities at boot,
derives Bitcoin addresses with coin type `1'` (`tb1…`, deposits and treasury alike) and
connects Lightning to Spark regtest. There is no canonical testnet USDT: set
`TESTNET_USDT_ETH_CONTRACT`, `TESTNET_USDT_TRON_CONTRACT` and `TESTNET_USDT_SOLANA_MINT` to
the test tokens you use. Never point a testnet instance at a mainnet database.

## USDT contracts

| Network | Contract |
|---|---|
| Ethereum | `0xdAC17F958D2ee523a2206206994597C13D831ec7` |
| Tron | `TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t` |
| Solana | `Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB` |
