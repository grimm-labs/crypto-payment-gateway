export type StaffRole = "admin" | "operator"

export interface Staff {
  id: string
  email: string
  first_name: string
  last_name: string
  role: StaffRole
  is_active: boolean
  totp_enabled: boolean
  created_at: string
}

/** Every endpoint that opens a session. The refresh token travels only in
 *  the HttpOnly `cpg_refresh` cookie — never in JSON. */
export interface AuthResponse {
  access_token: string
  staff: Staff
}

export interface MfaChallenge {
  mfa_required: true
  mfa_token: string
}

export type LoginResult = AuthResponse | MfaChallenge

export interface TotpSetup {
  secret: string
  otpauth_url: string
}

export interface Paginated<T> {
  data: T[]
  total: number
  limit: number
  offset: number
}

// ------------------------------------------------------------------ projects

/** The 8-colour palette of the dashboard (CSS tokens `--project-<color>`). */
export type ProjectColor =
  | "blue"
  | "emerald"
  | "amber"
  | "rose"
  | "violet"
  | "cyan"
  | "orange"
  | "fuchsia"

export interface ProjectCounts {
  api_keys: number
  deposit_addresses: number
  withdrawals: number
  webhook_endpoints: number
  address_book_entries: number
}

/** One of the merchant's applications; owns keys, payments, withdrawals,
 *  endpoints and address-book entries. */
export interface Project {
  id: string
  name: string
  color: ProjectColor
  /** Tag of the project's API keys; null = the global tag. */
  key_tag: string | null
  created_at: string
  updated_at: string
  counts: ProjectCounts
}

/** Rows from before the first project, offered to it by the first-project step. */
export interface UnassignedCounts {
  api_keys: number
  deposit_addresses: number
  withdrawals: number
  webhook_endpoints: number
  address_book_entries: number
}

export interface ProjectsResponse {
  projects: Project[]
  /** Set while no project exists; null afterwards. */
  unassigned: UnassignedCounts | null
}

/** The literal `project_id` takes to name the merchant funds / endpoints. */
export const MERCHANT_OWNER = "merchant"

/** One project's balance of one asset (spec §7): a ledger figure derived
 *  from its rows. `balance` and `available` are signed decimal strings;
 *  every `_raw` is the integer in base units. */
export interface ProjectAssetBalance {
  network: Network
  token: Token
  deposits: string
  deposits_raw: string
  unattributed: string
  unattributed_raw: string
  withdrawn: string
  withdrawn_raw: string
  fees: string
  fees_raw: string
  transfers_in: string
  transfers_in_raw: string
  transfers_out: string
  transfers_out_raw: string
  balance: string
  balance_raw: string
  reserved: string
  reserved_raw: string
  available: string
  available_raw: string
  /** Detected, unconfirmed deposits — informational. */
  pending: string
  pending_raw: string
}

export interface ProjectBalances {
  project_id: string
  balances: ProjectAssetBalance[]
}

/** One payment pair reconciled (spec §10.4): what the gateway holds against
 *  the project balances. Holdings and merchant funds are null when unknown. */
export interface LedgerProjectRow {
  project_id: string
  balance: string
  balance_raw: string
  available: string
  available_raw: string
}

export interface LedgerPair {
  network: Network
  token: Token
  holdings: string | null
  holdings_raw: string | null
  projects: LedgerProjectRow[]
  project_total: string
  project_total_raw: string
  merchant_funds: string | null
  merchant_funds_raw: string | null
  status: "ok" | "shortfall" | "unknown"
}

/** One side of an internal transfer. */
export interface TransferSide {
  type: "project" | "merchant"
  id: string | null
}

/** An admin's ledger move of one asset on one network between two owners
 *  (nothing moves on chain); append-only. */
export interface InternalTransfer {
  id: string
  network: Network
  token: Token
  amount: string
  amount_raw: string
  from: TransferSide
  to: TransferSide
  note: string
  created_at: string
}

