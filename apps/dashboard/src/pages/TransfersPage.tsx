import { useState } from "react"
import { ArrowRight, Plus } from "lucide-react"
import { useSearchParams } from "react-router-dom"

import { Pagination } from "@/components/shared/Pagination"
import { ProjectBadge } from "@/components/shared/ProjectBadge"
import { QueryError } from "@/components/shared/QueryError"
import { TransferDialog } from "@/components/transfers/TransferDialog"
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
import { useTransfers } from "@/hooks/transfers"
import { useIsAdmin } from "@/lib/auth"
import { formatAmount, formatDate, networkLabel } from "@/lib/format"
import { NETWORKS } from "@/lib/networks"
import { useProjectScope } from "@/lib/project-scope"
import type { TransferSide } from "@/lib/types"

const PAGE_SIZE = 20
const ALL = "all"

function SideBadge({ side }: { side: TransferSide }) {
  return <ProjectBadge projectId={side.type === "project" ? side.id : null} merchantLabel="Merchant funds" />
}

export function TransfersPage() {
  const [searchParams, setSearchParams] = useSearchParams()
  const network = searchParams.get("network") ?? ALL
  const offset = Math.max(0, Number(searchParams.get("offset") ?? 0) || 0)
  const isAdmin = useIsAdmin()
  const [creating, setCreating] = useState(false)
  // The selected project on either side; "All" shows every transfer.
  const { scopeParam } = useProjectScope()

  const transfers = useTransfers({
    project_id: scopeParam,
    network: network === ALL ? undefined : network,
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
          <h1 className="text-2xl font-semibold tracking-tight">Internal transfers</h1>
          <p className="max-w-3xl text-sm text-muted-foreground">
            Ledger moves of one asset between two owners (a project, or the merchant
            funds). Nothing moves on chain, both sides are notified, and a transfer is
            never edited — a mistake is undone with the reverse transfer.
          </p>
        </div>
        {isAdmin && (
          <Button onClick={() => setCreating(true)}>
            <Plus className="size-4" /> New transfer
          </Button>
        )}
      </div>

      <Select value={network} onValueChange={(value) => updateParams({ network: value, offset: undefined })}>
        <SelectTrigger className="w-40">
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

      {transfers.isError ? (
        <QueryError error={transfers.error} />
      ) : transfers.isLoading ? (
        <Skeleton className="h-64 w-full" />
      ) : (
        <>
          <div className="rounded-lg border">
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>Date</TableHead>
                  <TableHead>From</TableHead>
                  <TableHead />
                  <TableHead>To</TableHead>
                  <TableHead>Asset</TableHead>
                  <TableHead className="text-right">Amount</TableHead>
                  <TableHead>Note</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {transfers.data?.data.length === 0 && (
                  <TableRow>
                    <TableCell colSpan={7} className="h-24 text-center text-muted-foreground">
                      No internal transfer yet.
                    </TableCell>
                  </TableRow>
                )}
                {transfers.data?.data.map((transfer) => (
                  <TableRow key={transfer.id}>
                    <TableCell className="text-muted-foreground">{formatDate(transfer.created_at)}</TableCell>
                    <TableCell>
                      <SideBadge side={transfer.from} />
                    </TableCell>
                    <TableCell>
                      <ArrowRight className="size-4 text-muted-foreground" />
                    </TableCell>
                    <TableCell>
                      <SideBadge side={transfer.to} />
                    </TableCell>
                    <TableCell>
                      {networkLabel(transfer.network)} · {transfer.token}
                    </TableCell>
                    <TableCell className="text-right tabular-nums">
                      {formatAmount(transfer.amount, transfer.token)}
                    </TableCell>
                    <TableCell className="max-w-72 truncate" title={transfer.note}>
                      {transfer.note}
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          </div>
          {transfers.data && (
            <Pagination
              total={transfers.data.total}
              limit={PAGE_SIZE}
              offset={offset}
              onOffsetChange={(next) => updateParams({ offset: next ? String(next) : undefined })}
            />
          )}
        </>
      )}

      {isAdmin && <TransferDialog open={creating} onOpenChange={setCreating} />}
    </div>
  )
}
