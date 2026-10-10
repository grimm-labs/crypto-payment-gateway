import { ArrowDownLeft, ArrowUpRight, ChevronLeft, ChevronRight, RefreshCw } from "lucide-react"
import { Link, useSearchParams } from "react-router-dom"

import { CopyButton } from "@/components/shared/CopyButton"
import { QueryError } from "@/components/shared/QueryError"
import { StatusBadge } from "@/components/shared/StatusBadge"
import { Button } from "@/components/ui/button"
import { Skeleton } from "@/components/ui/skeleton"
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table"
import { useLightningPayments } from "@/hooks/queries"
import { formatAmount, formatDate, shortHash } from "@/lib/format"
import type { LightningPayment } from "@/lib/types"

const PAGE_SIZE = 25

function Reference({ payment }: { payment: LightningPayment }) {
  const value = payment.invoice ?? payment.tx_id
  if (!value) return <span className="text-xs text-muted-foreground">—</span>
  return (
    <div className="flex items-center gap-1">
      <code className="text-xs" title={value}>
        {shortHash(value, 8)}
      </code>
      <CopyButton value={value} />
    </div>
  )
}

export function LightningPage() {
  const [searchParams, setSearchParams] = useSearchParams()
  const offset = Math.max(0, Number(searchParams.get("offset") ?? 0) || 0)
  const payments = useLightningPayments({ limit: PAGE_SIZE, offset })

  function goTo(next: number) {
    const params = new URLSearchParams(searchParams)
    if (next > 0) params.set("offset", String(next))
    else params.delete("offset")
    setSearchParams(params)
  }

  const page = Math.floor(offset / PAGE_SIZE) + 1

  return (
    <div className="space-y-6 p-4 lg:p-6">
      <div className="flex flex-wrap items-end justify-between gap-4">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">Lightning</h1>
          <p className="text-sm text-muted-foreground">
            Every payment of the Breez/Spark wallet, newest first — including funds
            unrelated to gateway invoices.
          </p>
        </div>
        <Button
          variant="outline"
          onClick={() => payments.refetch()}
          disabled={payments.isFetching}
        >
          <RefreshCw className={payments.isFetching ? "size-4 animate-spin" : "size-4"} />
          Refresh
        </Button>
      </div>

      {payments.isError ? (
        <QueryError error={payments.error} />
      ) : payments.isLoading ? (
        <Skeleton className="h-64 w-full" />
      ) : (
        <>
          <div className="rounded-lg border">
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>Date</TableHead>
                  <TableHead>Direction</TableHead>
                  <TableHead>Amount</TableHead>
                  <TableHead>Fees</TableHead>
                  <TableHead>Status</TableHead>
                  <TableHead>Method</TableHead>
                  <TableHead>Invoice / tx</TableHead>
                  <TableHead>Gateway payment</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {payments.data?.data.length === 0 && (
                  <TableRow>
                    <TableCell colSpan={8} className="h-24 text-center text-muted-foreground">
                      No Lightning payments yet.
                    </TableCell>
                  </TableRow>
                )}
                {payments.data?.data.map((payment) => (
                  <TableRow key={payment.id}>
                    <TableCell className="text-muted-foreground">
                      {formatDate(payment.created_at)}
                    </TableCell>
                    <TableCell>
                      <span className="inline-flex items-center gap-1 text-sm">
                        {payment.direction === "receive" ? (
                          <ArrowDownLeft className="size-3.5 text-emerald-600" />
                        ) : (
                          <ArrowUpRight className="size-3.5 text-amber-600" />
                        )}
                        {payment.direction === "receive" ? "Received" : "Sent"}
                      </span>
                    </TableCell>
                    <TableCell className="tabular-nums">
                      <div>{formatAmount(payment.amount, "BTC")}</div>
                      <div className="text-xs text-muted-foreground">
                        {payment.amount_sats} sats
                      </div>
                    </TableCell>
                    <TableCell className="tabular-nums text-muted-foreground">
                      {payment.fees_sats} sats
                    </TableCell>
                    <TableCell>
                      <StatusBadge status={payment.status} />
                    </TableCell>
                    <TableCell className="capitalize">{payment.method}</TableCell>
                    <TableCell>
                      <Reference payment={payment} />
                      {payment.description && (
                        <p
                          className="max-w-48 truncate text-xs text-muted-foreground"
                          title={payment.description}
                        >
                          {payment.description}
                        </p>
                      )}
                    </TableCell>
                    <TableCell>
                      {payment.deposit_address_id ? (
                        <Link
                          to={`/payments/${payment.deposit_address_id}`}
                          className="text-sm underline-offset-4 hover:underline"
                        >
                          View payment
                        </Link>
                      ) : (
                        <span className="text-xs text-muted-foreground">—</span>
                      )}
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          </div>

          <div className="flex items-center justify-between pt-2">
            <p className="text-sm text-muted-foreground">Page {page}</p>
            <div className="flex gap-2">
              <Button
                variant="outline"
                size="sm"
                disabled={offset === 0 || payments.isFetching}
                onClick={() => goTo(Math.max(0, offset - PAGE_SIZE))}
              >
                <ChevronLeft className="size-4" />
                Previous
              </Button>
              <Button
                variant="outline"
                size="sm"
                disabled={!payments.data?.has_more || payments.isFetching}
                onClick={() => goTo(offset + PAGE_SIZE)}
              >
                Next
                <ChevronRight className="size-4" />
              </Button>
            </div>
          </div>
        </>
      )}
    </div>
  )
}