export type DepositAddressStatus = "pending" | "paid" | "expired"
export type DepositStatus = "detected" | "confirmed"
export type WebhookStatus = "pending" | "delivered" | "failed"
export type Network = "tron" | "ethereum" | "solana" | "bitcoin" | "lightning"
/** API token codes. TRX and SOL are gas assets: never accepted as payments. */
export type Token = "USDT" | "BTC" | "ETH" | "TRX" | "SOL"

// All amounts are decimal strings — never parse them into floats.
export interface DepositAddress {
  id: string
  /** Owning project; null only on payments from before projects existed. */
  project_id: string | null
  /** For lightning: the BOLT11 payment request. */
  address: string
  network: Network
  token: string
  status: DepositAddressStatus
  expected_amount: string
  /** Base units of the row's token: sats for BTC, wei for ETH, 10^-6 for USDT. */
  expected_amount_raw: string
  expires_at: string
  created_at: string
}

export interface DepositWebhook {
  status: WebhookStatus
  attempts: number
  last_response: number | null
  last_error: string | null
  updated_at: string
}

export interface DepositWebhookEntry extends DepositWebhook {
  endpoint_id: string
  endpoint_url: string | null
}

export interface Deposit {
  id: string
  tx_hash: string
  amount: string
  amount_raw: string
  confirmations: number
  status: DepositStatus
  confirmed_at: string | null
  /** Most recent delivery across all endpoints (legacy aggregate). */
  webhook: DepositWebhook | null
  /** Per-endpoint delivery states. */
  webhooks: DepositWebhookEntry[]
}

export interface DepositAddressDetail extends DepositAddress {
  metadata: Record<string, unknown>
  deposits: Deposit[]
}

export type WebhookEventType =
  | "pending"
  | "paid"
  | "expired"
  | "payout_succeeded"
  | "payout_failed"
  | "withdrawal_pending_approval"
  | "withdrawal_broadcast"
  | "withdrawal_confirmed"
  | "withdrawal_failed"
  | "withdrawal_cancelled"
  | "gas_reserve_low"
  | "treasury_cap_exceeded"
  | "address_book_entry_added"
  | "withdrawal_stuck"
  | "transfer_completed"
  | "ledger_shortfall"

export interface WebhookDelivery {
  id: string
  /** Owner of the target endpoint; null = merchant endpoint (or a legacy row). */
  project_id: string | null
  /** Internal-transfer deliveries only. */
  transfer_id?: string | null
  /** Address lifecycle event this delivery notifies. */
  event: WebhookEventType
  /** deposits.id — null on pending/expired event deliveries (no deposit). */
  deposit_id: string | null
  /** deposit_addresses.id — what the outgoing webhook payload calls deposit_id. Null on payouts. */
  deposit_address_id: string | null
  /** Lightning wallet payment id — payout deliveries only. */
  lightning_payment_id: string | null
  /** Withdrawal lifecycle deliveries only. */
  withdrawal_id?: string | null
  /** Operational alert deliveries only. */
  alert_id?: string | null
  /** null on deliveries from before per-endpoint tracking. */
  webhook_endpoint_id: string | null
  endpoint_url: string | null
  /** null on pending/expired event deliveries. */
  tx_hash: string | null
  /** null on payout deliveries. */
  address: string | null
  network: Network | "unknown"
  /** Deposit amount for paid events, expected amount for address events, sent amount for payouts and withdrawals. */
  amount: string | null
  deposit_status: DepositStatus | null
  status: WebhookStatus
  attempts: number
  last_response: number | null
  last_error: string | null
  /** JSON body of the most recent attempt — null until one is made. */
  payload: Record<string, unknown> | null
  created_at: string
  updated_at: string
}

export type ApiKeyStatus = "active" | "revoked" | "expired"

/** Destination policy of a merchant key: `book` = address-book entries
 *  only, `open` = any destination within the key's own limits. */
