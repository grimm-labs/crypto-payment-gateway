# Spec — Projects: several applications on one gateway instance

| | |
|---|---|
| Status | Ready for implementation |
| Base | `d54615e` (branch `feat/kamal-accessories`) |
| Glossary | [`CONTEXT.md`](../CONTEXT.md) — terms in **bold** below are defined there (Merchant, Project, Project balance, Merchant funds, Internal transfer, Owner, Ledger shortfall) |
| Decisions | [ADR 0004](adr/0004-projects-ledger-over-pooled-custody.md); builds on [ADR 0001](adr/0001-treasury-and-sweeping.md) and the [withdrawals spec](eth-and-withdrawals-spec.md) |
| Delivery | One release. Production has not launched, so the breaking changes to `/api/v1` (§11) ship without a deprecation period |

Read `AGENTS.md` first: every convention there applies, including Prisma-style SQL, the enum casings, amounts through `domain` helpers, `ApiError`, audit, rate limits, Redis keys through `config.redis_key`, migrations that never fail on existing data, and the `DISTINCT ON` rule for webhook deliveries. Where this spec says "as for X today", follow the existing code for X.

---

## 1. Goals

1. One **merchant** runs several **projects** on one instance. Each project owns its merchant keys, deposit addresses (payments), withdrawals, webhook endpoints and address-book entries.
2. **See the projects apart**: a project selector and a color per project throughout the dashboard, with per-project stats and balances.
3. **Isolate the projects from each other**: a project's keys and webhook endpoints never see another project's data, nor the treasury.
4. **A project balance per (network, token)**, enforced on withdrawals requested by keys, with admin **internal transfers** between owners and a reconciliation against what the gateway really holds.
5. **No default project.** On first launch, a blocking step asks an admin to create the first project.

## 2. Out of scope

- Physical separation of funds: per-project derivation paths, treasuries or Lightning wallets ([ADR 0004](adr/0004-projects-ledger-over-pooled-custody.md)).
- Charging sweep and gas costs to projects (the "strict complete" option). They reduce the **merchant funds**.
- Staff access restricted to some projects. Every staff member sees every project.
- Archiving a project. Only renaming, and deleting an empty project, are supported.
- Moving a key, payment, withdrawal, endpoint or address-book entry to another project.
- Per-project asset settings, withdrawal limits, approval thresholds or default payment expiry. Per-key limits remain the per-caller control.
- A per-project view of the Lightning wallet history.
- Several merchants (multi-tenancy), and third parties operating a project.
- Automatic detection of funds coming back from cold storage (§8.6).

## 3. Decisions log

| # | Decision |
|---|---|
| Q1 | A single **merchant** (the operator); a **project** is one of the merchant's applications. "Merchant key" stays the term for an API key, which now belongs to a project. |
| Q2 | Strict-light separation: a withdrawal requested by a key may not exceed its project's balance. Admins may exceed it. |
| Q3 | Project balances are kept per (network, token). |
| Q4 | Credits: confirmed deposits, plus the **unattributed funds** swept from the project's deposit addresses. |
| Q5 | Bitcoin on-chain (never swept): unattributed funds are detected by the address scan (total received − confirmed deposits). |
| Q6 | Debits: withdrawn amounts, plus their network fees when those are paid in the withdrawn asset (BTC on-chain, ETH, Lightning). Fees paid in a gas asset stay a merchant cost. |
| Q7 | An admin withdrawal names a project or the **merchant funds**, chosen explicitly (no default). TRX/SOL withdrawals are always merchant funds. The dashboard shows the gap between holdings and the sum of project balances. |
| Q8 | Every staff member sees every project; the selector only filters the view. |
| Q9 | A webhook endpoint belongs to one owner, fixed at creation. URLs are unique per owner. Payloads gain `project_id`. |
| Q10 | Merchant endpoints (owner = merchant) receive the instance events: alerts and `payout_*`. |
| Q11 | A key belongs to one project forever. |
| Q12 | Every deposit address belongs to a project; staff choose it when creating a payment from the dashboard. |
| Q13 | Blocking "create your first project" step after `/setup`. Only admins can create it; operators see a waiting screen. |
| Q14 | Data that predates the feature is attached to the first project in the same transaction (opt-out checkbox). Endpoints subscribed to both kinds of events are split in two. |
| Q15 | Address-book entries belong to an owner (project or merchant). |
| Q16 | Treasury, sweeps, gas, freeze, asset settings, alerts, staff, audit and app settings stay instance-level. The **key tag** is per project, falling back to the global tag. |
| Q17 | A project can be renamed. It can be deleted only when empty, and never when it is the last one. |
| Q18 | Project attributes: name, color (8-color palette), optional key tag. |
| Q19 | Admins manage projects; no TOTP; audited. |
| Q20 | Merchant API visibility is per project: every key of a project sees all of it. Another project's object answers 404. |
| Q21 | `GET /api/v1/project` returns the key's project and its balances. `/balances` and `/wallet-balances` become staff-only. |
| Q22 | The Lightning wallet history becomes staff-only. |
| Q23 | Global project selector in the header. Instance pages show "Instance" instead. |
| Q24 | Home page: per-project stats and balance cards; "All projects" shows a table per token. The Treasury page gains the reconciliation table; a Projects page is added. |
| Q25 | `LEDGER_SHORTFALL` alert after 3 consecutive negative checks. |
| Q26 | Events follow the **owner** of their subject. A merchant-funds withdrawal notifies merchant endpoints. `withdrawal_stuck` stays an alert. |
| Q27 | Internal transfers between any two owners (project ↔ project, project ↔ merchant funds): same (network, token), admin + TOTP, append-only, `transfer_completed` sent to both sides. |
| Q28 | Funds sent to cold storage are a withdrawal debited from a project. To bring them back: a treasury deposit, then an internal transfer from the merchant funds to that project. |

## 4. Invariants (must hold at all times)

