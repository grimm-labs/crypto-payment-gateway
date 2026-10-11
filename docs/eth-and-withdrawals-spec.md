# Spec — Native ETH deposits and withdrawals for every asset

| | |
|---|---|
| Status | Ready for implementation |
| Base | `main` @ `72599f1` |
| Glossary | [`CONTEXT.md`](../CONTEXT.md) — terms in **bold** below are defined there |
| Decisions | [ADR 0001](adr/0001-treasury-and-sweeping.md), [ADR 0002](adr/0002-treasury-derivation-paths.md), [ADR 0003](adr/0003-in-process-signing.md) |
| Delivery | One release (everything ships together), protected by the **outflow freeze** |

Read `AGENTS.md` first: every convention there (Prisma-style SQL, enum casings, amounts through `domain` helpers, `ApiError`, audit, rate limits, Redis keys, migrations that never fail on existing data, golden vectors) applies to this work. Where this spec says "as for X today", follow the existing code for X.

---

## 1. Goals

1. Accept **native ETH** payments on Ethereum mainnet (other EVM networks come later).
2. Let the gateway send funds out — **withdrawals** — for every asset it holds:

| Asset | Network | Withdrawal source | Who may withdraw |
|---|---|---|---|
| USDT | Ethereum, Tron, Solana | the network's **treasury** | admin, merchant key |
| ETH | Ethereum | Ethereum treasury | admin, merchant key |
| BTC | Bitcoin (on-chain) | deposit UTXOs + Bitcoin treasury | admin, merchant key |
| BTC | Lightning | Breez/Spark wallet | admin, merchant key |
| TRX, SOL (**gas assets**) | Tron, Solana | the network's treasury | admin only |

3. Keep funds manageable: confirmed deposits are **swept** into one treasury per network; **gas top-ups** let token deposits be swept.
4. Keep the hot wallet defensible: **withdrawal limits**, **address book**, **approval**, step-up TOTP, idempotency, audit, **outflow freeze**, hot-wallet cap alerts.

## 2. Out of scope

