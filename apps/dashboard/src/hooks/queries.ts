import {
  keepPreviousData,
  useMutation,
  useQuery,
  useQueryClient,
} from "@tanstack/react-query"

import { api } from "@/lib/api"
import type {
  AdminStats,
  ApiKey,
  AppSettings,
  AppSettingsUpdate,
  CreatedApiKey,
  DepositAddress,
  DepositAddressDetail,
  LightningPayment,
  LightningPaymentsPage,
  Paginated,
  Staff,
  TotpSetup,
  TrafficOverview,
  TrafficRequest,
  TrafficSurface,
  TrafficWindow,
  WalletBalances,
  WebhookDelivery,
  WebhookEndpoint,
  WebhookEventType,
} from "@/lib/types"

function listParams(params: Record<string, string | number | undefined>): string {
  const query = new URLSearchParams()
  for (const [key, value] of Object.entries(params)) {
    if (value !== undefined && value !== "") query.set(key, String(value))
  }
  const encoded = query.toString()
  return encoded ? `?${encoded}` : ""
}

export function useStats() {
  return useQuery({
    queryKey: ["stats"],
    queryFn: () => api<AdminStats>("/stats"),
    refetchInterval: 30_000,
  })
}

export interface DepositPoint {
  date: string
  count: number
  volume: string
}

export function useDepositsTimeseries(days: number) {
  return useQuery({
    queryKey: ["deposits-timeseries", days],
    queryFn: () => api<{ days: number; data: DepositPoint[] }>(
      `/stats/deposits-timeseries?days=${days}`,
    ),
    refetchInterval: 60_000,
  })
}

/** Near-real-time: 5 s polling (30 s on the 24 h window, whose percentile
 * queries scan far more rows). Previous data is kept so refetches never
 * flash a skeleton. */
export function useTrafficOverview(params: { surface: TrafficSurface; window: TrafficWindow }) {
  return useQuery({
    queryKey: ["traffic", "overview", params],
    queryFn: () => api<TrafficOverview>(`/traffic/overview${listParams(params)}`),
    refetchInterval: params.window === "24h" ? 30_000 : 5_000,
    placeholderData: keepPreviousData,
  })
}

export interface TrafficRequestFilters {
  surface: TrafficSurface
  status?: string
  route?: string
  api_key_id?: string
  limit: number
  offset: number
}

/** Only the first page auto-refreshes, so deeper pages don't shift under
 * the reader as new rows arrive. */
export function useTrafficRequests(filters: TrafficRequestFilters) {
  return useQuery({
    queryKey: ["traffic", "requests", filters],
    queryFn: () => api<Paginated<TrafficRequest>>(`/traffic/requests${listParams({ ...filters })}`),
    refetchInterval: filters.offset === 0 ? 5_000 : false,
    placeholderData: keepPreviousData,
  })
}

export function useWalletBalances() {
  return useQuery({
    queryKey: ["wallet-balances"],
    queryFn: () => api<WalletBalances>("/wallet-balances"),
  })
}

/** Slow endpoint (one RPC call per address) — only ever fetched on demand. */
export function useLiveBalances() {
  return useQuery({
    queryKey: ["live-balances"],
    queryFn: () => api<WalletBalances>("/balances"),
    enabled: false,
    gcTime: 0,
  })
}

/** The whole Lightning wallet history, newest first. */
export function useLightningPayments(page: { limit: number; offset: number }) {
  return useQuery({
    queryKey: ["lightning-payments", page],
    queryFn: () => api<LightningPaymentsPage>(`/lightning/payments${listParams(page)}`),
    placeholderData: (previous) => previous,
  })
}

export interface PayoutPayload {
  ln_address: string
  /** Integer sats string; fees come on top. */
  amount_sats: string
  comment?: string
  /** UUID — a replay with the same key never pays twice. */
  idempotencyKey: string
}

/** Pay a Lightning address from the wallet (admin only). */
export function usePayLightningAddress() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ idempotencyKey, ...body }: PayoutPayload) =>
      api<LightningPayment>("/lightning/payments", {
        body,
        headers: { "Idempotency-Key": idempotencyKey },
      }),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["lightning-payments"] })
      queryClient.invalidateQueries({ queryKey: ["wallet-balances"] })
    },
  })
}

export interface PaymentFilters {
  status?: string
  network?: string
  limit: number
  offset: number
}

export function usePayments(filters: PaymentFilters) {
  return useQuery({
    queryKey: ["payments", filters],
    queryFn: () =>
      api<Paginated<DepositAddress>>(`/deposit-addresses${listParams({ ...filters })}`),
  })
}

export function usePayment(id: string | undefined) {
  return useQuery({
    queryKey: ["payments", id],
    queryFn: () => api<DepositAddressDetail>(`/deposit-addresses/${id}`),
    enabled: Boolean(id),
  })
}

export interface CreatePaymentPayload {
  network: string
  token: string
  /** Decimal string — USDT networks only. */
  expected_amount?: string
  /** Integer sats string — bitcoin/lightning only. */
  expected_amount_sats?: string
  expires_at?: string
  metadata?: Record<string, unknown>
  /** Lightning only: BOLT11 memo (≤ 128 chars). */
  description?: string
}

export function useCreatePayment() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (payload: CreatePaymentPayload) =>
      api<DepositAddress>("/deposit-addresses", { body: payload }),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["payments"] })
      queryClient.invalidateQueries({ queryKey: ["stats"] })
    },
  })
}

export interface WebhookFilters {
  status?: string
  limit: number
  offset: number
}

export function useWebhooks(filters: WebhookFilters) {
  return useQuery({
    queryKey: ["webhooks", filters],
    queryFn: () => api<Paginated<WebhookDelivery>>(`/webhooks${listParams({ ...filters })}`),
  })
}

