import {
  keepPreviousData,
  useMutation,
  useQuery,
  useQueryClient,
  type QueryClient,
} from "@tanstack/react-query"

import { listParams } from "@/hooks/queries"
import { api, apiResponse } from "@/lib/api"
import type {
  AddressBookEntry,
  Alert,
  AssetSettingsUpdate,
  AssetSettingsView,
  LegacyPayoutRequest,
  Network,
  OutflowsState,
  Paginated,
  Sweep,
  TreasuryView,
  Withdrawal,
  WithdrawalDetail,
  WithdrawalQuote,
  WithdrawalRequest,
} from "@/lib/types"
import type { WithdrawalAction } from "@/lib/withdrawals"

/** Everything a withdrawal move can change on screen. */
function invalidateWithdrawals(queryClient: QueryClient) {
  queryClient.invalidateQueries({ queryKey: ["withdrawals"] })
  queryClient.invalidateQueries({ queryKey: ["treasury"] })
  queryClient.invalidateQueries({ queryKey: ["project-balance"] })
  queryClient.invalidateQueries({ queryKey: ["ledger"] })
}

// ---------------------------------------------------------------- withdrawals

export interface WithdrawalFilters {
  status?: string
  network?: string
  token?: string
  api_key_id?: string
  /** A project id, "merchant", or undefined for all. */
  project_id?: string
  limit: number
  offset: number
}

export function useWithdrawals(filters: WithdrawalFilters) {
  return useQuery({
    queryKey: ["withdrawals", "list", filters],
    queryFn: () => api<Paginated<Withdrawal>>(`/withdrawals${listParams({ ...filters })}`),
    // In-flight withdrawals move on their own (signing, confirmations).
    refetchInterval: filters.offset === 0 ? 15_000 : false,
    placeholderData: keepPreviousData,
  })
}

/** Sidebar badge: how many withdrawals wait for an admin's approval. */
export function usePendingApprovalCount() {
  return useQuery({
    queryKey: ["withdrawals", "pending-approval-count"],
    queryFn: () =>
      api<Paginated<Withdrawal>>("/withdrawals?status=pending_approval&limit=1"),
    select: (page) => page.total,
    refetchInterval: 30_000,
  })
}

export function useWithdrawal(id: string | undefined) {
  return useQuery({
    queryKey: ["withdrawals", "detail", id],
    queryFn: () => api<WithdrawalDetail>(`/withdrawals/${id}`),
    enabled: Boolean(id),
    refetchInterval: 15_000,
  })
}

/** Fee estimate, approval and destination checks — nothing is reserved. */
export function useWithdrawalQuote() {
  return useMutation({
    mutationFn: (request: WithdrawalRequest) =>
      api<WithdrawalQuote>("/withdrawals/quote", { body: request }),
  })
}

export interface CreateWithdrawalPayload extends WithdrawalRequest {
  /** Step-up code — single use. */
  totp_code: string
  /** UUID — a replay with the same key and parameters never sends twice. */
  idempotencyKey: string
}

/** `replayed` = the API answered 200 with a withdrawal an earlier request created. */
export function useCreateWithdrawal() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: async ({ idempotencyKey, ...body }: CreateWithdrawalPayload) => {
      const { status, data } = await apiResponse<Withdrawal>("/withdrawals", {
        body,
        headers: { "Idempotency-Key": idempotencyKey },
      })
      return { withdrawal: data, replayed: status === 200 }
    },
    onSuccess: () => {
      invalidateWithdrawals(queryClient)
      queryClient.invalidateQueries({ queryKey: ["lightning-payments"] })
    },
  })
}

/** Approve / reject / cancel / bump / abandon — every one needs a fresh code. */
export function useWithdrawalAction() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ id, action, totp_code }: { id: string; action: WithdrawalAction; totp_code: string }) =>
      api<Withdrawal>(`/withdrawals/${id}/${action}`, { body: { totp_code } }),
    onSuccess: () => invalidateWithdrawals(queryClient),
  })
}

// --------------------------------------------------------------- address book