- EVM networks other than Ethereum mainnet (Arbitrum, Base, Polygon…) and ERC-20 tokens other than USDT.
- Accepting TRX or SOL as payments.
- Spark on-chain exits (cooperative exit) and Spark/USDB token sends: on-chain BTC leaves only from the Bitcoin treasury.
- Tron energy staking/delegation and rental markets (fees are paid by burning TRX).
- EIP-7702 sponsorship, smart-contract wallets, forwarders.
- A double-entry ledger (balances = chain + reservations, §7.5).
- Automatic transfers to cold storage (alert + guided withdrawal only, §7.10).
- Multi-merchant isolation (audit #7) — except that a merchant key only lists its own withdrawals.
- Reorg handling beyond confirmation thresholds (audit #12).
- External signer / KMS (the `Signer` interface prepares it, ADR 0003).

## 3. Decisions log

| # | Decision |
|---|---|
| Q1 | One concept, **Withdrawal**, for every outgoing transfer; the Lightning payout becomes a Lightning withdrawal. |
| Q2 | **Treasury + sweep** model ([ADR 0001](adr/0001-treasury-and-sweeping.md)). BTC is swept implicitly: withdrawals spend deposit UTXOs directly. |
| Q3 | Withdrawals by ADMIN staff (fresh TOTP) and by merchant keys holding an explicit withdrawal permission (expiring keys only). OPERATOR never withdraws. |
| Q4 | Asynchronous execution through a persisted state machine and workers. |
| Q5 | ETH deposits detected with `alchemy_getAssetTransfers` (`external` + `internal`); exact amount from `rawContract.value`; amount columns widened to `DECIMAL(38,18)`. |
| Q6 | Signing in-process, behind a `Signer` interface in `wallet` ([ADR 0003](adr/0003-in-process-signing.md)). |
| Q7 | Base guard rails + **address book** (24 h cooling-off) + **approval** above a threshold. |
| Q8 | Automatic sweep once deposits are confirmed, above a per-asset minimum. |
| Q9 | **Gas reserve** in each treasury, funded manually; automatic **gas top-ups**; TRX/SOL are gas assets (visible, admin-withdrawable, never payments). |
| Q10 | Network fees on top, paid by the gateway. Limits count the amount only. |
| Q11 | Treasuries on the same seed, reserved derivation paths ([ADR 0002](adr/0002-treasury-derivation-paths.md)). |
| Q12 | Hot-wallet cap per asset: alert + guided "send excess to cold storage" (a normal withdrawal). |
| Q13 | Lightning destinations: Lightning address + fixed-amount BOLT11. |
| Q14 | Merchant keys have a **destination policy**: book mode or open mode. |
| Q15 | Limits per asset (global) + per merchant key; approval threshold and hot-wallet cap per asset. |
| Q16 | Approval by another ADMIN (TOTP); single-admin instance: self-approval after 24 h. Pending approvals expire after 72 h. |
| Q17 | Address book is admin-only (TOTP) and shared by every key. |
| Q18 | Unified `/withdrawals` API; `POST /lightning/payments` kept as a deprecated alias; `canSend` migrated; new `withdrawal_*` webhooks; `payout_*` kept for wallet sends the gateway did not make. |
| Q19 | Automatic, capped fee bumping; `STUCK` + alert beyond the cap. Withdrawals are confirmed at the deposit thresholds. |
| Q20 | Available balance = confirmed treasury balance − reservations (row locks in DB). |
| Q21 | Webhooks: withdrawal lifecycle + operational alerts; sweeps are not notified. |
| Q22 | Single release; one global **outflow freeze**. |
| Q23 | Sweeps take every balance above the minimum (including **unattributed funds**), traced per source address. |
| Q24 | `CHAIN_MODE=testnet` for development (testnets end to end). |

## 4. Invariants (must hold at all times)

1. **Persist before broadcast.** Every outbound transaction is fully signed and stored (`outbound_transactions`, status `SIGNED`) before it is broadcast. A restart re-broadcasts the *same* bytes; it never signs a different transaction for the same purpose unless the previous one is provably invalid (§7.8).
2. **Sweeps only go to the treasury address computed locally** from the seed (never read from the DB, settings or a request).
3. **No outflow while frozen.** Workers check the freeze immediately before signing *and* immediately before broadcasting.
4. **One withdrawal, one effect.** Idempotency-Key + caller + parameters identify a withdrawal; a replay returns the stored withdrawal (as payouts do today).
5. **Reservations never exceed availability.** A withdrawal is admitted (status `QUEUED` or `PENDING_APPROVAL`) only inside a per-asset advisory lock that checks the available balance and every limit.
6. **Approval and destination rules are re-checked by the worker** right before signing, from the database, not trusted from the creation step.
7. **Destinations never point at the gateway itself** (deposit addresses, treasuries, their Solana token accounts).
8. Amounts are integers in base units (`NUMERIC(78,0)`) in every new table, with the decimal rendering derived through `domain` helpers. Never floats (Alchemy's `value` field is a float: never read it).
9. A **deposit** and a **withdrawal** are separate status machines; a sweep never changes a deposit's or a deposit address's status.

## 5. Asset and network matrix

| Asset | Decimals | Networks (payments) | Treasury path | Confirmed when | Fee paid in |
|---|---|---|---|---|---|
| USDT | 6 | tron, ethereum, solana | per network | Tron 19 · Ethereum 12 · Solana `finalized` | TRX · ETH · SOL |
| ETH | 18 | ethereum | `m/44'/60'/1'/0/0` | 12 | ETH |
| BTC | 8 | bitcoin, lightning | `m/84'/0'/1'/0/0` (on-chain) | 3 · settled | BTC · BTC |
| TRX | 6 (sun) | — (gas only) | `m/44'/60'/1'/0/0` (Tron encoding) | 19 | TRX |
| SOL | 9 (lamports) | — (gas only) | `m/44'/501'/0'/1'` | `finalized` | SOL |

Confirmation thresholds reuse `ETH_CONFIRMATIONS`, `TRON_CONFIRMATIONS`, `BTC_CONFIRMATIONS` (Q19).

---

## 6. Part A — Native ETH deposits

### 6.1 Domain (`packages/rust/domain`)

- `Token` gains `Eth`, `Trx`, `Sol` (DB `ETH`/`TRX`/`SOL`, API `ETH`/`TRX`/`SOL`); `decimals()`: ETH 18, TRX 6, SOL 9. Add constants `ETH_DECIMALS`, `TRX_DECIMALS`, `SOL_DECIMALS`.
- Replace the one-token-per-network assumption:
  - `Network::payment_tokens() -> &'static [Token]`: tron `[Usdt]`, ethereum `[Usdt, Eth]`, solana `[Usdt]`, bitcoin `[Btc]`, lightning `[Btc]`.
  - `Network::gas_token() -> Option<Token>`: ethereum `Eth`, tron `Trx`, solana `Sol`, bitcoin `Btc`, lightning `None`.
  - `Network::withdrawable_tokens()`: payment tokens + gas token (deduplicated).
  - `supports(network, token)` = `payment_tokens().contains(&token)` (payments only; TRX/SOL are refused for deposit addresses).
  - Remove `Network::token()`; fix every call site listed in the research (validation, balance sync, totals, fixtures, dashboard `networkToken`).
- `parse_api`/`DomainError` messages list every token.
- Tests: casings roundtrip, decimals, `supports` matrix (ETH on ethereum only; TRX/SOL never), raw ↔ decimal conversion of wei (1 wei, 10^18, 10^24 wei) without overflow.

### 6.2 Migration (part of the single migration, §10)

- `ALTER TYPE "Token" ADD VALUE IF NOT EXISTS 'ETH'`, `'TRX'`, `'SOL'` (not used in the same transaction — see the 20261010 migration comment).
- `deposit_addresses."expectedAmount"` and `deposits."amount"` → `DECIMAL(38,18)` (existing 8-decimal values stay exact).

### 6.3 Detection (`packages/rust/chain`)

- New `poll_ethereum_native(address) -> Vec<DetectedOnChainTransfer>`:
  - `alchemy_getAssetTransfers` with `toAddress`, `category: ["external","internal"]`, `fromBlock` = tip − `ETH_LOG_SCAN_BLOCKS` (same window as USDT), `excludeZeroValue: true`, `order: "asc"`, following `pageKey` until blank.
  - Amount = `rawContract.value` (hex → u128 wei). Reject a transfer whose `rawContract.value` is missing.
  - **Sum** every transfer sharing the same `hash` (one transaction can carry several internal transfers to the address) into one `DetectedOnChainTransfer` — like `esplora_transfers` does for BTC outputs.
  - Confirmations = `confirmations_between(tip, blockNum)`.
  - Through `chain/src/request.rs` only (URL-free errors; the clippy ban stays).
- The Ethereum monitor dispatches **per deposit address token**: `USDT` → existing `eth_getLogs` path, `ETH` → `poll_ethereum_native`.
- `fetch_address_balance(network, token, address)`: ETH → `eth_getBalance` at `latest`.
- Fixtures: a real `alchemy_getAssetTransfers` response (external + two internal transfers in one tx, plus pagination), hex parsing edge cases (`0x0`, missing field).

### 6.4 API

- `POST /deposit-addresses` accepts `{ "network": "ethereum", "token": "ETH", "expected_amount": "0.05" }` (decimal string; > 0; ≤ `ETH_MAX_EXPECTED_AMOUNT` = 100 000; rounded to 18 decimals). `expected_amount_sats` stays BTC-only.
- Responses: `token: "ETH"`, `expected_amount_raw` in wei.
- Webhook payloads unchanged in shape (`token: "ETH"`, `amount` rendered by `format_amount` at 18 decimals, `amount_raw` in wei).
- Tolerance (`AMOUNT_TOLERANCE_PERCENT`) and zero-amount rejection apply unchanged.

### 6.5 Balances

- `/balances` and `/wallet-balances`: `networks[]` becomes one entry **per (network, token)** — Ethereum appears twice (`USDT`, `ETH`). Totals gain `total_eth` (never mixed into USDT or BTC), and `total_trx` / `total_sol` for gas reserves (§7.10).
- After sweeping exists, these endpoints report **treasury + unswept** balances (§7.10), with the treasury broken out.

### 6.6 Dashboard

- Payment creation: token choice on Ethereum (USDT / ETH), decimal input with 18 decimals.
- Formatting helpers per token (replace `networkToken`).

---

## 7. Part B — Withdrawals

### 7.1 Treasury and Signer (`packages/rust/wallet`)

**Treasury paths** ([ADR 0002](adr/0002-treasury-derivation-paths.md)):

| Network | Path | Address |
|---|---|---|
| Ethereum | `m/44'/60'/1'/0/0` | EIP-55 |
| Tron | `m/44'/60'/1'/0/0` (same key as the Ethereum treasury, Tron encoding — as deposits share keys today) | base58check `T…` |
| Bitcoin | `m/84'/0'/1'/0/0` (also receives change) | `bc1q…` |
| Solana | `m/44'/501'/0'/1'` (owner; fee payer); its USDT ATA holds USDT | base58 |
| Lightning | — (the Spark wallet) | — |

No deposit index can ever produce these paths (deposits use account `0'`/change `0` for secp256k1 and change `0'` for Solana).

**Signer interface** (new module `wallet::signer`, [ADR 0003](adr/0003-in-process-signing.md)):

```rust
pub enum KeyRef { Deposit { network: Network, index: u32 }, Treasury(Network) }

pub trait Signer: Send + Sync {
    fn address(&self, key: KeyRef) -> Result<String, WalletError>;
    /// secp256k1 over a 32-byte digest, low-S, recovery id (ETH: keccak; Tron: sha256).
    fn sign_secp256k1_recoverable(&self, key: KeyRef, digest: [u8; 32]) -> Result<RecoverableSignature, WalletError>;
    /// BIP143 sighash → DER signature + compressed pubkey.
    fn sign_p2wpkh(&self, key: KeyRef, sighash: [u8; 32]) -> Result<P2wpkhWitness, WalletError>;
    /// ed25519 over the Solana message bytes (ed25519-dalek 2 — never solana_sdk::Keypair).
    fn sign_ed25519(&self, key: KeyRef, message: &[u8]) -> Result<[u8; 64], WalletError>;
}
```

- `WalletService` implements it; keys are derived per call and zeroized (as today).
- Nothing else in the workspace sees private keys. Transaction building lives in `chain`; orchestration in `jobs`.
- Golden tests: treasury addresses for the test mnemonic on every network (pinned byte for byte); signature vectors — EIP-1559 (a published vector), BIP143 P2WPKH (the BIP's native P2WPKH example), Tron (sha256 + recoverable signature checked by recovering the address), ed25519 (RFC 8032 test 1).
- `deny.toml`: reword the ed25519-dalek 1.x ignore — "the gateway never signs with ed25519-dalek 1.x (`solana_sdk::Keypair`); Solana signatures use ed25519-dalek 2".

### 7.2 Data model

All columns camelCase and quoted, enums PascalCase, amounts `NUMERIC(78,0)` raw + rendered on the fly (invariant 8).

**`withdrawals`**

| Column | Type | Notes |
|---|---|---|
| `id` | TEXT PK | UUID; also the SDK idempotency key for Lightning |
| `idempotencyKey` | TEXT UNIQUE | caller-provided UUID |
| `apiKeyId` / `staffId` | TEXT FK | exactly one (CHECK) |
| `paramsHash` | TEXT | SHA-256 of normalized network, token, destination, amount, comment |
| `network` | `"Network"` | |
| `token` | `"Token"` | must be withdrawable on the network |
| `destinationKind` | `"DestinationKind"` | `ADDRESS`, `LN_ADDRESS`, `BOLT11` |
| `destination` | TEXT | normalized (EIP-55, lowercase LN address…) |
| `addressBookEntryId` | TEXT FK NULL | set when the caller targeted a book entry |
| `amountRaw` | NUMERIC(78,0) | > 0 |
| `feeEstimateRaw` / `feeRaw` | NUMERIC(78,0) NULL | in `feeToken` |
| `feeToken` | `"Token"` | gas token of the network (BTC for bitcoin/lightning) |
| `status` | `"WithdrawalStatus"` | §7.3 |
| `approvalRequired` | BOOLEAN | |
| `approvedById` / `approvedAt` | TEXT / TIMESTAMP(3) NULL | |
| `selfApprovalNotBefore` | TIMESTAMP(3) NULL | single-admin case |
| `approvalExpiresAt` | TIMESTAMP(3) NULL | creation + 72 h |
| `cancelledById` / `cancelReason` | TEXT NULL | `rejected`, `expired`, `cancelled`, `freeze` is never a reason |
| `txHash` | TEXT NULL | current/final tx (Lightning: SDK payment id) |
| `confirmations` | INTEGER | |
| `failureReason` | TEXT NULL | client-safe code (`insufficient_funds`, `reverted`, `rejected_by_network`, `destination_invalid`…) |
| `reference` | TEXT NULL | merchant reference (≤ 128 chars) |
| `comment` | TEXT NULL | Lightning only |
| `metadata` | JSONB | ≤ 4 KB |
| `createdAt`, `updatedAt`, `broadcastAt`, `confirmedAt` | TIMESTAMP(3) | |

Indexes: `(status)`, `(apiKeyId, createdAt DESC)`, `(token, createdAt)` for the 24 h sums.

**`outbound_transactions`** — every transaction the gateway signs (withdrawals, sweeps, gas top-ups):

| Column | Notes |
|---|---|
| `id`, `purpose` (`"OutboundPurpose"`: `WITHDRAWAL`, `SWEEP`, `GAS_TOPUP`) | |
| `withdrawalId` / `sweepId` | FK, exactly one |
| `network`, `token`, `fromAddress`, `toAddress`, `amountRaw` | |
| `nonce` | BIGINT NULL (Ethereum) |
| `txHash`, `signedTx` (TEXT, hex/base64) | `signedTx` is what gets (re)broadcast |
| `feeParams` | JSONB (maxFee/priority/gasLimit, sat/vB, fee_limit, CU price…) |
| `feeRaw` | NUMERIC(78,0) NULL (actual, once known) |
| `status` (`"OutboundTxStatus"`) | `SIGNED`, `BROADCAST`, `CONFIRMED`, `FAILED`, `REPLACED`, `EXPIRED` |
| `replacesId` | FK NULL (fee bump chain) |
| `expiresAt` | Tron/Solana validity end |
| `blockNumber`, `confirmations`, `lastError`, timestamps | |

**`sweeps`**: `id`, `network`, `token`, `depositAddressId`, `fromAddress`, `amountRaw`, `creditedRaw` (sum of the address's confirmed deposits not yet swept), `unattributedRaw` (= `amountRaw − creditedRaw`, ≥ 0), `status` (`"SweepStatus"`: `PLANNED`, `TOPUP_SENT`, `READY`, `BROADCAST`, `CONFIRMED`, `FAILED`, `SKIPPED`), `lastError`, timestamps.

**`address_book_entries`**: `id`, `network`, `destinationKind` (`ADDRESS` / `LN_ADDRESS`), `destination` (normalized), `label`, `isColdStorage` BOOLEAN, `createdById`, `activeAt` (= creation + 24 h), `deletedAt`, `deletedById`. Partial UNIQUE `(network, destination) WHERE "deletedAt" IS NULL`.

**`asset_settings`** (one row per token): `token` PK, `maxPerWithdrawalRaw`, `dailyMaxRaw`, `approvalThresholdRaw`, `hotWalletCapRaw` (NULL = none), `sweepMinimumRaw` (per network, see below), `gasReserveFloorRaw` and `gasAlertBelowRaw` (gas assets only), `updatedAt`. Sweep minimums are per (network, token): store them in **`sweep_settings`** (`network`, `token`, `minimumRaw`, `enabled`).

**`api_keys`**: add `withdrawalPolicy` (`"WithdrawalPolicy"`: `NONE`, `BOOK`, `OPEN`; default `NONE`), `withdrawalTokens` TEXT[] (subset of USDT, BTC, ETH) and `withdrawalNetworks` TEXT[] NULL (NULL = every network of the allowed tokens). CHECK `"withdrawalPolicy" = 'NONE' OR "expiresAt" IS NOT NULL`. Drop `canSend` after migrating it (§10).

**`api_key_withdrawal_limits`**: `apiKeyId`, `token`, `maxPerWithdrawalRaw`, `dailyMaxRaw` (PK `apiKeyId, token`). Required for every token of an `OPEN` key.

**`btc_utxo_reservations`**: `txid`, `vout` (PK), `outboundTxId` FK — UTXOs spent by an unconfirmed gateway transaction.

**`signer_nonces`**: `network`, `address` (PK), `nextNonce` BIGINT — Ethereum nonce allocation per sending address.

**`alerts`**: `id`, `type` (`"AlertType"`: `GAS_RESERVE_LOW`, `TREASURY_CAP_EXCEEDED`, `ADDRESS_BOOK_ENTRY_ADDED`, `WITHDRAWAL_STUCK`), `network`, `token`, `details` JSONB, `createdAt`. Dedup: one alert per (type, network, token) per 24 h while the condition holds.

**`webhook_deliveries`**: add `withdrawalId` and `alertId`; the subject CHECK becomes "exactly one of `depositAddressId`, `lightningPaymentId`, `withdrawalId`, `alertId`"; add partial unique indexes for `(withdrawalId, eventType, webhookEndpointId)` and `(alertId, eventType, webhookEndpointId)` (guarded like the 20261012 migration); **every** `DISTINCT ON` key must include the new columns (AGENTS.md rule).

`payout_requests` stays as read-only history (not migrated, §7.14).

### 7.3 Withdrawal lifecycle

```
                 ┌──────────────► CANCELLED (rejected / expired / cancelled)
                 │                    ▲
create ──► PENDING_APPROVAL ──approve──┤
   │                                   ▼
   └────────────────────────────────► QUEUED ──cancel──► CANCELLED
                                        │
                                  worker signs
                                        ▼
                                     SIGNED ──► BROADCAST ──► CONFIRMED
                                        │           │  ▲
                                        │           ▼  │ fee bump (new outbound tx)
                                        │         STUCK ──admin bump──┘
                                        ▼           │
                                     FAILED ◄───────┘ (reverted, provably dropped, admin abandon)
```

- `create`: validation, authorization, destination rules, limits and balance checks inside one transaction under the asset's advisory lock (invariant 5) → `PENDING_APPROVAL` (if approval required) or `QUEUED`. Both reserve funds.
- Cancel: requester (same key/staff) or any ADMIN (TOTP), only in `PENDING_APPROVAL` / `QUEUED`.
- Lightning skips `SIGNED`: `QUEUED → BROADCAST` (SDK accepted, `txHash` = payment id) → `CONFIRMED` (`PaymentSucceeded`) / `FAILED` (`PaymentFailed`).
- `FAILED` before broadcast releases the reservation; `FAILED` after broadcast records the fee actually spent.
- `STUCK`: the fee cap is reached (§7.8) or a Tron/Solana tx cannot be proven expired after the expected window. Admin actions on `STUCK`: *bump* (new fee cap ×2, TOTP) or *abandon* (only once the network proves the tx can no longer land: replaced nonce mined, inputs spent elsewhere, blockhash/TAPOS expired).
- Every transition writes `updatedAt`, an audit entry (§7.17) and, where listed in §7.12, a webhook.

### 7.4 Authorization, destination policy, address book, approval

**Who** (Q3): ADMIN staff with a fresh `totp_code`; a merchant key with `withdrawalPolicy ≠ NONE` and the token in `withdrawalTokens`. OPERATOR → 403. TRX/SOL → ADMIN only.

**Destination rules**

| Caller | Below approval threshold | At/above approval threshold |
|---|---|---|
| ADMIN | any valid destination | an **active** address-book entry, + approval |
| Key `BOOK` | an active address-book entry | an active address-book entry, + approval |
| Key `OPEN` | any valid destination, within the key's own limits | any valid destination, within the key's limits, + approval |

- BOLT11 invoices can never be book entries: they are only possible where "any valid destination" applies.
- A book entry is *active* when `activeAt ≤ now` and not deleted; the worker re-checks it before signing (invariant 6).

**Destination validation** (pure functions in `domain` or `chain`, unit-tested):

| Network | Accepted | Refused |
|---|---|---|
| ethereum | 20-byte hex; lowercase or valid EIP-55 checksum | bad checksum, zero address, the USDT contract, gateway addresses |
| tron | base58check, `0x41` prefix | other prefixes, gateway addresses |
| solana | a base58 32-byte owner address (the USDT ATA is derived and created if missing); an existing USDT token account is used as-is | other mints' token accounts, gateway owners/ATAs |
| bitcoin | any **mainnet** address type (P2WPKH, P2WSH, P2TR, P2SH, P2PKH) with amount ≥ the type's dust limit | other networks, gateway addresses |
| lightning | `user@domain` (current rules + public DNS check); BOLT11 mainnet invoice with an amount equal to the requested amount, not expired | amountless or mismatched invoices, LNURL strings |

**Address book**: list (any staff; book-mode keys list active entries), create (ADMIN + TOTP; fires `ADDRESS_BOOK_ENTRY_ADDED`), delete (ADMIN + TOTP, immediate). Entries are network-wide (any withdrawable token of that network). No update — delete and re-create (restarts the 24 h).

**Approval** (Q16):
- `approvalRequired = amount ≥ asset_settings.approvalThresholdRaw` (any caller).
- Approve/reject: an ADMIN other than the requesting staff member, with TOTP. Merchant-requested withdrawals may be approved by any ADMIN.
- Single active ADMIN at creation time: `selfApprovalNotBefore = createdAt + 24 h`; the requester may approve (TOTP) after it.
- `approvalExpiresAt = createdAt + 72 h`: an expiry loop cancels (`expired`) and releases the reservation.
- Approval re-runs the balance and limit checks under the lock (the reservation already exists; it is re-validated, not double-counted).

### 7.5 Limits, reservations, available balance

- **Global per asset** (`asset_settings`): per withdrawal, rolling 24 h. BTC limits cover bitcoin + lightning together; USDT limits cover all three networks together (Q15).
- **Per key** (`api_key_withdrawal_limits`): per withdrawal and rolling 24 h, per token. Mandatory for `OPEN` keys; optional for `BOOK` keys.
- 24 h sums count withdrawals in `PENDING_APPROVAL`, `QUEUED`, `SIGNED`, `BROADCAST`, `STUCK`, `CONFIRMED` (by `createdAt`), plus legacy `payout_requests` PENDING/SENT during the 24 h after deploy.
- **Available (per network, token)** = confirmed spendable balance − Σ reserved amounts (withdrawals in `PENDING_APPROVAL…STUCK`) − (for the gas token) `gasReserveFloorRaw` − Σ estimated fees of reserved withdrawals paid in that token.
  - Treasury balances are read from the chain at the confirmation depth (ETH/USDT via `eth_getBalance`/`balanceOf` at `tip − confirmations`; Tron via `walletsolidity`; Solana with `finalized`; BTC = confirmed UTXOs of the treasury **and** of final deposit addresses (`PAID`/`EXPIRED`), minus `btc_utxo_reservations`; Lightning = SDK balance).
  - Cached per (network, token) for 30 s in Redis; the admission check re-reads when the cached figure is insufficient.
- Admission fails with `422` and an explicit message (`insufficient_funds`, `limit_exceeded`, `key_limit_exceeded`, `approval_threshold_requires_address_book`…).
- A `0` limit disables withdrawals of that asset (as payouts today).

### 7.6 Fees and quotes

- Fees are on top (Q10), paid from the network's gas token (BTC for bitcoin/lightning).
- `POST /withdrawals/quote` returns the fee estimate, the fee token, whether approval is required, whether the destination is acceptable for this caller, and the available balance. Same validation as creation, no reservation.
- The fee cap of a withdrawal = 3 × the quote at creation (§7.8). The actual fee is recorded on confirmation.
- Gas top-ups and sweep fees are gateway costs: recorded on `sweeps`/`outbound_transactions`, shown in the dashboard, never charged to a withdrawal.

### 7.7 Execution per network

**Bitcoin (on-chain)**
- Inputs: confirmed UTXOs (`BTC_CONFIRMATIONS`) of the treasury and of deposit addresses whose status is final (`PAID`/`EXPIRED`), excluding reserved ones. Coin selection: treasury change first, then oldest deposit UTXOs; skip UTXOs worth less than 3× their own input cost at the current fee rate; at most 200 inputs.
- Outputs: destination + change to the treasury (`m/84'/0'/1'/0/0`) unless change < dust (then it goes to fees).
- Fee rate: Esplora `/fee-estimates` target 3 blocks (bounded by a min of 1 sat/vB and a max setting); size from `predict_weight`.
- `nSequence = ENABLE_RBF_NO_LOCKTIME`; each input signed with its own key (`KeyRef::Deposit{bitcoin,i}` or the treasury) over its BIP143 sighash (needs each input's amount).
- Broadcast `POST /tx`; reserve inputs in `btc_utxo_reservations` in the same DB transaction that stores the signed tx.

**Ethereum — ETH**
- From the treasury: EIP-1559 type 2, chain id 1 (testnet mode: 11155111), gas 21 000, `maxPriorityFeePerGas` from `eth_maxPriorityFeePerGas`, `maxFeePerGas = 2 × baseFee + priority` (base fee from `eth_feeHistory`).
- Nonce from `signer_nonces` (§7.8). Encode/sign with `alloy-consensus` + `alloy-signer-local` (already in the lock via breez; add as direct workspace deps at the locked versions) or hand-rolled with `alloy-rlp` + `k256`; signature through `Signer`.
- Confirmed when the receipt has `status = 1` and 12 confirmations.

**Ethereum — USDT**
- `transfer(address,uint256)` to the USDT contract, ABI-encoded with `alloy-sol-types`. USDT returns **no bool**: success = receipt `status = 1` only. `gasLimit = eth_estimateGas × 1.3`.
- A revert (blacklisted destination, paused contract) → `FAILED` (`reverted`), fee recorded.

**Tron — USDT and TRX**
- USDT: `wallet/triggersmartcontract` (owner = treasury, `transfer(address,uint256)`, `fee_limit` = estimated energy × energy price × 1.5, capped by a setting) → **decode `raw_data_hex`** (hand-written `prost` messages for `Transaction.raw` and `TriggerSmartContract`) and assert owner, contract, selector, recipient and amount before signing; `txID = sha256(raw_data)`; sign with `Signer::sign_secp256k1_recoverable` (65 bytes `r‖s‖v`); `wallet/broadcasttransaction`.
- TRX (admin): `wallet/createtransaction`, same decode-and-verify rule for `TransferContract`.
- Energy price and fees from `wallet/getchainparameters` (never hard-coded); estimate with `wallet/estimateenergy` (or `triggerconstantcontract`).
- Validity: TAPOS expiration (~60 s) → `expiresAt`. Confirmed via `walletsolidity/gettransactioninfobyid` (`receipt.result = SUCCESS`) and 19 confirmations.

**Solana — USDT and SOL**
- USDT: instructions = compute budget (limit, price from Helius `getPriorityFeeEstimate`, medium) + `create_associated_token_account_idempotent` (payer = treasury owner) when the destination ATA does not exist + `transfer_checked` (treasury ATA → destination ATA, mint USDT, 6 decimals). Fee payer = treasury owner. Built with `solana-sdk` 2.3 / `spl-token` 7; **signatures inserted by hand** from `Signer::sign_ed25519(message_data)`.
- SOL (admin): system transfer from the treasury owner.
- Validity: recent blockhash (~150 slots) → `expiresAt`; confirmed when `getSignatureStatuses` reports `finalized` with `err: null`.

**Lightning**
- `LightningOps` gains `prepare_send_payment` / `send_payment` (BOLT11); Lightning addresses keep the LNURL path. SDK `idempotency_key` = `withdrawals.id`.
- `BOLT11`: the prepared invoice amount must equal `amountRaw`; fees from the prepare response (`lightning_fee_sats` + `spark_transfer_fee_sats`) must stay ≤ the fee cap.
- `CONFIRMED` on `PaymentSucceeded` (event or reconciliation via `get_payment`), `FAILED` on `PaymentFailed`. No confirmations.

### 7.8 Outbound transaction engine (`jobs`)

- **Claiming**: workers claim withdrawals/sweeps with `SELECT … FOR UPDATE SKIP LOCKED` and hold a per-sending-address advisory lock (`hashtext(network || address)`) while allocating nonces/UTXOs and signing. Safe with two containers during a Kamal overlap.
- **Persist before broadcast** (invariant 1): build → sign → insert `outbound_transactions` (`SIGNED`) + move the withdrawal/sweep to `SIGNED` in one DB transaction → check the freeze again → broadcast → `BROADCAST`. A crash between the two re-broadcasts the stored `signedTx` at boot (`SIGNED` rows older than 30 s).
- **Ethereum nonces**: `signer_nonces.nextNonce` per address, allocated under the address lock; initialized/resynced from `eth_getTransactionCount(pending)` whenever the address has no `SIGNED`/`BROADCAST` tx; at most 8 in-flight txs per address.
- **Replacement (Q19)**, after a stuck window per network (ETH 5 blocks, BTC 3 blocks, Lightning n/a):
  - ETH: same nonce, fees +25 % (≥ the 10 % replacement minimum), new `outbound_transactions` row with `replacesId`; the old one becomes `REPLACED` once the new one is mined.
  - BTC: RBF with the same inputs (BIP125: higher absolute fee + incremental relay fee).
  - Tron/Solana: never re-signed until the previous tx is **provably expired** (TAPOS expiration / blockhash no longer valid **and** the tx absent with `searchTransactionHistory`), then a fresh tx with a fresh fee estimate.
  - Stop at the fee cap (3 × quote) → `STUCK` + `WITHDRAWAL_STUCK` alert.
- **Confirmation tracking**: one loop per network re-reads `BROADCAST` txs (receipts, Esplora status, `gettransactioninfobyid`, `getSignatureStatuses`) and moves them to `CONFIRMED`/`FAILED`, updating the withdrawal/sweep and releasing BTC UTXO reservations on failure.
- **Errors** from providers are logged (URL-free) and stored as generic codes on the withdrawal; SDK/provider details never reach API responses.

### 7.9 Sweeping

- **Loop** per network (every 5 min; never while frozen; disabled per (network, token) via `sweep_settings.enabled`):
  - Candidates: deposit addresses of the network with a confirmed deposit not yet swept, plus a **daily full rescan** of every deposit address of the network (late payments, unattributed funds), at `ADDRESS_REQUEST_DELAY_MS` pacing.
  - Skip addresses that are still `PENDING` (the payment may complete) or have a `DETECTED` deposit.
  - Read the confirmed balance per token; sweep when ≥ `sweep_settings.minimumRaw`.
  - `creditedRaw` = sum of the address's `CONFIRMED` deposits created since its last sweep; `unattributedRaw = amount − creditedRaw` (≥ 0) — **unattributed funds** are flagged in the dashboard (Q23).
- **Per network**:
  - **ETH**: from the deposit address itself; `value = balance − 21 000 × maxFeePerGas` (the refund leaves dust on the address — expected, below the minimum).
  - **USDT-ERC20**: if the deposit address's ETH < `estimateGas × maxFeePerGas × 1.2`, a **gas top-up** (`GAS_TOPUP`, treasury → deposit, exact shortfall) first; once it has 1 confirmation the sweep tx is signed with the deposit key (`TOPUP_SENT → READY → BROADCAST`).
  - **USDT-TRC20**: top-up in TRX = (activation if the account does not exist) + estimated energy burn × 1.2 + bandwidth; then the deposit address signs `transfer` to the treasury (same decode-and-verify rule). TRX dust stays on the deposit address.
  - **USDT-SPL**: `transfer_checked` deposit ATA → treasury ATA, fee payer = treasury owner (no top-up), then `close_account` on the deposit ATA (rent back to the treasury owner). A later payment to a closed ATA fails on the sender side or re-creates it; the daily rescan sweeps it again.
  - **BTC**: no sweep (spent at withdrawal time).
- Sweeps and top-ups are not notified by webhook (Q21); they are listed in the dashboard and audited as `SYSTEM`.

### 7.10 Treasury monitoring, gas reserve, hot-wallet cap

- Every 5 min per network: treasury confirmed balances, unswept balances (sum of candidate addresses known from the last scan), gas reserve status.
- `GAS_RESERVE_LOW` when a gas token's treasury balance < `gasAlertBelowRaw`. Below `gasReserveFloorRaw`, top-ups (and withdrawals of the gas token) stop for that network; sweeps wait.
- `TREASURY_CAP_EXCEEDED` when treasury (+ for BTC, all spendable UTXOs) > `hotWalletCapRaw`; the dashboard offers "Send excess to cold storage": pre-fills a withdrawal of `balance − cap` to an address-book entry flagged `isColdStorage` (normal rules: TOTP, approval if above threshold).
- Alerts are deduplicated (§7.2 `alerts`) and also logged at `warn`.

### 7.11 Outflow freeze

- `app_settings.outflowsFrozen` (`true`/`false`) + env `OUTFLOWS_FROZEN=true` (forces the freeze at boot; the setting cannot lift it while the env var is set).
- **Freeze**: any staff member (ADMIN or OPERATOR), no TOTP — `POST /api/admin/v1/outflows/freeze` (audited, alert-level log). **Unfreeze**: ADMIN + TOTP.
- While frozen: new withdrawals → `503 "Outflows are frozen"`; `QUEUED` withdrawals wait; workers sign nothing and broadcast nothing new (re-broadcasting an already `BROADCAST` tx is allowed — it is already public); sweeps and top-ups stop; quotes still work; approvals/cancellations still work.

### 7.12 Webhooks

New `WebhookEventType` values (DB / API / payload `event_type`):

| DB | API | Payload `event_type` | Subject |
|---|---|---|---|
| `WITHDRAWAL_PENDING_APPROVAL` | `withdrawal_pending_approval` | same | withdrawal |
| `WITHDRAWAL_BROADCAST` | `withdrawal_broadcast` | same | withdrawal |
| `WITHDRAWAL_CONFIRMED` | `withdrawal_confirmed` | same | withdrawal |
| `WITHDRAWAL_FAILED` | `withdrawal_failed` | same | withdrawal |
| `WITHDRAWAL_CANCELLED` | `withdrawal_cancelled` | same | withdrawal |
| `GAS_RESERVE_LOW` | `gas_reserve_low` | same | alert |
| `TREASURY_CAP_EXCEEDED` | `treasury_cap_exceeded` | same | alert |
| `ADDRESS_BOOK_ENTRY_ADDED` | `address_book_entry_added` | same | alert |
| `WITHDRAWAL_STUCK` | `withdrawal_stuck` | same | alert |

- Queue members `"{EVENT}|{subject_id}|{endpoint_id}"` (UUID subjects) — the existing encoder/decoder handles them once the event list grows.
- Withdrawal payload `data`: `withdrawal_id`, `status`, `network`, `token`, `amount`, `amount_raw`, `fee`, `fee_raw`, `fee_token`, `destination`, `address_book_entry_id`, `tx_hash`, `confirmations`, `reference`, `requested_by` (`{type: "api_key"|"staff", id}`), `failure_reason`. Envelope: `event_id`, `occurred_at` as today, V2 signature.
- Alert payload `data`: `alert_id`, `type`, `network`, `token`, `details`.
- `payout_succeeded` / `payout_failed`: still emitted for finished sends of the Lightning wallet **not** created by a gateway withdrawal (`notify_payout` skips payments whose id matches a `withdrawals.id` or a legacy `payout_requests.paymentId`).

### 7.13 API

All mutating calls require `Idempotency-Key` where noted, return `ApiError` JSON, are rate-limited (`WITHDRAWALS`: 10/min per caller for create + quote) and audited.

**Merchant `/api/v1`** (key with `withdrawalPolicy ≠ NONE`; otherwise 403):

| Method | Path | Notes |
|---|---|---|
| POST | `/withdrawals` | `Idempotency-Key` required. Body: `network`, `token`, `amount` (decimal string) or `amount_sats` (bitcoin/lightning), exactly one of `destination` / `address_book_entry_id`, optional `reference`, `comment` (lightning), `metadata`. `202` new, `200` replay, `409` in progress, `422` key reused/limits/funds/destination rule, `503` frozen or network disabled. |
| POST | `/withdrawals/quote` | Same body without the key; no reservation. |
| GET | `/withdrawals` | Only this key's withdrawals; filters `status`, `network`, `token`; paging as elsewhere. |
| GET | `/withdrawals/{id}` | 404 for another key's withdrawal. |
| POST | `/withdrawals/{id}/cancel` | Own withdrawal, `PENDING_APPROVAL`/`QUEUED` only. |
| GET | `/address-book` | `BOOK` keys: active entries (id, network, destination, label). |
| POST | `/lightning/payments` (+ `/quote`) | **Deprecated alias**: creates a Lightning withdrawal; old response shape plus `withdrawal_id`; `Deprecation` + `Sunset` headers. Removal in the next release. |

**Admin `/api/admin/v1`**:

| Method | Path | Access |
|---|---|---|
| GET | `/withdrawals`, `/withdrawals/{id}` | any staff |
| POST | `/withdrawals`, `/withdrawals/quote` | ADMIN (+ `totp_code` on create) |
| POST | `/withdrawals/{id}/approve`, `/reject` | ADMIN + TOTP (§7.4 rules) |
| POST | `/withdrawals/{id}/cancel`, `/bump`, `/abandon` | ADMIN + TOTP |
| GET / POST / DELETE | `/address-book`, `/address-book/{id}` | list: any staff; create/delete: ADMIN + TOTP |
| GET | `/treasury` | any staff — per (network, token): treasury address, confirmed, reserved, available, unswept, unattributed, gas reserve, cap status |
| GET | `/sweeps` | any staff |
| POST | `/sweeps/run` | ADMIN + TOTP (`network` optional) |
| POST | `/outflows/freeze` · `/outflows/unfreeze` | any staff · ADMIN + TOTP |
| GET / PATCH | `/settings/assets` | GET any staff; PATCH ADMIN + TOTP (limits, threshold, cap, gas floor/alert, sweep minimums/enabled) |
| PATCH | `/api-keys/{id}` | gains `withdrawal_policy`, `withdrawal_tokens`, `withdrawal_limits` (TOTP when granting or raising) |

Response shape (`Withdrawal`): `id`, `status`, `network`, `token`, `amount`, `amount_raw`, `fee_estimate`, `fee`, `fee_token`, `destination`, `destination_kind`, `address_book_entry_id`, `approval_required`, `approved_by`, `approval_expires_at`, `tx_hash`, `confirmations`, `failure_reason`, `reference`, `metadata`, `requested_by`, `created_at`, `broadcast_at`, `confirmed_at`.

### 7.14 Lightning migration

- Existing `canSend = true` keys → `withdrawalPolicy = OPEN`, `withdrawalTokens = {BTC}`, `withdrawalNetworks = {lightning}` (BTC covers two networks: this keeps the old Lightning-only scope), per-key BTC limits = the current `payoutMaxSats` / `payoutDailyMaxSats` values. Their Lightning payouts keep working through the alias; on-chain BTC needs an admin to widen `withdrawalNetworks`.
- `app_settings.payoutMaxSats` / `payoutDailyMaxSats` → seed the BTC row of `asset_settings` (explicit values carried over; otherwise the defaults of §7.16). The old setting rows are removed.
- `payout_requests` stays for history (read in the dashboard's legacy list); new sends never write it.
- `LightningPage` payout dialog becomes the generic withdrawal dialog (network lightning).

### 7.15 Dashboard

- **Withdrawals** page: list (filters), detail (timeline of states and outbound txs, explorer links), create dialog (network → token → destination or book entry → amount → quote review → TOTP), approve/reject/cancel/bump/abandon actions with TOTP, pending-approval badge in the sidebar.
- **Address book** page: list, add (24 h countdown shown), delete; cold-storage flag.
- **Treasury** page: per network/token balances (treasury, reserved, available, unswept, unattributed), gas reserves with thresholds, cap status with "send excess to cold storage", sweeps history, manual sweep, prominent **freeze** button and frozen banner across the app.
- **Settings → Assets**: per-asset limits, approval threshold, cap, gas floor/alert, sweep minimums/toggles (TOTP on save).
- **API keys**: withdrawal policy, tokens, networks, per-key limits.
- **Payments**: ETH option; balances per (network, token).
- Operators: read-only on everything above except the freeze button.

### 7.16 Settings defaults (proposals — adjust before release)

| Token | Max per withdrawal | Rolling 24 h | Approval at | Hot-wallet cap | Gas floor / alert |
|---|---|---|---|---|---|
| USDT | 10 000 | 50 000 | 5 000 | 100 000 | — |
| ETH | 5 | 20 | 2 | 50 | 0.05 / 0.1 ETH |
| BTC | 0.01 (1 000 000 sats, current payout default) | 0.1 (current) | 0.005 | 1 | — |
| TRX | 10 000 | 50 000 | 5 000 | — | 50 / 200 TRX |
| SOL | 50 | 200 | 20 | — | 0.1 / 0.5 SOL |

Sweep minimums: ETH 0.01 · USDT-ERC20 50 · USDT-TRC20 20 · USDT-SPL 1. Fee cap = 3 × quote. Approval expiry 72 h, self-approval delay 24 h, address-book cooling-off 24 h (constants in `config`).

### 7.17 Audit, rate limits, security requirements

- **Audit actions**: `withdrawal.requested`, `withdrawal.approved`, `withdrawal.rejected`, `withdrawal.cancelled`, `withdrawal.signed`, `withdrawal.broadcast`, `withdrawal.confirmed`, `withdrawal.failed`, `withdrawal.stuck`, `withdrawal.bumped`, `withdrawal.abandoned`, `address_book.created`, `address_book.deleted`, `sweep.broadcast`, `sweep.confirmed`, `gas_topup.broadcast`, `outflows.frozen`, `outflows.unfrozen`, `settings.assets_updated`, `api_key.withdrawal_policy_updated`. Details never contain keys, signed txs or TOTP codes.
- **Rate limits**: `WITHDRAWALS` 10/min per caller (create + quote); approvals share the TOTP failure budget.
- **Security**: invariants of §4; Tron raw data decoded and verified; ETH/BTC/Solana txs built locally; destinations re-validated by the worker; gateway-owned destinations refused; provider errors URL-free; Solana never signs through ed25519-dalek 1.x; withdrawals of gas tokens ADMIN-only; every new secret-bearing struct with a redacted `Debug`.

---

## 8. Testnet mode (`CHAIN_MODE`)

- `CHAIN_MODE=mainnet` (default) | `testnet`. `testnet` is refused unless `APP_ENV=development` (config validation).
- Per mode (constants in `config`, selected once at boot):

| | mainnet | testnet |
|---|---|---|
| Ethereum | chain id 1 | Sepolia, chain id 11155111 |
| Tron | mainnet genesis | Nile (genesis to pin at implementation) |
| Solana | mainnet genesis | devnet (genesis `EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG` — verify) |
| Bitcoin | mainnet, `bc1`, coin type `0'` | testnet4 (`tb1`, coin type `1'` for deposits **and** treasury paths), genesis to verify |
| Lightning | Spark mainnet | Spark regtest if the pinned SDK supports it (verify), else Lightning disabled |
| USDT contracts | constants | `TESTNET_USDT_ETH_CONTRACT`, `TESTNET_USDT_TRON_CONTRACT`, `TESTNET_USDT_SOLANA_MINT` (required in testnet mode) |

- `verify_networks` checks the mode's identities; address validation uses the mode's prefixes/networks.
- The Redis prefix and the DB are per environment already; never point a testnet instance at a mainnet database.

## 9. Configuration and deployment

- New env: `CHAIN_MODE` (dev only), `OUTFLOWS_FROZEN`, testnet contract variables. Everything else lives in DB settings.
- Providers: Alchemy needs a paid plan (`alchemy_getAssetTransfers` 120 CU per call; `eth_getLogs` ranges beyond the free tier); TronGrid, Helius and Esplora are required to *send* on their networks (a network without its provider has withdrawals and sweeps disabled, like monitoring today).
- Operator runbook (README): fund the gas reserves (ETH, TRX, SOL) on the treasury addresses shown in Dashboard → Treasury; the Tron treasury must be activated (send it TRX); create at least one cold-storage address-book entry; enable 2FA for at least two admins; review default limits; the freeze button.
- `config/deploy.yml` / `.kamal/secrets`: add `OUTFLOWS_FROZEN` (clear, empty by default).

## 10. Migration plan (one migration, guarded like `20261012000000`)

1. `ALTER TYPE "Token" ADD VALUE` ETH, TRX, SOL; new enums (`WithdrawalStatus`, `DestinationKind`, `OutboundPurpose`, `OutboundTxStatus`, `SweepStatus`, `WithdrawalPolicy`, `AlertType`); `WebhookEventType` is TEXT today — new values need no DDL.
2. Widen `expectedAmount` / `amount` to `DECIMAL(38,18)`.
3. Create the tables of §7.2.
4. `api_keys`: add `withdrawalPolicy`, `withdrawalTokens`, `withdrawalNetworks`; migrate `canSend` (§7.14) and per-key limits; replace the CHECK; drop `canSend`.
5. Seed `asset_settings` and `sweep_settings` (carry over payout settings); delete `payoutMaxSats`/`payoutDailyMaxSats` rows.
6. `webhook_deliveries`: new subject columns, CHECK, guarded unique indexes; update every `DISTINCT ON`.

## 11. Testing

- **Unit (no network, no DB)**: token/network matrix; wei conversions; destination validators (fixtures per network, checksum and dust cases); params hash; state-machine transition function (pure); limit and availability arithmetic; approval rules (single-admin delay, expiry, requester ≠ approver); BTC coin selection and fee/size; ETH fee computation and replacement bump ≥ 10 %; Tron `raw_data` decoding/verification (real `triggersmartcontract` fixture, tampered variants refused); Solana message construction (fee payer first, idempotent ATA, `transfer_checked`, `close_account`); treasury golden addresses; signature vectors (§7.1); alert dedup.
- **Mocked workers**: chain clients behind traits (as `LightningOps`), covering crash between sign and broadcast (re-broadcast same bytes), freeze checks, nonce resync, expiry proofs, STUCK path.
- **Testnet end to end** (`CHAIN_MODE=testnet`): ETH deposit (external + internal via a contract), each sweep path (ETH, ERC-20 with top-up, TRC-20 with activation, SPL with close), each withdrawal path, approval with two admins, single-admin self-approval (time-travelled in tests), freeze mid-flow, fee bump on Sepolia/testnet4.
- Existing suites stay green; `cargo clippy --workspace --all-targets --locked -- -D warnings`; dashboard lint/build.

## 12. Implementation map (single release, suggested order)

1. `domain` (tokens, network matrix, validators) → `config` (constants, `CHAIN_MODE`, freeze env).
2. `wallet` (`Signer`, treasury paths, vectors).
3. Migration + `db` modules (`withdrawals.rs`, `outbound.rs`, `sweeps.rs`, `address_book.rs`, `asset_settings.rs`, `alerts.rs`).
4. `chain`: ETH native detection; per-network tx builders/broadcasters/status readers (`chain/src/evm.rs`, `tron_tx.rs`, `solana_tx.rs`, `btc_tx.rs`) through `request.rs`; `lightning-client`: `prepare_send_payment`/`send_payment`.
5. `jobs`: admission (`withdrawals.rs`), outbound engine, sweeper, treasury monitor, approval expiry, webhooks/alerts, Lightning migration (`payouts.rs` → alias).
6. `http-api`: merchant + admin routes, deprecation alias, settings, freeze.
7. Dashboard pages.
8. Docs: README (API, operator runbook, env), AGENTS.md (scope line "No other outbound transfers" removed; new invariants of §4; event list; DISTINCT ON keys), `deny.toml` wording.

## 13. Points to verify during implementation

- Tron: activation behaviour of USDT-only deposit addresses and real energy figures (use `estimateenergy` on Nile/mainnet before fixing buffers).
- Alchemy `uniqueId` format and `internal` coverage of ERC-4337/Safe transfers (testnet + a mainnet sample).
- Spark regtest support in SDK 0.26.1; BOLT11 fee fields semantics.
- Testnet genesis hashes / chain identities (Nile, devnet, testnet4).
- `alloy-*` as direct dependencies: no duplicate versions; MSRV 1.91 acceptable (CI uses stable).
- Bitcoin Core incremental relay fee for RBF bumps.

## 14. References

Facts behind this spec (APIs, costs, crates, quirks) were gathered on 2026-10-11 with sources: Alchemy `alchemy_getAssetTransfers` and CU costs; EIP-1559; Tether USDT ABI (no bool return); TRON signature/broadcast flow, resource model, `delegateresource`; Solana fee payer, ATA rent (2 039 280 lamports), `getSignatureStatuses`; Esplora API; BIP143, BIP125 and Bitcoin Core 28/29 full-RBF; Breez SDK Spark 0.26.1 `prepare_send_payment`/`send_payment`; BitGo consolidation/gas-tank patterns.