export type WithdrawalPolicy = "none" | "book" | "open"

/** Tokens a merchant key may be allowed to withdraw (gas assets are admin-only). */
export type KeyWithdrawalToken = "USDT" | "BTC" | "ETH"

export interface ApiKeyWithdrawalLimit {
  token: Token
  max_per_withdrawal: string | null
  max_per_withdrawal_raw: string
  daily_max: string | null
  daily_max_raw: string
}

export interface ApiKeyWithdrawalGrant {
  withdrawal_policy: WithdrawalPolicy
  withdrawal_tokens: string[]
  /** null = every network of the allowed tokens. */
  withdrawal_networks: Network[] | null
  withdrawal_limits: ApiKeyWithdrawalLimit[]
}

export interface ApiKey extends ApiKeyWithdrawalGrant {
  id: string
  /** Owning project; null only on a legacy key. */
  project_id: string | null
  name: string
  prefix: string
  status: ApiKeyStatus
  created_by: string | null
  last_used_at: string | null
  revoked_at: string | null
  /** null = never expires (revocable only — such a key never withdraws). */
  expires_at: string | null
  created_at: string
}

export interface CreatedApiKey extends ApiKeyWithdrawalGrant {
  id: string
  project_id: string | null
  name: string
  prefix: string
  /** Full secret — shown exactly once at creation. */
  key: string
  status: ApiKeyStatus
  expires_at: string | null
  created_at: string
}

export interface ApiKeyWithdrawalGrantUpdate {
  withdrawal_policy: WithdrawalPolicy
  withdrawal_tokens: KeyWithdrawalToken[]
  withdrawal_networks: Network[] | null
  /** Decimal token amounts; "0" disables. Required for every token of an open key. */
  withdrawal_limits: Array<{ token: KeyWithdrawalToken; max_per_withdrawal: string; daily_max: string }>
  /** Step-up code — required unless the policy is set to none. */
  totp_code?: string
}

export interface AdminStats {
  addresses: { pending: number; paid: number; expired: number }
  deposits: { detected: number; confirmed: number }
  confirmed_volume: string
  webhooks: { pending: number; delivered: number; failed: number }
}

/** One (network, token): treasury + unswept deposit addresses. */
export interface NetworkBalance {
  network: string
  token: string
  amount: string
  amount_raw: string
  /** null when unreadable (or cached before treasuries existed). */
  treasury_amount?: string | null
  treasury_amount_raw?: string | null
  /** Held on deposit addresses, not swept yet. */
  deposits_amount?: string
  deposits_amount_raw?: string
  available: boolean
  message?: string
  indices_scanned?: number
  addresses_checked?: number
}

export interface WalletBalances {
  total_usdt: string
  /** Bitcoin + Lightning — never folded into total_usdt. */
  total_btc?: string
  total_eth?: string
  /** Gas reserves (treasuries). */
  total_trx?: string
  total_sol?: string
  networks: NetworkBalance[]
  updated_at?: string | null
}

export type LightningDirection = "send" | "receive"
export type LightningPaymentStatus = "completed" | "pending" | "failed"

/** One entry of the Breez/Spark wallet history (any direction, any status). */
export interface LightningPayment {
  id: string
  direction: LightningDirection
  status: LightningPaymentStatus
  /** BTC decimal string. */
  amount: string
  amount_sats: string
  fees_sats: string
  /** lightning | spark | token | deposit | withdraw | unknown */
  method: string
  /** BOLT11, for Lightning payments. */
  invoice: string | null
  description: string | null
  /** On-chain txid, for deposits into / withdrawals from the wallet. */
  tx_id: string | null
  /** Lightning address paid (LNURL-pay sends). */
  ln_address: string | null
  comment: string | null
  created_at: string | null
  /** Gateway payment (deposit_addresses.id) this settled — null otherwise. */
  deposit_address_id: string | null
}

