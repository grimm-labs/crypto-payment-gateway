import { useState } from "react"
import {
  ArrowDownLeft,
  ArrowUpRight,
  ChevronLeft,
  ChevronRight,
  RefreshCw,
  Send,
  TriangleAlert,
} from "lucide-react"
import { Link, useSearchParams } from "react-router-dom"
import { toast } from "sonner"

import { CopyButton } from "@/components/shared/CopyButton"
import { QueryError } from "@/components/shared/QueryError"
import { StatusBadge } from "@/components/shared/StatusBadge"
import { Button } from "@/components/ui/button"
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { Skeleton } from "@/components/ui/skeleton"
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table"
import { useLightningPayments, usePayLightningAddress } from "@/hooks/queries"
import { useAuth } from "@/lib/auth"
import { formatAmount, formatDate, satsToBtc, shortHash } from "@/lib/format"
import type { LightningPayment } from "@/lib/types"

const PAGE_SIZE = 25

/** Mirrors the API check: `user@domain` with a dotted domain. */
const LN_ADDRESS = /^[A-Za-z0-9._+-]+@[A-Za-z0-9-]+(\.[A-Za-z0-9-]+)+$/

function PayoutDialog() {
  const [open, setOpen] = useState(false)
  const [address, setAddress] = useState("")
  const [sats, setSats] = useState("")
  const [comment, setComment] = useState("")
  // One key per dialog session: a double submit or a retry after a
  // network error never pays twice.
  const [idempotencyKey, setIdempotencyKey] = useState(() => crypto.randomUUID())
  const pay = usePayLightningAddress()

  const addressValid = LN_ADDRESS.test(address.trim())
  const satsValid = /^[1-9]\d*$/.test(sats.trim())
  const btcHint = satsValid ? satsToBtc(sats) : null

  function reset() {
    setAddress("")
    setSats("")
    setComment("")
    setIdempotencyKey(crypto.randomUUID())
  }

  async function submit() {
    try {
      const payment = await pay.mutateAsync({
        ln_address: address.trim(),
        amount_sats: sats.trim(),
        comment: comment.trim() || undefined,
        idempotencyKey,
      })
      toast.success(
        `Payment ${payment.status === "completed" ? "sent" : "submitted"}: ${payment.amount_sats} sats (+${payment.fees_sats} sats fees)`,
      )
      setOpen(false)
      reset()
    } catch (error) {
      toast.error(error instanceof Error ? error.message : "Payment failed")
    }
  }

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        setOpen(next)
        if (!next) reset()
      }}
    >
      <DialogTrigger asChild>
        <Button>
          <Send className="size-4" /> Pay a Lightning address
        </Button>
      </DialogTrigger>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>Pay a Lightning address</DialogTitle>
          <DialogDescription>
            Sends from the Lightning wallet. Network fees come on top of the amount.
          </DialogDescription>
        </DialogHeader>
        <div className="space-y-4">
          <div className="space-y-2">
            <Label htmlFor="ln-address">Lightning address</Label>
            <Input
              id="ln-address"
              placeholder="alice@example.com"
              value={address}
              onChange={(event) => setAddress(event.target.value)}
            />
            {address && !addressValid && (
              <p className="text-xs text-destructive">Expected user@domain</p>
            )}
          </div>
          <div className="space-y-2">
            <Label htmlFor="ln-amount">Amount (sats)</Label>
            <Input
              id="ln-amount"
              placeholder="10000"
              inputMode="numeric"
              value={sats}
              onChange={(event) => setSats(event.target.value)}
            />
            {btcHint && <p className="text-xs text-muted-foreground">≈ {btcHint} BTC</p>}
            {sats && !satsValid && (
              <p className="text-xs text-destructive">Whole number of sats, at least 1</p>
            )}
          </div>
          <div className="space-y-2">
            <Label htmlFor="ln-comment">Comment (optional)</Label>
            <Input
              id="ln-comment"
              placeholder="Withdrawal"
              value={comment}
              onChange={(event) => setComment(event.target.value)}
            />
          </div>
          <div className="flex items-start gap-2 rounded-lg border border-amber-300 bg-amber-50 p-3 text-xs dark:border-amber-800 dark:bg-amber-950/40">
            <TriangleAlert className="mt-0.5 size-3.5 shrink-0 text-amber-600" />
            <p>Lightning payments are final — they cannot be reversed.</p>
          </div>
        </div>
        <DialogFooter>
          <Button
            onClick={submit}
            disabled={!addressValid || !satsValid || pay.isPending}
          >
            {pay.isPending ? "Sending…" : "Send payment"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
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
  const { staff } = useAuth()
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
        <div className="flex gap-2">
          <Button
            variant="outline"
            onClick={() => payments.refetch()}
            disabled={payments.isFetching}
          >
            <RefreshCw className={payments.isFetching ? "size-4 animate-spin" : "size-4"} />
            Refresh
          </Button>
          {staff?.role === "admin" && <PayoutDialog />}
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
    </div>
  )
}