/** `projectId`: one owner's entries (a project id or "merchant"); undefined = all. */
export function useAddressBook(network?: Network, projectId?: string, options?: { enabled?: boolean }) {
  return useQuery({
    queryKey: ["address-book", network ?? "all", projectId ?? "all"],
    queryFn: () =>
      api<{ data: AddressBookEntry[] }>(
        `/address-book${listParams({ network, project_id: projectId })}`,
      ),
    enabled: options?.enabled ?? true,
  })
}

export function useCreateAddressBookEntry() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (payload: {
      network: Network
      destination: string
      label: string
      is_cold_storage: boolean
      totp_code: string
      /** The owner: a project id or "merchant". */
      project_id: string
    }) => api<AddressBookEntry>("/address-book", { body: payload }),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["address-book"] })
      queryClient.invalidateQueries({ queryKey: ["alerts"] })
    },
  })
}

export function useDeleteAddressBookEntry() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ id, totp_code }: { id: string; totp_code: string }) =>
      api<{ status: string; id: string }>(`/address-book/${id}`, {
        method: "DELETE",
        body: { totp_code },
      }),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["address-book"] }),
  })
}

// ------------------------------------------------------------ treasury & co

/** Reads every treasury balance from the providers: refreshed once a minute. */
export function useTreasury() {
  return useQuery({
    queryKey: ["treasury"],
    queryFn: () => api<TreasuryView>("/treasury"),
    refetchInterval: 60_000,
  })
}

export interface SweepFilters {
  network?: string
  status?: string
  limit: number
  offset: number
}

export function useSweeps(filters: SweepFilters) {
  return useQuery({
    queryKey: ["sweeps", filters],
    queryFn: () => api<Paginated<Sweep>>(`/sweeps${listParams({ ...filters })}`),
    placeholderData: keepPreviousData,
  })
}

/** Plans sweeps now (the engine then executes them). */
export function useRunSweeps() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (payload: { network?: Network; totp_code: string }) =>
      api<{ planned: number }>("/sweeps/run", { body: payload }),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["sweeps"] })
      queryClient.invalidateQueries({ queryKey: ["treasury"] })
    },
  })
}

export function useAlerts(page: { limit: number; offset: number }) {
  return useQuery({
    queryKey: ["alerts", page],
    queryFn: () => api<Paginated<Alert>>(`/alerts${listParams(page)}`),
    refetchInterval: page.offset === 0 ? 60_000 : false,
    placeholderData: keepPreviousData,
  })
}

// ------------------------------------------------------------ outflow freeze

/** Polled app-wide: the frozen banner must appear on every page quickly. */
export function useOutflows() {
  return useQuery({
    queryKey: ["outflows"],
    queryFn: () => api<OutflowsState>("/outflows"),
    refetchInterval: 15_000,
  })
}

function onFreezeChange(queryClient: QueryClient) {
  queryClient.invalidateQueries({ queryKey: ["outflows"] })
  queryClient.invalidateQueries({ queryKey: ["treasury"] })
}

/** Any staff member, no code: speed matters more. */
export function useFreezeOutflows() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: () => api<{ frozen: true }>("/outflows/freeze", { method: "POST" }),
    onSuccess: () => onFreezeChange(queryClient),
  })
}

export function useUnfreezeOutflows() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (totp_code: string) =>
      api<{ frozen: false }>("/outflows/unfreeze", { body: { totp_code } }),
    onSuccess: () => onFreezeChange(queryClient),
  })
}

// ------------------------------------------------------------- asset settings

export function useAssetSettings() {
  return useQuery({
    queryKey: ["asset-settings"],
    queryFn: () => api<AssetSettingsView>("/settings/assets"),
  })
}

export function useUpdateAssetSettings() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (update: AssetSettingsUpdate) =>
      api<AssetSettingsView>("/settings/assets", { method: "PATCH", body: update }),
    onSuccess: (view) => {
      queryClient.setQueryData(["asset-settings"], view)
      queryClient.invalidateQueries({ queryKey: ["treasury"] })
    },
  })
}

// ------------------------------------------------------------ legacy payouts

/** Lightning payouts made before withdrawals existed (admin only). */
export function useLegacyPayouts(page: { limit: number; offset: number }) {
  return useQuery({
    queryKey: ["payout-requests", page],
    queryFn: () => api<Paginated<LegacyPayoutRequest>>(`/payout-requests${listParams(page)}`),
    placeholderData: keepPreviousData,
  })
}