/** No total from the SDK: paging is driven by `has_more`. */
export interface LightningPaymentsPage {
  data: LightningPayment[]
  limit: number
  offset: number
  has_more: boolean
}

export interface AppSettings {
  deposit_expiry_minutes: number
  api_key_prefix: string | null
  /** Effective value — the default is returned when nothing is configured. */
  totp_issuer: string
  /** Days the API traffic log is kept; null = forever. */
  api_traffic_retention_days: number | null
}

/** PATCH /settings is partial — only provided fields are updated. */
export type AppSettingsUpdate = Partial<{
  deposit_expiry_minutes: number
  api_key_prefix: string
  /** Empty string resets to the default issuer. */
  totp_issuer: string
  /** 1–3650 days, or null to keep forever. */
  api_traffic_retention_days: number | null
}>

/** `merchant` = /api/v1 (API keys), `admin` = dashboard calls, `all` = both. */
export type TrafficSurface = "merchant" | "admin" | "all"
export type TrafficWindow = "15m" | "1h" | "24h"
export type StatusClass = "2xx" | "3xx" | "4xx" | "5xx"

export interface TrafficSummary {
  requests_total: number
  requests_last_minute: number
  requests_per_minute_avg: number
  /** 0–1 fraction of 4xx+5xx over the window. */
  error_rate: number
  client_errors: number
  server_errors: number
  p50_ms: number | null
  p95_ms: number | null
  active_keys: number
  /** Events lost since boot because the in-memory queue was full. */
  dropped_events: number
}

export interface TrafficBucket {
  bucket: string
  ok: number
  client_error: number
  server_error: number
  p95_ms: number | null
}

export interface TrafficRouteStat {
  method: string
  route: string
  count: number
  client_errors: number
  server_errors: number
  p95_ms: number | null
}

export interface TrafficApiKeyStat {
  /** null = requests that carried no valid key. */
  api_key_id: string | null
  name: string | null
  prefix: string | null
  status: ApiKeyStatus | null
  count: number
  client_errors: number
  server_errors: number
  p95_ms: number | null
  last_seen_at: string
}

export interface TrafficOverview {
  surface: TrafficSurface
  window: TrafficWindow
  bucket_seconds: number
  summary: TrafficSummary
  timeseries: TrafficBucket[]
  top_routes: TrafficRouteStat[]
  top_api_keys: TrafficApiKeyStat[]
}

export interface TrafficRequest {
  id: number
  surface: Exclude<TrafficSurface, "all">
  method: string
  route: string
  status: number
  duration_ms: number
  api_key: { id: string; name: string | null; prefix: string | null } | null
  staff: { id: string; name: string | null } | null
  ip: string | null
  created_at: string
}

export interface WebhookEndpoint {
  id: string
  /** The owner, fixed at creation; null = merchant endpoint (instance events). */
  project_id: string | null
  url: string
  is_active: boolean
  /** Events this endpoint receives. */
  events: WebhookEventType[]
  /** Whether deliveries are signed (the secret itself is write-only). */
  has_secret: boolean
  created_by: string | null
  created_at: string
  updated_at: string
}

export type AuditActorType = "staff" | "api_key" | "system"

export interface AuditLogEntry {
  id: string
  actor_type: AuditActorType
  actor_id: string | null
  /** Staff full name or API key name at the time of the action. */
  actor_name: string | null
  /** e.g. `payout.sent`, `staff.updated`. */
  action: string
  target_type: string | null
  target_id: string | null
  details: Record<string, unknown>
  ip: string | null
  created_at: string
}

// ---------------------------------------------------------------- withdrawals
// Amounts: `x` is a decimal string in token units, `x_raw` the integer string
// in base units — never parsed into floats.

export type WithdrawalStatus =
  | "pending_approval"
  | "queued"
  | "signed"
  | "broadcast"
  | "confirmed"
  | "failed"
  | "cancelled"
  | "stuck"

export type DestinationKind = "address" | "ln_address" | "bolt11"

