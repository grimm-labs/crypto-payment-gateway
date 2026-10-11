import { useState } from "react"
import {
  ArrowDownLeft,
  ArrowUpRight,
  ChevronLeft,
  ChevronRight,
  RefreshCw,
  Send,
} from "lucide-react"
import { Link, useSearchParams } from "react-router-dom"

import { CopyButton } from "@/components/shared/CopyButton"
import { Pagination } from "@/components/shared/Pagination"
import { QueryError } from "@/components/shared/QueryError"
import { StatusBadge } from "@/components/shared/StatusBadge"
import { RequesterLabel } from "@/components/withdrawals/RequesterLabel"
import { WithdrawalDialog } from "@/components/withdrawals/WithdrawalDialog"
import { Button } from "@/components/ui/button"
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card"
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
import { useLegacyPayouts } from "@/hooks/withdrawals"
import { formatAmount, formatDate, satsToBtc, shortHash } from "@/lib/format"
import type { LightningPayment } from "@/lib/types"

const PAGE_SIZE = 25
const LEGACY_PAGE_SIZE = 10

/** Payouts made before withdrawals existed — kept for history (read-only). */
function LegacyPayoutsCard() {
  const [offset, setOffset] = useState(0)
  const payouts = useLegacyPayouts({ limit: LEGACY_PAGE_SIZE, offset })

  if (payouts.isSuccess && payouts.data.total === 0) return null

  return (
    <Card>
      <CardHeader>
        <CardTitle>Legacy payouts</CardTitle>
        <CardDescription>
          Lightning-address payouts requested before withdrawals existed. New sends
          are withdrawals (see the Withdrawals page).
        </CardDescription>
      </CardHeader>
      <CardContent>
        {payouts.isError ? (
          <QueryError error={payouts.error} />
        ) : payouts.isLoading ? (
          <Skeleton className="h-32 w-full" />
        ) : (
          <>
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>Date</TableHead>
                  <TableHead>Lightning address</TableHead>
                  <TableHead>Amount</TableHead>
                  <TableHead>Status</TableHead>
                  <TableHead>Requested by</TableHead>
                  <TableHead>Payment id</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {payouts.data?.data.map((payout) => (
                  <TableRow key={payout.id}>
                    <TableCell className="text-muted-foreground">
                      {formatDate(payout.created_at)}
                    </TableCell>
                    <TableCell className="text-sm">{payout.ln_address}</TableCell>
                    <TableCell>
                      <SatsAmount sats={payout.amount_sats} />
                    </TableCell>
                    <TableCell>
                      <StatusBadge status={payout.status} />
                    </TableCell>
                    <TableCell>
                      <RequesterLabel requester={payout.requested_by} />
                    </TableCell>
                    <TableCell>
                      {payout.payment_id ? (
                        <div className="flex items-center gap-1">
                          <code className="text-xs" title={payout.payment_id}>
                            {shortHash(payout.payment_id, 6)}
                          </code>
                          <CopyButton value={payout.payment_id} />
                        </div>
                      ) : (
                        <span className="text-xs text-muted-foreground">—</span>
                      )}
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
            {payouts.data && payouts.data.total > LEGACY_PAGE_SIZE && (
              <Pagination
                total={payouts.data.total}
                limit={LEGACY_PAGE_SIZE}
                offset={offset}
                onOffsetChange={setOffset}
              />
            )}
          </>
        )}
      </CardContent>
    </Card>
  )
}

function SatsAmount({ sats }: { sats: string }) {
  const btc = satsToBtc(sats)
  return (
    <span className="tabular-nums">
      {sats} sats
      {btc && <span className="ml-1 text-xs text-muted-foreground">({btc} BTC)</span>}
    </span>
  )
}

function Reference({ payment }: { payment: LightningPayment }) {
  const value = payment.invoice ?? payment.tx_id
  if (!value) return <span className="text-xs text-muted-foreground">—</span>
  return (
    <div className="flex items-center gap-1">
      {payment.ln_address && (
        <span className="mr-1 text-xs font-medium">{payment.ln_address}</span>
      )}
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
  const [sending, setSending] = useState(false)

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
        <div className="flex gap-2">
          <Button
            variant="outline"
            onClick={() => payments.refetch()}
            disabled={payments.isFetching}
          >
            <RefreshCw className={payments.isFetching ? "size-4 animate-spin" : "size-4"} />
            Refresh
          </Button>
          {/* The former payout dialog: a Lightning withdrawal now. */}
          <Button onClick={() => setSending(true)}>
            <Send className="size-4" /> Send a Lightning payment
          </Button>
        </div>
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
                      {(payment.description ?? payment.comment) && (
                        <p
                          className="max-w-48 truncate text-xs text-muted-foreground"
                          title={payment.description ?? payment.comment ?? undefined}
                        >
                          {payment.description ?? payment.comment}
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

      <LegacyPayoutsCard />

      <WithdrawalDialog
        open={sending}
        onOpenChange={setSending}
        preset={{ network: "lightning", token: "BTC" }}
        lockNetwork
        title="Send a Lightning payment"
      />
    </div>
  )
}
