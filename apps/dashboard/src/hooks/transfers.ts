import { keepPreviousData, useMutation, useQuery, useQueryClient } from "@tanstack/react-query"

import { listParams } from "@/hooks/queries"
import { api, apiResponse } from "@/lib/api"
import type { InternalTransfer, Paginated } from "@/lib/types"

export interface TransferFilters {
  /** A project id (either side), "merchant", or undefined for all. */
  project_id?: string
  network?: string
  token?: string
  limit: number
  offset: number
}

export function useTransfers(filters: TransferFilters) {
  return useQuery({
    queryKey: ["transfers", filters],
    queryFn: () => api<Paginated<InternalTransfer>>(`/transfers${listParams({ ...filters })}`),
    placeholderData: keepPreviousData,
  })
}

export interface CreateTransferPayload {
  from_project_id: string
  to_project_id: string
  network: string
  token: string
  /** Decimal token amount — or `amount_sats` on bitcoin/lightning. */
  amount?: string
  amount_sats?: string
  note: string
  /** Step-up code — single use. */
  totp_code: string
  /** UUID — a replay with the same key and parameters never records twice. */
  idempotencyKey: string
}

/** `replayed` = the API answered 200 with a transfer an earlier request recorded. */
export function useCreateTransfer() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: async ({ idempotencyKey, ...body }: CreateTransferPayload) => {
      const { status, data } = await apiResponse<InternalTransfer>("/transfers", {
        body,
        headers: { "Idempotency-Key": idempotencyKey },
      })
      return { transfer: data, replayed: status === 200 }
    },
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["transfers"] })
      queryClient.invalidateQueries({ queryKey: ["project-balance"] })
      queryClient.invalidateQueries({ queryKey: ["ledger"] })
      queryClient.invalidateQueries({ queryKey: ["webhooks"] })
    },
  })
}