export interface Requester {
  type: "api_key" | "staff"
  id: string
}

export interface Withdrawal {
  id: string
  /** The owner debited; null = the merchant funds. */
  project_id: string | null
  status: WithdrawalStatus
  network: Network
  token: Token
  amount: string
  amount_raw: string
  fee_estimate: string | null
  fee_estimate_raw: string | null
  /** Actual fee, recorded once the network settled it. */
  fee: string | null
  fee_raw: string | null
  /** What the fee is paid in (the network's gas asset, BTC on bitcoin/lightning). */
  fee_token: Token
  destination: string
  destination_kind: DestinationKind
  address_book_entry_id: string | null
  approval_required: boolean
  /** Staff id of the approving admin. */
  approved_by: string | null
  approved_at: string | null
  /** Pending approvals are cancelled (expired) after this. */
  approval_expires_at: string | null
  /** Single-admin instance: the requester may approve their own after this. */
  self_approval_not_before: string | null
  cancel_reason: string | null
  /** On-chain hash — the wallet payment id on lightning. */
  tx_hash: string | null
  confirmations: number | null
  failure_reason: string | null
  reference: string | null
  comment: string | null
  metadata: Record<string, unknown> | null
  requested_by: Requester | null
  created_at: string
  updated_at: string
  broadcast_at: string | null
  confirmed_at: string | null
}

export type OutboundPurpose = "withdrawal" | "sweep" | "gas_topup"
export type OutboundTxStatus = "signed" | "broadcast" | "confirmed" | "failed" | "replaced" | "expired"

export interface OutboundTx {
  id: string
  purpose: OutboundPurpose
  tx_hash: string | null
  status: OutboundTxStatus
  from_address: string | null
  to_address: string | null
  nonce: number | null
  /** In the network's fee token. */
  fee_raw: string | null
  /** The transaction this one replaced (fee bump). */
  replaces_id: string | null
  block_number: number | null
  confirmations: number | null
  last_error: string | null
  created_at: string
  broadcast_at: string | null
  confirmed_at: string | null
}

export interface WithdrawalDetail extends Withdrawal {
  transactions: OutboundTx[]
}

export interface WithdrawalQuote {
  network: Network
  token: Token
  amount: string
  amount_raw: string
  fee_estimate: string
  fee_estimate_raw: string
  fee_token: Token
  approval_required: boolean
  destination: string
  destination_kind: DestinationKind
  address_book_entry_id: string | null
  /** False when this caller may not send to this destination (see refusal). */
  destination_acceptable: boolean
  destination_refusal: string | null
  /** Available balance of the token; null when unreadable. */
  available: string | null
  available_raw: string | null
  /** Available balance of the fee token. */
  fee_available: string | null
  fee_available_raw: string | null
  /** Merchant keys only: the project's available balance (signed) and
   *  whether the project check would pass; null for staff. */
  project_available: string | null
  project_available_raw: string | null
  project_check_ok: boolean | null
}

export interface WithdrawalRequest {
  network: Network
  token: Token
  /** Decimal token amount — or `amount_sats` on bitcoin/lightning. */
  amount?: string
  amount_sats?: string
  /** Exactly one of destination / address_book_entry_id. */
  destination?: string
  address_book_entry_id?: string
  reference?: string
  /** Lightning only. */
  comment?: string
  metadata?: Record<string, unknown>
  /** Staff: the owner debited — a project id or "merchant" (required). */
  project_id?: string
}

export interface AddressBookEntry {
  id: string
  /** The owner; null = a merchant entry. */
  project_id: string | null
  network: Network
  destination_kind: DestinationKind
  destination: string
  label: string
  is_cold_storage: boolean
  /** Usable once the cooling-off delay (24 h) has passed. */
  active: boolean
  active_at: string
  created_by: string | null
  created_at: string
}