1. **One owner, forever.** Every key, deposit address, withdrawal, internal-transfer side, webhook endpoint and address-book entry gets its owner at creation, and the owner never changes. The owner is a project, or the merchant where §5 allows it. Once a project exists, no active key lacks a project, and every new deposit address has one.
2. **The merchant surface is project-scoped in SQL.** Every read and every mutation reached with a merchant key filters on the key's project in the query itself. Another project's object answers `404`, exactly like a missing one. No bare `WHERE id = $1` on the merchant surface.
3. **A key withdrawal fits in its project's balance.** It is admitted only if the project's available balance covers `amount` plus, when the fee is paid in the same asset, the fee cap. The check runs inside the per-asset advisory-lock admission transaction and is re-run at approval. Internal transfers take the same lock.
4. **Project balances are derived, never stored.** They are recomputed from source rows (deposits, sweeps, scans, withdrawals, transfers) on every read, as integer base units. Values are signed (`i128`) only in arithmetic and rendering.
5. **Events follow their owner.** Payment and withdrawal events go only to endpoints with the same owner as their subject. Alerts and `payout_*` go only to merchant endpoints; `transfer_completed` goes to the endpoints of both sides. `deliver_webhook` re-checks owner, activity and subscription before every attempt.
6. **Internal transfers are append-only** (DB trigger), idempotent, and confined to one (network, token).
7. **Nothing about custody reaches a project.** Treasury balances, holdings, merchant funds and other projects' figures are never sent to a merchant key or a project endpoint.
8. The invariants of the [withdrawals spec §4](eth-and-withdrawals-spec.md#4-invariants-must-hold-at-all-times) are unchanged.

## 5. Ownership model

| Object | Owner | Set from | Notes |
|---|---|---|---|
| Merchant key | project (required) | creation form | Its key tag comes from the project (§6.1) |
| Deposit address (payment) | project (required) | the calling key's project, or the staff member's choice | Deposits, sweeps and BTC scans inherit it through the address |
| Withdrawal by a key | the key's project | the key | Capped by the project balance (invariant 3) |
| Withdrawal by an admin | a project, or the merchant | explicit `project_id` (no default) | TRX/SOL: merchant only (DB CHECK, §6.2) |
| Internal transfer | two sides: `from` and `to` owners | admin form | Each side is a project or the merchant |
| Webhook endpoint | a project, or the merchant | creation form | The allowed events depend on the owner (§10.2) |
| Address-book entry | a project, or the merchant | creation form | Same destination allowed once per owner |
| Alert, non-gateway Lightning send | the merchant | — | Delivered to merchant endpoints |

No owner, instance-level: treasuries, sweeps, gas reserves and top-ups, the outflow freeze, `asset_settings`, `sweep_settings`, alerts as objects, staff, the audit log, `app_settings`. Per-key limits stay attached to their key.

---

## 6. Data model

All columns camelCase and quoted, enums PascalCase, amounts `NUMERIC(78,0)` read and bound as `db::RawAmount` (AGENTS.md).

### 6.1 `projects`

| Column | Type | Notes |
|---|---|---|
| `id` | TEXT PK | UUID |
| `name` | TEXT NOT NULL | 1–60 chars after trim; unique index on `lower("name")` |
| `color` | TEXT NOT NULL | CHECK in the palette `blue`, `emerald`, `amber`, `rose`, `violet`, `cyan`, `orange`, `fuchsia` |
| `keyTag` | TEXT NULL | CHECK `"keyTag" ~ '^[A-Za-z0-9]{1,5}$'`; partial unique index on `lower("keyTag")` when set |
| `createdById` | TEXT NOT NULL | FK `staff_users` |
| `createdAt`, `updatedAt` | TIMESTAMP(3) | |

- **Color**: when none is given, the first palette color no project uses yet; once all are taken, the least-used one, palette order breaking ties.
- **Key tag**: a new key of the project is generated with `keyTag` when set, otherwise with `app_settings.apiKeyPrefix` (today's behaviour). `auth::parse_api_key_prefix` stays tag-agnostic, so existing keys keep validating.

### 6.2 Owner columns

`"projectId" TEXT NULL`, FK `projects(id)` `ON DELETE RESTRICT ON UPDATE CASCADE`, on:

| Table | NULL means | Index |
|---|---|---|
| `api_keys` | legacy key from before the first project (§13) | `("projectId")` |
| `deposit_addresses` | legacy payment from before the first project | `("projectId", "createdAt" DESC)`, `("projectId", status)` |
| `withdrawals` | merchant funds | `("projectId", "createdAt" DESC)`, `("projectId", network, token, status)` |
| `webhook_endpoints` | merchant endpoint | — |
| `address_book_entries` | merchant entry | — |

- `withdrawals`: CHECK `token NOT IN ('TRX', 'SOL') OR "projectId" IS NULL` (gas-only assets are always merchant funds).
- `webhook_endpoints`: drop the unique index `webhook_endpoints_url_key`, then create two partial unique indexes: `("projectId", url) WHERE "projectId" IS NOT NULL` and `(url) WHERE "projectId" IS NULL`. Existing URLs are already unique, so neither index can fail.
- `address_book_entries`: replace the partial unique index on `(network, destination) WHERE "deletedAt" IS NULL` with two: `("projectId", network, destination)` for project entries and `(network, destination)` for merchant entries, both `WHERE "deletedAt" IS NULL`.

### 6.3 `internal_transfers`

| Column | Type | Notes |
|---|---|---|
| `id` | TEXT PK | UUID |
| `idempotencyKey` | TEXT UNIQUE | caller-provided UUID |
| `paramsHash` | TEXT | SHA-256 of the normalized from, to, network, token, amount and note |
| `network` | `"Network"` | |
| `token` | `"Token"` | a **payment** pair only (`Network::payment_tokens`): projects never hold TRX/SOL |
| `amountRaw` | NUMERIC(78,0) | CHECK `> 0` |
| `fromProjectId` / `toProjectId` | TEXT NULL | FK `projects`; NULL = merchant funds; CHECK `"fromProjectId" IS DISTINCT FROM "toProjectId"` |
| `note` | TEXT NOT NULL | 1–200 chars |
| `staffId` | TEXT NOT NULL | FK `staff_users` |
| `createdAt` | TIMESTAMP(3) | |

- Indexes: `("fromProjectId", "createdAt" DESC)`, `("toProjectId", "createdAt" DESC)`, `(network, token)`.
- Append-only: a `BEFORE UPDATE OR DELETE` trigger raises an exception, on the same pattern as `audit_log`.

### 6.4 Bitcoin received watermark

`deposit_addresses."btcReceivedRaw" NUMERIC(78,0) NULL`: the total confirmed amount ever received by a Bitcoin deposit address, at `BTC_CONFIRMATIONS` depth (§7.1).

- Written by the Bitcoin address scan.
- Only ever raised: `GREATEST(old, new)`. A smaller reading from a lagging provider is ignored.
- NULL on every other network.

### 6.5 Webhooks and alerts

- `webhook_deliveries` gains a `"transferId" TEXT NULL` column, FK `internal_transfers`.
  - The subject CHECK becomes `num_nonnulls("depositAddressId", "lightningPaymentId", "withdrawalId", "alertId", "transferId") = 1`.
  - New partial unique index `("transferId", "eventType", "webhookEndpointId") WHERE "transferId" IS NOT NULL AND "webhookEndpointId" IS NOT NULL`.
  - **Every** `DISTINCT ON` over deliveries adds `"transferId"` to its key (AGENTS.md rule).
- `"AlertType"` gains `LEDGER_SHORTFALL` (`ALTER TYPE … ADD VALUE IF NOT EXISTS`, not used in the same migration).
- `WebhookEventType` (TEXT) gains `TRANSFER_COMPLETED` and `LEDGER_SHORTFALL`; no DDL.

---

## 7. Project balance

### 7.1 Components, per (project, network, token)

| Component | Source | Rule |
|---|---|---|
| `deposits` | `deposits` ⋈ `deposit_addresses` | Σ `"amountRaw"::numeric` (TEXT column) of `CONFIRMED` deposits, the amount actually received |
| `unattributed` (swept networks) | `sweeps` ⋈ `deposit_addresses` | Σ `"unattributedRaw"` of `CONFIRMED` sweeps |
| `unattributed` (bitcoin) | `deposit_addresses` | Σ over the project's Bitcoin addresses of `GREATEST("btcReceivedRaw" − Σ its CONFIRMED deposits, 0)` |
| `withdrawn` | `withdrawals` | Σ `"amountRaw"` of `CONFIRMED` withdrawals |
| `fees` | `withdrawals` | Σ `"feeRaw"` of `CONFIRMED` and `FAILED` withdrawals **where `token = "feeToken"`** (a fee actually spent in the withdrawn asset) |
| `transfers_in` / `transfers_out` | `internal_transfers` | Σ `"amountRaw"` where the project is `to` / `from` |
| `pending` (informational) | `deposits` | Σ `DETECTED` deposits: shown, never counted |

```
balance   = deposits + unattributed + transfers_in − withdrawn − fees − transfers_out
reserved  = Σ over the project's withdrawals in PENDING_APPROVAL, QUEUED, SIGNED, BROADCAST, STUCK of
            "amountRaw" + (token = "feeToken" ? COALESCE("feeCapRaw", "feeEstimateRaw", 0) : 0)
available = balance − reserved
```

- The fee rule follows Q6. A USDT withdrawal on Ethereum pays its fee in ETH: `token ≠ feeToken`, so the project is not charged. An ETH, BTC or Lightning withdrawal pays in the same asset, so the project is charged.
- `reserved` uses the fee **cap**: the actual fee can never exceed what was reserved, except after an admin bump (§8.4).
- `balance` can be negative only after admin actions: a withdrawal attributed to the project (Q7), or a bumped fee cap. The dashboard shows it in red.
- Bitcoin unattributed funds are credited from the scan alone. A withdrawal that spends a project's UTXOs does not lower `btcReceivedRaw`.

### 7.2 Computation

- **Database**: `db::ledger::project_components(conn, filter)` returns the components above per (projectId, network, token) in one round trip (`UNION ALL` of grouped aggregates). The filter covers one project, all projects, or one (network, token).
  - It takes a `&mut PgConnection`, so the admission check (§8.3) runs it inside the locked transaction.
- **Arithmetic**: a pure function in `domain`, `ProjectBalance::from_components(...) -> ProjectBalance { balance: i128, reserved: u128, available: i128, … }`, unit-tested on its own.
- **Rendering**: through the `domain` amount helpers at token scale, prefixed with `-` when negative. API fields come in pairs (`balance` / `balance_raw`, …), like every amount today.
- **Never cached** for the admission check. Dashboard reads may share a 30 s Redis cache keyed `ledger:projects` (`config.redis_key`), invalidated on every withdrawal status change and every internal transfer.

### 7.3 Bitcoin scan

The Bitcoin address scan of `jobs::sweeper` already reads every funded final deposit address to feed the UTXO pool. It now also writes `btcReceivedRaw`: the confirmed amount received at `BTC_CONFIRMATIONS` depth, raised monotonically. Lightning has no unattributed funds: an invoice is settled once, for its amount.

---

## 8. Withdrawals

### 8.1 Owner at creation

| Caller | `projectId` |
|---|---|
| Merchant key | the key's project (no body field; a `project_id` in the body is refused with 422) |
| ADMIN | body `project_id`: a project UUID or the literal `"merchant"`, **required** (422 when missing). TRX/SOL must use `"merchant"`. |

### 8.2 Destination rules (address book scoped by owner)

The table of the [withdrawals spec §7.4](eth-and-withdrawals-spec.md#74-authorization-destination-policy-address-book-approval) applies, with "an active address-book entry" now meaning **an active entry with the same owner as the withdrawal**:
- a `BOOK` key of project P: entries of P only;
- an admin withdrawal for P at or above the approval threshold: entries of P;
- a merchant-funds withdrawal at or above the threshold: merchant entries.

`GET /api/v1/address-book` lists the active entries of the key's project.

### 8.3 Admission

These steps are added to `jobs::withdrawals::create_withdrawal`, inside `begin_withdrawal_admission` (the transaction that already holds `withdrawal-asset:{TOKEN}` for the token and the fee token).

1. All existing checks, unchanged: authorization, destination, asset limits, per-key limits, instance availability.
2. **Key withdrawals only**: compute the project's `available` for (network, token) on the admission connection.
   - Refuse when `available < amount + (token = feeToken ? feeCap : 0)`.
   - Error: `422` with code `project_insufficient_funds` and message `Insufficient project balance for this withdrawal`.
3. Insert with `projectId`.

- **Quote**: `POST /withdrawals/quote` with a key adds `project_available` and `project_available_raw`, and reports whether the project check would pass.
- **Approval**: `approve` re-runs the project check (excluding the withdrawal's own reservation, as the instance check does today).
- **Before signing**: `recheck_before_signing` does not re-check the project balance; the reservation already holds it.

### 8.4 Admin withdrawals attributed to a project

- No project check: an admin may exceed the balance, which then turns negative (red in the dashboard).
- The project's endpoints are notified (§10.1), and the payload's `requested_by` names the staff member.
- A bump of the fee cap (`/bump`) may push a project's real fee above its reservation; the actual fee is debited on confirmation regardless.

### 8.5 Merchant-API visibility

- `GET /withdrawals` and `GET /withdrawals/{id}` filter on the key's **project** (today: on the key).
- `POST /withdrawals/{id}/cancel` accepts any key of the project.
- `cancelledById` keeps the cancelling key's id.
- Staff filters: `?api_key_id` stays; `?project_id=<uuid>|merchant` is added.

### 8.6 Cold storage (Q28)

- **"Send excess to cold storage"** (Treasury page) pre-fills an admin withdrawal of `holdings − cap`. The dialog requires an owner like any admin withdrawal, and lists only the `isColdStorage` entries of that owner.
- **When no single project's available balance covers the excess**, the dialog shows each project's available balance and the admin sends several withdrawals.
- **Bringing funds back**: send them to the treasury address, which raises the merchant funds. Then make an internal transfer from the merchant funds to the project (§9).
- **Runbook**: every project that should use cold storage needs its own `isColdStorage` address-book entry, each with its own 24 h cooling-off.

### 8.7 Unchanged

The deprecated `POST /lightning/payments` alias creates a withdrawal of the key's project, with the project check. Per-key limits, idempotency, the freeze, the outbound engine and the legacy `payout_requests` counting all stay as they are.

---

## 9. Internal transfers

- **Who**: ADMIN with a fresh `totp_code`; `Idempotency-Key` required.
  - A replay by the same staff member with the same parameters returns the stored transfer (`200`).
  - Any other reuse answers `422`.
- **Body**: `from_project_id` and `to_project_id` (a UUID or `"merchant"`), `network`, `token`, `amount` (decimal string) or `amount_sats` (bitcoin/lightning), `note` (1–200 chars).
- **Rules**, in one transaction under `withdrawal-asset:{TOKEN}` (invariant 3):
  - (network, token) is a payment pair; `from ≠ to`; amount > 0.
  - From a project: its `available` ≥ amount.
  - From the merchant funds: `merchant_funds − amount ≥ 0`, with merchant funds computed from fresh holdings (§10.4, cache ≤ 30 s). If the holdings of that pair are `unknown`, answer `503` with `Holdings unavailable, retry later`.
- **Freeze**: transfers are allowed during the outflow freeze, because nothing moves on chain.
- **Effects**: one row in `internal_transfers`, the ledger cache invalidated, audit `transfer.created`, and `TRANSFER_COMPLETED` queued for the subscribed endpoints of **both** owners.
- **No correction**: a mistake is undone with the reverse transfer.
- **Rate limit**: new `TRANSFERS` limit, 10/min per staff member.

---

## 10. Webhooks

### 10.1 Owner rule (Q9, Q10, Q26)

| Event (payload `event_type`) | Subject | Owner | Endpoints that receive it |
|---|---|---|---|
| `payment_pending`, `deposit_confirmed`, `payment_expired` | deposit address / deposit | the address's project | that project's endpoints |
| `withdrawal_pending_approval` / `_broadcast` / `_confirmed` / `_failed` / `_cancelled` | withdrawal | its `projectId`, or the merchant | endpoints of that owner |
| `transfer_completed` | internal transfer | both sides | endpoints of `from` **and** of `to` |
| `gas_reserve_low`, `treasury_cap_exceeded`, `address_book_entry_added`, `withdrawal_stuck`, `ledger_shortfall` | alert | the merchant | merchant endpoints |
| `payout_succeeded`, `payout_failed` | Lightning payment | the merchant | merchant endpoints |

- **Selection**: `db::webhook_endpoints::active_webhook_endpoints_for_event` gains an owner argument and adds `AND "projectId" IS NOT DISTINCT FROM $owner` to the existing `"isActive" AND $event = ANY(events)`. `transfer_completed` calls it once per side and takes the union, deduplicated by endpoint.
- **Every fan-out path applies the rule**, retry paths included: the PAID fan-out, `notify_address_event`, `expire_addresses_and_notify`, `notify_payout`, `notify_withdrawal`, `alerts::raise`, the manual retries (`retrigger_*`), and the legacy bare-member fan-out of `webhook_loop`, which today uses all active endpoints.
- **`deliver_webhook`** re-checks, before every attempt, that the endpoint is active, subscribed and **owned by the subject's owner**. Otherwise it closes the pending row as `FAILED`, as today.
- **Before the first project exists** (§13), owner filtering is off and fan-out behaves exactly as today. A single helper, `jobs::projects::projects_exist(state)` (cached 10 s), gates it.

### 10.2 Allowed subscriptions

| Endpoint owner | Allowed `events` |
|---|---|
| project | `PENDING`, `PAID`, `EXPIRED`, `WITHDRAWAL_PENDING_APPROVAL`, `WITHDRAWAL_BROADCAST`, `WITHDRAWAL_CONFIRMED`, `WITHDRAWAL_FAILED`, `WITHDRAWAL_CANCELLED`, `TRANSFER_COMPLETED` |
| merchant | the five `WITHDRAWAL_*` above (merchant-funds withdrawals only), `TRANSFER_COMPLETED`, `PAYOUT_SUCCEEDED`, `PAYOUT_FAILED`, `GAS_RESERVE_LOW`, `TREASURY_CAP_EXCEEDED`, `ADDRESS_BOOK_ENTRY_ADDED`, `WITHDRAWAL_STUCK`, `LEDGER_SHORTFALL` |

Create and update refuse other events with `422`, implemented as a pure function `domain::allowed_events(owner_kind)`. The owner can never be changed after creation.

### 10.3 Payloads

- Payment payloads (`build_payload`, `build_address_payload`) gain `data.project_id` (UUID).
- Withdrawal payloads gain `data.project_id` (UUID, or `null` for merchant funds).
- These are additive fields only: no existing field changes, and the historical names (`deposit_id`, `deposit_confirmed`) stay.
- `transfer_completed`: `data` = `transfer_id`, `network`, `token`, `amount`, `amount_raw`, `from` (`{ "type": "project" | "merchant", "id": <uuid> | null }`), `to` (same shape), `note`, `created_at`.
- `ledger_shortfall` (alert payload): `details` = `{ holdings_raw, project_balances_raw, shortfall_raw }` for the alert's (network, token).
- New `WebhookSubject::Transfer` (`WebhookEventType::TransferCompleted.subject()`). Queue members are `"TRANSFER_COMPLETED|{transfer_id}|{endpoint_id}"`; the existing encoder and decoder handle UUID subjects.

### 10.4 Holdings, merchant funds, reconciliation, shortfall

- **Holdings** per (network, token) come from the figures the treasury view already computes:
  - confirmed treasury balance + unswept balance of deposit addresses;
  - Bitcoin: the treasury plus the scanned deposit UTXOs at depth;
  - Lightning: the SDK balance.
- **Merchant funds** = holdings − Σ project balances (all projects, signed).
- **Status**:
  - `ok` when merchant funds ≥ 0;
  - `shortfall` when < 0;
  - `unknown` when a holdings figure is unavailable: network or Lightning disabled, provider error, or sweeping disabled for the pair (the unswept balance is not scanned then).
- **Shortfall monitor**: the treasury monitor loop (every 5 min) evaluates each pair.
  - A per-pair Redis counter `ledger-shortfall:{NETWORK}:{TOKEN}` counts consecutive `shortfall` checks; `ok` and `unknown` reset it.
  - At 3, it raises `LEDGER_SHORTFALL` through `alerts::raise`, deduplicated per (type, network, token) like the other alerts.
  - It is logged at `warn` and delivered to merchant endpoints (§10.1).

---

## 11. Merchant API (`/api/v1`)

`CurrentApiKey` gains `project_id: Option<String>`. It is `None` only for a legacy key before the first project exists, which keeps today's unscoped behaviour (§13). Merchant handlers pass the key's project to scoped `db` functions (invariant 2).

| Method | Path | Change |
|---|---|---|
| POST | `/deposit-addresses` | Stores the key's `projectId`. The pending-address cap and rate limit stay per key. |
| GET | `/deposit-addresses` | The project's payments only. Items gain `project_id`. |
| GET | `/deposit-addresses/{id}` | 404 for another project's payment. |
| POST | `/deposits/{id}/webhooks/retry` | 404 for another project's deposit. |
| GET | `/project` | **New.** `{ id, name, color, key_tag, balances: [...] }`, one entry per payment (network, token) with `deposits`, `unattributed`, `withdrawn`, `fees`, `transfers_in`, `transfers_out`, `balance`, `reserved`, `available`, `pending` (each with `_raw`). A legacy key without a project answers `409 "This API key belongs to no project"`. |
| GET | `/transfers`, `/transfers/{id}` | **New**, read-only: the transfers where the project is `from` or `to` (filters `network`, `token`; paging as elsewhere). |
| POST | `/withdrawals`, `/withdrawals/quote` | Owner = the key's project; project check (§8.3); quote adds `project_available`. |
| GET / POST | `/withdrawals`, `/withdrawals/{id}`, `/withdrawals/{id}/cancel` | Scoped by project instead of key (§8.5). Responses gain `project_id`. |
| GET | `/address-book` | Active entries of the key's project. |
| POST | `/lightning/payments` (+ `/quote`) | Deprecated alias, unchanged, with the project check. |
| GET | `/balances`, `/wallet-balances` | **Removed from `/api/v1`** (404); still served on `/api/admin/v1`. |
| GET | `/lightning/payments`, `/lightning/payments/{id}` | **Removed from `/api/v1`** (404); still served on `/api/admin/v1`. |

**Removing routes from `/api/v1` without touching staff access.** Today one handler is mounted on both surfaces. The routes above are taken out of the merchant router only; their handlers stay on the admin router. Every handler kept on both surfaces branches explicitly on `CurrentApiKey` (scoped) versus `CurrentStaff` (unscoped plus optional filters), as `authorize_lightning_read` does today.

---

## 12. Admin API (`/api/admin/v1`)

**New routes**

| Method | Path | Access | Notes |
|---|---|---|---|
| GET | `/projects` | any staff | `{ projects: [{ id, name, color, key_tag, created_at, counts: { api_keys, deposit_addresses, withdrawals, webhook_endpoints, address_book_entries } }], unassigned }`. `unassigned` holds the counts of legacy rows while no project exists, and is `null` otherwise. |
| POST | `/projects` | ADMIN | `name`, optional `color` and `key_tag`, `attach_existing` (bool, default `true`, only accepted while no project exists — §13). `201` |
| PATCH | `/projects/{id}` | ADMIN | `name`, `color`, `key_tag` (`null` clears it) |
| DELETE | `/projects/{id}` | ADMIN | `409` when any key, payment, withdrawal, transfer, endpoint or address-book entry references it (deleted entries included), or when it is the last project |
| GET | `/projects/{id}/balance` | any staff | Same `balances` shape as `GET /api/v1/project` |
| GET | `/ledger` | any staff | Per payment (network, token): `holdings`, `projects: [{ project_id, balance, available }]`, `project_total`, `merchant_funds`, `status` (`ok` / `shortfall` / `unknown`), each amount with `_raw` |
| GET | `/transfers`, `/transfers/{id}` | any staff | Filters `project_id` (`<uuid>` or `merchant`, matched on either side), `network`, `token` |
| POST | `/transfers` | ADMIN + TOTP | §9 |

**Changes to existing routes**

| Route | Change |
|---|---|
| `POST /api-keys` | `project_id` required (UUID). Responses and lists gain `project_id`; the list accepts `?project_id`. |
| `POST /deposit-addresses` (staff) | `project_id` required (UUID). |
| `POST /webhook-endpoints` | `project_id` required: a UUID or `"merchant"`. Events validated per owner (§10.2). `PATCH` refuses `project_id`. |
| `POST /address-book` | `project_id` required: a UUID or `"merchant"`. |
| `POST /withdrawals` (admin) | `project_id` required: a UUID or `"merchant"` (§8.1). |
| List routes | `?project_id=<uuid>` filter on `/deposit-addresses`, `/withdrawals`, `/webhooks` (deliveries, through the endpoint's owner), `/stats`, `/stats/deposits-timeseries`, `/traffic/overview`, `/traffic/requests` (through the key's project; staff requests have none), `/api-keys`, `/webhook-endpoints`, `/address-book`. Where the merchant can be an owner (withdrawals, endpoints, deliveries, address book), `?project_id=merchant` selects merchant-owned rows. |
| Responses | `project_id` (UUID or `null`) on keys, payments, withdrawals, endpoints, address-book entries and deliveries. |

**Before the first project exists**, every route that creates an owned object answers `409 "Create a project first"`: `POST /api-keys`, `POST /deposit-addresses`, `POST /webhook-endpoints`, `POST /address-book`, admin `POST /withdrawals`, `POST /transfers`.

---

## 13. First run and existing data

**Flow**

1. `/setup` creates the first admin (unchanged).
2. After sign-in, the dashboard loads `GET /projects`.
   - **Empty list, ADMIN**: the blocking **"Create your first project"** step, shown in place of the app.
   - **Empty list, OPERATOR**: a waiting screen ("An admin must create the first project"), with Refresh and Sign out.
3. **Form**: name; color (pre-selected from the palette); optional key tag. When `unassigned` reports legacy rows, a checkbox "Attach existing data to this project (N keys, N payments, N withdrawals, N endpoints)" appears, checked by default.
4. **On success**: the new project becomes the selected one and the dashboard opens on `/`.

**Attach transaction**: `POST /projects` with `attach_existing = true` while no project exists. It runs in one transaction under `pg_advisory_xact_lock(hashtext('projects-first'))`, re-checking that no project exists (`409` otherwise):

- `api_keys`, `deposit_addresses` with `projectId IS NULL` → the new project.
- `withdrawals` with an `apiKeyId` → the new project. Withdrawals with a `staffId` stay NULL, which makes them merchant funds.
- `address_book_entries` (all NULL) → the new project. Merchant entries can be added afterwards.
- `webhook_endpoints`, classified with the pure function `domain::split_legacy_endpoint(events)`:
  - only project events (§10.2) → attached to the project;
  - only merchant events → stay merchant;
  - both → the row is attached to the project with the project events, and a **new merchant endpoint** is inserted with the same `url`, `secret`, `isActive` and the merchant events. `WITHDRAWAL_*` and `TRANSFER_COMPLETED` are kept on both, so the receiver still gets every withdrawal.
- Audit `project.created` with the attached counts.

**`attach_existing = false`**: the legacy rows become the merchant's.
- Legacy keys are **revoked** (`status = REVOKED`, audit `api_key.revoked` with reason `no_project`), so invariant 1 holds.
- Legacy payments and key withdrawals stay `NULL`, belong to no project, and count in the merchant funds.
- Legacy endpoints become merchant endpoints keeping only the merchant events; those left with no event are deactivated.
- Legacy address-book entries become merchant entries.

**Until the first project exists**, the instance runs as today:
- legacy keys keep working, unscoped;
- webhook fan-out is not owner-filtered (§10.1);
- the project check does not apply (no key has a project).

A fresh install (production) has no legacy rows and goes straight from `/setup` to the step.

---

## 14. Dashboard

### 14.1 Project scope

- **`ProjectScopeProvider`** (`src/lib/project-scope.tsx`), inside `AuthProvider`.
  - It loads the project list (query `["projects"]`) and holds the selection (`"all"` or a project id).
  - The selection is persisted in `localStorage["cpg.project"]`, read and written inside `try/catch`. An unknown id (for example a deleted project) falls back to `"all"`.
  - `useProjectScope()` returns `{ projects, selected, setSelected, scopeParam }`.
- **Every project-scoped query** includes the scope in its React Query key (`["payments", scope, filters]`) and sends `project_id` when a project is selected.
- **Selector**: in `SiteHeader`, left of the title. Entries are "All projects" and one per project, each with its color dot. On instance pages the selector is replaced by an **"Instance"** badge with a tooltip ("This page shows the whole instance"). Each route declares its scope.
- **`ProjectBadge`** (color dot + name; "Merchant" for merchant-owned rows) appears in every list and detail of an owned object. The 8 colors are CSS tokens defined for light and dark themes.

### 14.2 Page scopes

| Page | Scope | Notes |
|---|---|---|
| Overview `/` | project | §14.3 |
| Payments, payment detail | project | Create dialog: project select, required, pre-filled from the selector |
| Deliveries | project | "All" includes merchant endpoints' deliveries |
| Traffic | project | Selecting a project shows the merchant surface of its keys |
| Withdrawals, withdrawal detail | project | "All" includes merchant-funds withdrawals; create dialog §14.4 |
| Transfers (new, `/transfers`) | project | Transfers where the selected project is either side; create dialog (ADMIN, TOTP) |
| API keys | project | Create dialog: project required, pre-filled; the tag preview follows the project |
| Webhooks (endpoints) | project | "All" includes merchant endpoints. Create: owner select (projects + "Merchant — instance events"); event checkboxes follow §10.2 |
| Address book | project | "All" includes merchant entries; create: owner required |
| Wallets, Treasury, Lightning, Asset settings, Staff, Settings, Audit log, Account | instance | Treasury gains the Ledger card (§14.3) |
| Projects (new, `/projects`, Settings group) | instance | Any staff can view; ADMIN creates, edits (name, color, tag) and deletes (only when empty) |

### 14.3 Balances

- **Overview with a project selected**:
  - the existing stats, filtered (counts, deposits time series);
  - **project balance cards**, one per (network, token) with activity: `balance`, `available`, then a breakdown of deposits, unattributed, withdrawn, fees, transfers in/out, reserved and pending. Negative values show in red.
- **Overview with "All projects"**: one table per token, with a row per project (badge, balance, available) and a **Merchant funds** row taken from `/ledger`.
- **Treasury → Ledger card**: the `/ledger` rows (holdings, project total, merchant funds, status), with `shortfall` in red and `unknown` greyed. It carries a **New internal transfer** button (ADMIN).

### 14.4 Admin withdrawal dialog

- The **owner** is the first field and has no default: the project list, plus "Merchant funds (no project)". TRX/SOL force the merchant.
- The dialog shows the chosen owner's `available` balance (projects) or the merchant funds.
- Address-book choices are filtered by owner.
- "Send excess to cold storage" opens it pre-filled (§8.6).

### 14.5 First-project step

`RequireProject` wraps `AppLayout` inside `RequireAuth` and renders the step of §13. The step reuses the card layout of `SetupPage`.

---

## 15. Audit, rate limits, security

- **New audit actions**: `project.created` (name, color, key_tag, attached counts), `project.updated` (changed fields), `project.deleted`, `transfer.created` (network, token, amount_raw, from, to, note).
- **Existing actions**: those about an owned object add `project_id` (UUID or `null`) to `details`: `api_key.created`, `webhook_endpoint.created`, `address_book.created`, `withdrawal.requested`.
- **Rate limits**: new `TRANSFERS` (10/min per staff member). Project CRUD rides on the admin JWT guards; no dedicated limit.
- **Step-up TOTP**: internal transfers only. Project management needs none (Q19). Withdrawals and the address book keep their existing rules.
- **Security review points**:
  - invariant 2 (one scoped query per merchant read; 404 never 403, so a project cannot probe another's ids);
  - invariants 5 and 7 (no other project's data, and no custody figure, in merchant responses or project webhooks);
  - transfers append-only (trigger);
  - the project check inside the lock;
  - `ApiError` messages never name another project.

---

## 16. Migration plan

Single migration `20261017000000_projects.sql`, written so it cannot fail on existing data (AGENTS.md; guard style of `20261012000000`):

1. `ALTER TYPE "AlertType" ADD VALUE IF NOT EXISTS 'LEDGER_SHORTFALL'` (not used in this migration).
2. `CREATE TABLE "projects"` with its indexes (§6.1).
3. Owner columns, FKs and indexes (§6.2); the TRX/SOL CHECK on `withdrawals` (existing gas-token withdrawals are admin ones with `projectId` NULL, so it holds).
4. Replace the uniqueness indexes of `webhook_endpoints` and `address_book_entries` (§6.2). Existing rows are already unique under the stricter old index.
5. `CREATE TABLE "internal_transfers"` with its append-only trigger (§6.3).
6. `deposit_addresses."btcReceivedRaw"` (§6.4).
7. `webhook_deliveries."transferId"`, its FK, the five-column subject CHECK and the partial unique index (§6.5).
8. **No backfill**: attaching legacy rows is the application step of §13.

---

## 17. Testing

**Unit (no DB, no network)**, in `domain`, `jobs` and `http-api`:
- `ProjectBalance::from_components`: signs, negative balances, the same-asset fee rule (`token = feeToken` only), reserved with fee cap vs estimate, `available` below zero.
- Bitcoin unattributed: received − deposits, floored at 0, monotonic watermark.
- `allowed_events(owner_kind)` and the owner rule of every event type (an exhaustive `match` over `WebhookEventType`).
- `split_legacy_endpoint`: project-only, merchant-only and mixed subscriptions, with `WITHDRAWAL_*` and `TRANSFER_COMPLETED` on both sides.
- Transfer validation: same pair, `from ≠ to`, amount > 0, payment pairs only, idempotency params hash.
- Key-tag and name validation; palette auto-assignment.
- Shortfall counter: 3 consecutive `shortfall` → alert; `ok` / `unknown` reset.
- Merchant scope extraction: a key with a project → scoped; staff → unscoped.
- Request validation of the new bodies (`project_id` UUID or `"merchant"`, required where stated).

**Router tests**: `tower::ServiceExt::oneshot` against the merchant router (as the traffic tests do), asserting that the removed routes answer 404 on `/api/v1`.

**Local end-to-end** (`docker compose`, `CHAIN_MODE=testnet` where funds are needed):
1. Fresh DB: `/setup` leads to the first-project step.
2. Two projects with keys and endpoints:
   - a payment of A notifies A's endpoint only;
   - B's key gets 404 on A's payment.
3. A key withdrawal above A's balance is refused (`project_insufficient_funds`); an admin withdrawal for A succeeds and turns A negative.
4. Internal transfers B → A and A → merchant funds; `transfer_completed` reaches both sides.
5. Ledger card; a forced shortfall (merchant-funds withdrawal) raises the alert after three checks.
6. An instance with legacy data: attach, including the split of a mixed endpoint.

**Gates**: existing suites stay green; `cargo clippy --workspace --all-targets --locked -- -D warnings`; dashboard lint and build.

## 18. Implementation map (single release, suggested order)

1. **`domain`**:
   - `OwnerKind` and the owner rule per event, `WebhookSubject::Transfer`, new event and alert types;
   - `allowed_events`, `split_legacy_endpoint`, `ProjectBalance`;
   - validators (name, key tag, color).
2. **`config`**: palette, shortfall threshold (3), ledger cache TTL.
3. **Migration and `db`**:
   - `projects.rs` (CRUD, legacy attach in one transaction);
   - `transfers.rs`;
   - `ledger.rs` (components, holdings inputs);
   - owner columns in the row structs;
   - scoped variants of every merchant-surface query;
   - owner-aware `active_webhook_endpoints_for_event`;
   - `DISTINCT ON` keys with `"transferId"`.
4. **`jobs`**:
   - `projects.rs` (`projects_exist`, attach);
   - the project check in withdrawal admission and approval;
   - `transfers.rs`;
   - owner-filtered fan-out on every path;
   - the `transfer_completed` and `ledger_shortfall` payloads;
   - the Bitcoin `btcReceivedRaw` scan;
   - reconciliation and the shortfall monitor in `treasury.rs`.
5. **`http-api`**:
   - `CurrentApiKey.project_id`;
   - scoped merchant handlers;
   - routes removed from the merchant router;
   - `/project`, `/transfers`, `/projects`, `/ledger`;
   - `project_id` on creation bodies and filters;
   - the `TRANSFERS` rate limit.
6. **Dashboard**: `ProjectScopeProvider`, selector and badges, `RequireProject` and the first-project step, the Projects and Transfers pages, owner fields in the dialogs, balance cards, the Ledger card.
7. **Docs**:
   - README: projects, the first run, the `/api/v1` breaking changes, the webhook owner rule and payload fields, internal transfers, the cold-storage runbook (§8.6);
   - AGENTS.md: projects replace "No multi-tenancy"; invariants of §4; ownership model; the merchant-scoping rule; `transferId` in delivery keys; new events and alert;
   - CONTEXT.md: already updated.

## 19. Points to verify during implementation

- **Esplora "received at depth"**: `chain_stats.funded_txo_sum` counts outputs with one confirmation or more. Derive the amount at `BTC_CONFIRMATIONS` depth from the address's transactions (or subtract recent funding) and pin the behaviour with a fixture.
  - *Verified (implementation)*: `chain::esplora::received_at_depth` = `funded_txo_sum` − Σ outputs to the address of the confirmed transactions still short of the depth (read from the address's most recent page, which holds every transaction that recent); mempool transactions were never in `funded_txo_sum`. Fixture test `received_at_depth_subtracts_the_fundings_still_short_of_the_depth`. Written by the Bitcoin branch of `jobs::sweeper::plan_address`, raised only (`db.raise_btc_received`).
- **Native ETH sweeps**: check whether `sweeps.amountRaw` is net of the sweep fee. If so, unattributed ETH is under-counted by that fee (acceptable: a merchant cost); document it.
  - *Verified (implementation)*: it is **gross**. `plan_address` records the address balance as `amountRaw` and `unattributedRaw = amountRaw − credited`; `advance_ethereum` then sends `balance − gas` and records the gas on `sweeps.feeRaw`. So unattributed ETH is credited gross, and the sweep fee lands on the merchant funds (holdings − Σ project balances), exactly as Q6/§2 want. Nothing to change.
- **Ledger query performance**: check the aggregation on the dev dataset (`EXPLAIN`) with the §6.2 indexes, and add `deposits ("depositAddressId", status)` if the plan needs it.
  - *Verified (implementation)*: `EXPLAIN (ANALYZE)` of `db::ledger::COMPONENTS_SQL` on the dev dataset: 0.5 ms execution, 1.9 ms planning. The per-address correlated subquery of the Bitcoin branch uses `deposits_addr_txHash_key` (`"depositAddressId"` prefix); the other branches are sequential scans only because the tables are tiny. No extra index added.
- **Retry fan-out paths**: two paths read every active endpoint without the event and owner filter: `retrigger_failed_webhooks` (`db.active_webhook_endpoints()`) and the legacy bare-member fan-out in `process_queue_member`. Both must go through the owner-aware query, and so must `retrigger_webhook`, `retrigger_failed_deposit_webhooks` and `retrigger_delivery`.
  - *Verified (implementation)*: every one of these paths now resolves the subject's owner through `jobs::webhooks` (`endpoints_for_subject` for fan-outs, `check_endpoint` for single retries and before every delivery attempt). `retrigger_failed_webhooks` keeps `db.active_webhook_endpoints()` only as its "no endpoint at all" guard; its per-target eligibility is the owner-aware check. The bare legacy member fans out to the deposit owner's PAID endpoints.
- **Lightning deposits**: confirm that `deposits.amountRaw` holds sats on Lightning, as on Bitcoin.
  - *Verified (implementation)*: `jobs::lightning_transfer` stores `payment.amount_sats` as the transfer's `amount_raw`, so `deposits.amountRaw` is in sats on Lightning; the ledger sums it with the Bitcoin figures at the same scale.
- **Header selector**: check its layout at phone width (the header already holds the freeze control).
  - *Verified (implementation)*: the selector sits left of the title with a bounded width (`max-w-48`, `sm:max-w-64`, truncated name); below the `sm` breakpoint the page title and its separator are hidden so the row holds the sidebar trigger, the selector and the freeze control without wrapping.
