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

export interface AuthResponse {
  access_token: string
  refresh_token: string
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

export type WebhookEventType = "pending" | "paid" | "expired"

export interface WebhookDelivery {
  id: string
  /** Address lifecycle event this delivery notifies. */
  event: WebhookEventType
  /** deposits.id — null on pending/expired event deliveries (no deposit). */
  deposit_id: string | null
  /** deposit_addresses.id — what the outgoing webhook payload calls deposit_id. */
  deposit_address_id: string
  /** null on deliveries from before per-endpoint tracking. */
  webhook_endpoint_id: string | null
  endpoint_url: string | null
  /** null on pending/expired event deliveries. */
  tx_hash: string | null
  address: string
  network: Network | "unknown"
  /** Deposit amount for paid events, expected amount otherwise. */
  amount: string
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