export interface TreasuryRow {
  network: Network
  token: Token
  treasury_address: string | null
  /** Solana USDT: the token account holding the treasury's USDT. */
  token_account: string | null
  confirmed: string | null
  confirmed_raw: string | null
  reserved: string | null
  reserved_raw: string
  /** Estimated fees of reserved withdrawals paid in this token. */
  reserved_fees_raw: string
  available_amount: string | null
  available_raw: string | null
  unswept: string | null
  unswept_raw: string
  unattributed: string | null
  unattributed_raw: string
  gas_floor: string | null
  gas_floor_raw: string | null
  gas_alert_below: string | null
  gas_alert_below_raw: string | null
  gas_low: boolean
  hot_wallet_cap: string | null
  hot_wallet_cap_raw: string | null
  /** The cap is per asset: the asset's treasuries on every network together (what `cap_exceeded` measures). */
  asset_holdings: string | null
  asset_holdings_raw: string | null
  cap_exceeded: boolean
  /** Whether the balance could be read. */
  available: boolean
  message: string | null
}

export interface TreasuryView {
  data: TreasuryRow[]
  outflows_frozen: boolean
}

export type SweepStatus =
  | "planned"
  | "topup_sent"
  | "ready"
  | "broadcast"
  | "confirmed"
  | "failed"
  | "skipped"

export interface Sweep {
  id: string
  network: Network
  token: Token
  deposit_address_id: string | null
  from_address: string
  amount: string
  amount_raw: string
  /** Part of the amount settling confirmed deposits. */
  credited: string
  /** Unattributed funds swept along (settled no payment). */
  unattributed: string
  unattributed_raw: string
  fee: string | null
  fee_token: Token
  status: SweepStatus
  last_error: string | null
  created_at: string
  updated_at: string
}

export type AlertType =
  | "gas_reserve_low"
  | "treasury_cap_exceeded"
  | "address_book_entry_added"
  | "withdrawal_stuck"
  | "ledger_shortfall"

export interface Alert {
  alert_id: string
  type: AlertType
  network: Network | null
  token: Token | null
  details: Record<string, unknown>
  created_at: string
}

export type ChainMode = "mainnet" | "testnet"

export interface OutflowsState {
  frozen: boolean
  /** OUTFLOWS_FROZEN=true: the freeze cannot be lifted from the dashboard. */
  forced_by_environment: boolean
  /** Which chains the instance runs on — the block explorers follow it. */
  chain_mode: ChainMode
}

export interface AmountSetting {
  amount: string
  raw: string
}

export interface AssetSettings {
  token: Token
  max_per_withdrawal: AmountSetting
  daily_max: AmountSetting
  approval_threshold: AmountSetting
  hot_wallet_cap: AmountSetting | null
  gas_reserve_floor: AmountSetting | null
  gas_alert_below: AmountSetting | null
  /** ETH, TRX, SOL: held in treasuries to pay network fees. */
  is_gas_asset: boolean
  updated_at: string
}

export interface SweepSetting {
  network: Network
  token: Token
  minimum: string
  minimum_raw: string
  enabled: boolean
}

export interface AssetSettingsView {
  assets: AssetSettings[]
  sweeps: SweepSetting[]
}

/** PATCH /settings/assets — only the fields present are changed. */
export interface AssetSettingsUpdate {
  assets: Partial<
    Record<
      Token,
      Partial<{
        max_per_withdrawal: string
        daily_max: string
        approval_threshold: string
        /** null clears the optional settings. */
        hot_wallet_cap: string | null
        gas_reserve_floor: string | null
        gas_alert_below: string | null
      }>
    >
  >
  sweeps: Array<{ network: Network; token: Token; minimum?: string; enabled?: boolean }>
  totp_code: string
}

/** Pre-withdrawal Lightning payout (read-only history). */
export interface LegacyPayoutRequest {
  id: string
  ln_address: string
  amount_sats: string
  status: string
  payment_id: string | null
  requested_by: Requester | null
  created_at: string
}
