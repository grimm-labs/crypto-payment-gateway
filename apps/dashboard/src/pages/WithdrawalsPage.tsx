import { useState } from "react"
import { Plus, ShieldAlert, X } from "lucide-react"
import { Link, useSearchParams } from "react-router-dom"

import { Pagination } from "@/components/shared/Pagination"
import { QueryError } from "@/components/shared/QueryError"
import { StatusBadge } from "@/components/shared/StatusBadge"
import { TxHash } from "@/components/shared/TxHash"
import { RequesterLabel } from "@/components/withdrawals/RequesterLabel"
import { WithdrawalDialog } from "@/components/withdrawals/WithdrawalDialog"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select"
import { Skeleton } from "@/components/ui/skeleton"
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table"
import { usePendingApprovalCount, useWithdrawals } from "@/hooks/withdrawals"
import { useIsAdmin } from "@/lib/auth"
import { formatAmount, formatDate, humanize, networkLabel, shortHash } from "@/lib/format"
import { NETWORKS } from "@/lib/networks"
import { WITHDRAWAL_STATUSES } from "@/lib/withdrawals"

const PAGE_SIZE = 20
const ALL = "all"
const TOKENS = ["USDT", "BTC", "ETH", "TRX", "SOL"]

export function WithdrawalsPage() {
  const [searchParams, setSearchParams] = useSearchParams()
  const status = searchParams.get("status") ?? ALL
  const network = searchParams.get("network") ?? ALL
  const token = searchParams.get("token") ?? ALL
  const apiKeyId = searchParams.get("api_key_id") ?? undefined
  const offset = Math.max(0, Number(searchParams.get("offset") ?? 0) || 0)
  const isAdmin = useIsAdmin()
  const [creating, setCreating] = useState(false)
  const pendingApprovals = usePendingApprovalCount()

  const withdrawals = useWithdrawals({
    status: status === ALL ? undefined : status,
    network: network === ALL ? undefined : network,
    token: token === ALL ? undefined : token,
    api_key_id: apiKeyId,
    limit: PAGE_SIZE,
    offset,
  })

  function updateParams(updates: Record<string, string | undefined>) {
    const next = new URLSearchParams(searchParams)
    for (const [key, value] of Object.entries(updates)) {
      if (value === undefined || value === ALL) next.delete(key)
      else next.set(key, value)
    }
    setSearchParams(next)
  }

  return (
    <div className="space-y-6 p-4 lg:p-6">
      <div className="flex flex-wrap items-end justify-between gap-4">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">Withdrawals</h1>
          <p className="text-sm text-muted-foreground">
            Every transfer out of the gateway — from the dashboard or merchant keys,
            on every network (Lightning included).
          </p>
        </div>
        {isAdmin && (
          <Button onClick={() => setCreating(true)}>
            <Plus className="size-4" /> New withdrawal
          </Button>
        )}
      </div>

      {(pendingApprovals.data ?? 0) > 0 && status !== "pending_approval" && (
        <div className="flex flex-wrap items-center justify-between gap-3 rounded-lg border border-amber-300 bg-amber-50 p-3 text-sm dark:border-amber-800 dark:bg-amber-950/40">
          <div className="flex items-center gap-2">
            <ShieldAlert className="size-4 shrink-0 text-amber-600" />
            <span>
              {pendingApprovals.data} withdrawal{pendingApprovals.data === 1 ? "" : "s"} awaiting
              approval{isAdmin ? "" : " by an admin"}.
            </span>
          </div>
          <Button
            size="sm"
            variant="outline"
            onClick={() => updateParams({ status: "pending_approval", offset: undefined })}
          >
            Show them
          </Button>
        </div>
      )}

      <div className="flex flex-wrap items-center gap-3">
        <Select
          value={status}
          onValueChange={(value) => updateParams({ status: value, offset: undefined })}
        >
          <SelectTrigger className="w-44">
            <SelectValue placeholder="Status" />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value={ALL}>All statuses</SelectItem>
            {WITHDRAWAL_STATUSES.map((value) => (
              <SelectItem key={value} value={value} className="capitalize">
                {humanize(value)}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
        <Select
          value={network}
          onValueChange={(value) => updateParams({ network: value, offset: undefined })}
        >
          <SelectTrigger className="w-36">
            <SelectValue placeholder="Network" />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value={ALL}>All networks</SelectItem>
            {NETWORKS.map((value) => (
              <SelectItem key={value} value={value}>
                {networkLabel(value)}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
        <Select
          value={token}
          onValueChange={(value) => updateParams({ token: value, offset: undefined })}
        >
          <SelectTrigger className="w-32">
            <SelectValue placeholder="Token" />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value={ALL}>All tokens</SelectItem>
            {TOKENS.map((value) => (
              <SelectItem key={value} value={value}>
                {value}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
        {apiKeyId && (
          <Badge variant="secondary" className="gap-1">
            API key {shortHash(apiKeyId, 4)}
            <button
              type="button"
              aria-label="Clear the API key filter"
              onClick={() => updateParams({ api_key_id: undefined, offset: undefined })}
            >
              <X className="size-3" />
            </button>
          </Badge>
        )}
      </div>

      {withdrawals.isError ? (
        <QueryError error={withdrawals.error} />
      ) : withdrawals.isLoading ? (
        <Skeleton className="h-64 w-full" />
      ) : (
        <>
          <div className="rounded-lg border">
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>Created</TableHead>
                  <TableHead>Amount</TableHead>
                  <TableHead>Network</TableHead>
                  <TableHead>Destination</TableHead>
                  <TableHead>Status</TableHead>
                  <TableHead>Requested by</TableHead>
                  <TableHead>Transaction</TableHead>
                  <TableHead className="text-right">Details</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {withdrawals.data?.data.length === 0 && (
                  <TableRow>
                    <TableCell colSpan={8} className="h-24 text-center text-muted-foreground">
                      No withdrawals found.
                    </TableCell>
                  </TableRow>
                )}
                {withdrawals.data?.data.map((withdrawal) => (
                  <TableRow key={withdrawal.id}>
                    <TableCell className="text-muted-foreground">
                      {formatDate(withdrawal.created_at)}
                    </TableCell>
                    <TableCell className="tabular-nums">
                      <span className="font-medium">
                        {formatAmount(withdrawal.amount, withdrawal.token)}
                      </span>
                      {withdrawal.approval_required && (
                        <p className="text-xs text-muted-foreground">Approval required</p>
                      )}
                    </TableCell>
                    <TableCell>{networkLabel(withdrawal.network)}</TableCell>
                    <TableCell>
                      <code className="text-xs" title={withdrawal.destination}>
                        {shortHash(withdrawal.destination, 8)}
                      </code>
                      {withdrawal.address_book_entry_id && (
                        <p className="text-xs text-muted-foreground">Address book</p>
                      )}
                    </TableCell>
                    <TableCell>
                      <StatusBadge status={withdrawal.status} />
                    </TableCell>
                    <TableCell>
                      <RequesterLabel requester={withdrawal.requested_by} />
                    </TableCell>
                    <TableCell>
                      <TxHash network={withdrawal.network} hash={withdrawal.tx_hash} keep={6} />
                    </TableCell>
                    <TableCell className="text-right">
                      <Button asChild variant="outline" size="sm">
                        <Link to={`/withdrawals/${withdrawal.id}`}>View</Link>
                      </Button>
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          </div>
          {withdrawals.data && (
            <Pagination
              total={withdrawals.data.total}
              limit={PAGE_SIZE}
              offset={offset}
              onOffsetChange={(next) => updateParams({ offset: next ? String(next) : undefined })}
            />
          )}
        </>
      )}

      {isAdmin && <WithdrawalDialog open={creating} onOpenChange={setCreating} />}
    </div>
  )
}
