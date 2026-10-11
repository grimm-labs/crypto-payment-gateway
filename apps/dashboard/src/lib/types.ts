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

export type DepositAddressStatus = "pending" | "paid" | "expired"
export type DepositStatus = "detected" | "confirmed"
export type WebhookStatus = "pending" | "delivered" | "failed"
export type Network = "tron" | "ethereum" | "solana" | "bitcoin" | "lightning"

// All amounts are decimal strings — never parse them into floats.
export interface DepositAddress {
  id: string
  /** For lightning: the BOLT11 payment request. */
  address: string
  network: Network
  token: string
  status: DepositAddressStatus
  expected_amount: string
  /** Base units: sats for BTC, 10^-6 for USDT. */
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

export interface WebhookDelivery {
  id: string
  /** Address lifecycle event this delivery notifies. */
  event: WebhookEventType
  /** deposits.id — null on pending/expired event deliveries (no deposit). */
  deposit_id: string | null
  /** deposit_addresses.id — what the outgoing webhook payload calls deposit_id. Null on payouts. */
  deposit_address_id: string | null
  /** Lightning wallet payment id — payout deliveries only. */
  lightning_payment_id: string | null
  /** null on deliveries from before per-endpoint tracking. */
  webhook_endpoint_id: string | null
  endpoint_url: string | null
  /** null on pending/expired event deliveries. */
  tx_hash: string | null
  /** null on payout deliveries. */
  address: string | null
  network: Network | "unknown"
  /** Deposit amount for paid events, expected amount for address events, sent amount for payouts. */
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

export interface ApiKey {
  id: string
  name: string
  prefix: string
  status: ApiKeyStatus
  created_by: string | null
  last_used_at: string | null
  revoked_at: string | null
  /** null = never expires (revocable only). */
  expires_at: string | null
  /** May send Lightning payouts through /api/v1. */
  can_send: boolean
  created_at: string
}

export interface CreatedApiKey {
  id: string
  name: string
  prefix: string
  /** Full secret — shown exactly once at creation. */
  key: string
  status: ApiKeyStatus
  expires_at: string | null
  can_send: boolean
  created_at: string
}

export interface AdminStats {
  addresses: { pending: number; paid: number; expired: number }
  deposits: { detected: number; confirmed: number }
  confirmed_volume: string
  webhooks: { pending: number; delivered: number; failed: number }
}

export interface NetworkBalance {
  network: string
  token: string
  amount: string
  amount_raw: string
  available: boolean
  message?: string
  indices_scanned?: number
  addresses_checked?: number
}

export interface WalletBalances {
  total_usdt: string
  /** Bitcoin + Lightning — never folded into total_usdt. */
  total_btc?: string
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

/** Fee quote shown on the payout review step. All values are integer sats strings. */
export interface LightningQuote {
  ln_address: string
  amount_sats: string
  fees_sats: string
  /** amount + fees. */
  total_sats: string
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
  /** Max sats per Lightning payout (fees excluded); 0 = payouts disabled. */
  payout_max_sats: number
  /** Max sats sent over a rolling 24 h window; 0 = payouts disabled. */
  payout_daily_max_sats: number
}

/** PATCH /settings is partial — only provided fields are updated. */
export type AppSettingsUpdate = Partial<{
  deposit_expiry_minutes: number
  api_key_prefix: string
  /** Empty string resets to the default issuer. */
  totp_issuer: string
  /** 1–3650 days, or null to keep forever. */
  api_traffic_retention_days: number | null
  payout_max_sats: number
  payout_daily_max_sats: number
  /** Step-up code — required when a payout limit is present. */
  totp_code: string
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