export function useRetryWebhook() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (depositId: string) =>
      api<{ status: string }>(`/deposits/${depositId}/webhooks/retry`, { method: "POST" }),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["webhooks"] })
      queryClient.invalidateQueries({ queryKey: ["payments"] })
      queryClient.invalidateQueries({ queryKey: ["stats"] })
    },
  })
}

/** Replay one delivery row — works for every event type. */
export function useRetryWebhookDelivery() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (deliveryId: string) =>
      api<{ status: string }>(`/webhook-deliveries/${deliveryId}/retry`, { method: "POST" }),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["webhooks"] })
      queryClient.invalidateQueries({ queryKey: ["payments"] })
      queryClient.invalidateQueries({ queryKey: ["stats"] })
    },
  })
}

export function useRetryAllFailedWebhooks() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: () => api<{ retried: number }>("/webhooks/retry-failed", { method: "POST" }),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["webhooks"] })
      queryClient.invalidateQueries({ queryKey: ["stats"] })
    },
  })
}

/** Admin-only endpoint — pass `enabled: false` for non-admin viewers. */
export function useSettings(enabled = true) {
  return useQuery({
    queryKey: ["settings"],
    queryFn: () => api<AppSettings>("/settings"),
    enabled,
  })
}

export function useUpdateSettings() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (settings: AppSettingsUpdate) =>
      api<AppSettings>("/settings", { method: "PATCH", body: settings }),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["settings"] }),
  })
}

export function useWebhookEndpoints() {
  return useQuery({
    queryKey: ["webhook-endpoints"],
    queryFn: () => api<{ data: WebhookEndpoint[] }>("/webhook-endpoints"),
  })
}

export function useCreateWebhookEndpoint() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({
      url,
      events,
      secret,
    }: {
      url: string
      events: WebhookEventType[]
      /** Optional signing secret; omitted = unsigned deliveries. */
      secret?: string
    }) =>
      api<WebhookEndpoint>("/webhook-endpoints", {
        body: { url, events, ...(secret ? { secret } : {}) },
      }),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["webhook-endpoints"] }),
  })
}

export function useUpdateWebhookEndpoint() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({
      id,
      ...body
    }: {
      id: string
      events?: WebhookEventType[]
      /** Non-empty = rotate the signing secret; empty string = remove it. */
      secret?: string
    }) =>
      api<WebhookEndpoint>(`/webhook-endpoints/${id}`, {
        method: "PATCH",
        body,
      }),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["webhook-endpoints"] }),
  })
}

export function useToggleWebhookEndpoint() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ id, is_active }: { id: string; is_active: boolean }) =>
      api<WebhookEndpoint>(`/webhook-endpoints/${id}`, {
        method: "PATCH",
        body: { is_active },
      }),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["webhook-endpoints"] })
      queryClient.invalidateQueries({ queryKey: ["webhooks"] })
    },
  })
}

export function useApiKeys() {
  return useQuery({
    queryKey: ["api-keys"],
    queryFn: () => api<{ data: ApiKey[] }>("/api-keys"),
  })
}

export function useCreateApiKey() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (payload: { name: string; expires_in_days: number | null; can_send: boolean }) =>
      api<CreatedApiKey>("/api-keys", { body: payload }),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["api-keys"] }),
  })
}

/** Grant or withdraw a key's Lightning payout permission. */
export function useUpdateApiKey() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ id, can_send }: { id: string; can_send: boolean }) =>
      api<{ id: string; can_send: boolean }>(`/api-keys/${id}`, {
        method: "PATCH",
        body: { can_send },
      }),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["api-keys"] }),
  })
}

export function useRevokeApiKey() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (id: string) => api<{ status: string }>(`/api-keys/${id}`, { method: "DELETE" }),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["api-keys"] }),
  })
}

export function useStaffList() {
  return useQuery({
    queryKey: ["staff"],
    queryFn: () => api<Paginated<Staff>>("/staff?limit=100"),
  })
}

export interface CreateStaffPayload {
  email: string
  password: string
  first_name: string
  last_name: string
  role: string
}

export function useCreateStaff() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (payload: CreateStaffPayload) => api<Staff>("/staff", { body: payload }),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["staff"] }),
  })
}

export interface UpdateStaffPayload {
  id: string
  first_name?: string
  last_name?: string
  role?: string
  is_active?: boolean
  password?: string
}

export function useUpdateStaff() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ id, ...payload }: UpdateStaffPayload) =>
      api<Staff>(`/staff/${id}`, { method: "PATCH", body: payload }),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["staff"] })
      // Deactivating a member revokes every API key they created.
      queryClient.invalidateQueries({ queryKey: ["api-keys"] })
    },
  })
}

export function useMe() {
  return useQuery({
    queryKey: ["me"],
    queryFn: () => api<Staff>("/auth/me"),
  })
}

export function useTotpSetup() {
  return useMutation({
    mutationFn: () => api<TotpSetup>("/auth/totp/setup", { method: "POST" }),
  })
}

export function useEnableTotp() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (code: string) => api<Staff>("/auth/totp/enable", { body: { code } }),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["me"] })
      queryClient.invalidateQueries({ queryKey: ["staff"] })
    },
  })
}

export function useDisableTotp() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (code: string) => api<Staff>("/auth/totp/disable", { body: { code } }),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["me"] })
      queryClient.invalidateQueries({ queryKey: ["staff"] })
    },
  })
}

export function useResetStaffTotp() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (id: string) => api<Staff>(`/staff/${id}/totp/reset`, { method: "POST" }),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["staff"] }),
  })
}
