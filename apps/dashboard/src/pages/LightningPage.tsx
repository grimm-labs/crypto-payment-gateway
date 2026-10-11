import { useState } from "react"
import {
  ArrowDownLeft,
  ArrowLeft,
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
import { StepUpCodeField } from "@/components/shared/StepUpCodeField"
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
import {
  type PayoutParams,
  useLightningPayments,
  useLightningQuote,
  usePayLightningAddress,
  useSettings,
} from "@/hooks/queries"
import { toastError } from "@/lib/errors"
import { formatAmount, formatDate, satsToBtc, shortHash } from "@/lib/format"
import type { LightningPayment, LightningQuote } from "@/lib/types"
import { isTotpCode } from "@/lib/validation"

const PAGE_SIZE = 25

/** Mirrors the API check: `user@domain` with a dotted domain. */
const LN_ADDRESS = /^[A-Za-z0-9._-]+@([A-Za-z0-9-]+\.)+[A-Za-z]{2,}$/

/**
 * Idempotency-Key per payout parameter set. Module-level on purpose: it
 * outlives the dialog and the page, so retrying the same payout after an
 * error — even after closing the dialog or navigating away — reuses the
 * same key and the API answers with the original payment instead of paying
 * twice (#48). Editing a parameter selects (or creates) another key; a key
 * is dropped only once its payout is confirmed.
 */
const payoutKeys = new Map<string, string>()

function payoutSignature(params: PayoutParams): string {
  return JSON.stringify([params.ln_address, params.amount_sats, params.comment ?? null])
}

function idempotencyKeyFor(params: PayoutParams): string {
  const signature = payoutSignature(params)
  let key = payoutKeys.get(signature)
  if (!key) {
    key = crypto.randomUUID()
    payoutKeys.set(signature, key)
  }
  return key
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

function PayoutLimits() {
  const settings = useSettings()
  if (!settings.data) return null
  const { payout_max_sats: perPayout, payout_daily_max_sats: daily } = settings.data
  if (perPayout === 0 || daily === 0) {
    return (
      <p className="text-xs text-destructive">
        Payouts are disabled (limit set to 0) — change it in{" "}
        <Link to="/settings" className="underline underline-offset-4">
          Settings
        </Link>
        .
      </p>
    )
  }
  return (
    <p className="text-xs text-muted-foreground">
      Limits: {perPayout.toLocaleString()} sats per payout,{" "}
      {daily.toLocaleString()} sats per 24 h (fees excluded).
    </p>
  )
}

function PayoutDialog() {
  const [open, setOpen] = useState(false)
  // The form survives closing the dialog: only a confirmed payout clears it,
  // so reopening after an error retries the very same payout (same key).
  const [address, setAddress] = useState("")
  const [sats, setSats] = useState("")
  const [comment, setComment] = useState("")
  const [quote, setQuote] = useState<LightningQuote | null>(null)
  const [code, setCode] = useState("")
  const getQuote = useLightningQuote()
  const pay = usePayLightningAddress()

  const addressValid = LN_ADDRESS.test(address.trim())
  const satsValid = /^[1-9]\d*$/.test(sats.trim())
  const btcHint = satsValid ? satsToBtc(sats.trim()) : null
  const params: PayoutParams = {
    ln_address: address.trim(),
    amount_sats: sats.trim(),
    comment: comment.trim() || undefined,
  }

  function handleOpenChange(next: boolean) {
    // Never drop a send in flight on the floor.
    if (!next && pay.isPending) return
    setOpen(next)
    if (!next) {
      setQuote(null)
      setCode("")
    }
  }

  async function review(event: React.FormEvent) {
    event.preventDefault()
    if (!addressValid || !satsValid) return
    try {
      setQuote(await getQuote.mutateAsync(params))
      setCode("")
    } catch (error) {
      toastError(error, "Could not get a quote")
    }
  }

  async function send(event: React.FormEvent) {
    event.preventDefault()
    if (!quote || !isTotpCode(code)) return
    const idempotencyKey = idempotencyKeyFor(params)
    try {
      const { payment, replayed } = await pay.mutateAsync({
        ...params,
        totp_code: code.trim(),
        idempotencyKey,
      })
      payoutKeys.delete(payoutSignature(params))
      if (payment.status === "failed") {
        // Failed for good: its key is dropped above, so sending again makes
        // a new attempt (with a fresh code).
        setCode("")
        toast.error(
          replayed
            ? "This exact payout was already attempted and failed — send again to make a new attempt"
            : "The payment failed — send again to make a new attempt",
        )
        return
      }
      if (replayed) {
        toast.info(
          `This payout was already made (${payment.amount_sats} sats, status ${payment.status}) — nothing new was sent`,
        )
      } else {
        toast.success(
          `Payment ${payment.status === "completed" ? "sent" : "submitted"}: ${payment.amount_sats} sats (+${payment.fees_sats} sats fees)`,
        )
      }
      setOpen(false)
      setAddress("")
      setSats("")
      setComment("")
      setQuote(null)
      setCode("")
    } catch (error) {
      // The key is kept: a retry of these parameters can never pay twice.
      // The code was consumed (or wrong) — a fresh one is needed.
      setCode("")
      toastError(error, "Payment failed")
    }
  }

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      <DialogTrigger asChild>
        <Button>
          <Send className="size-4" /> Pay a Lightning address
        </Button>
      </DialogTrigger>
      <DialogContent className="sm:max-w-md">
        {quote ? (
          <form onSubmit={send} className="space-y-4">
            <DialogHeader>
              <DialogTitle>Review payment</DialogTitle>
              <DialogDescription>
                Check the recipient and the total before sending.
              </DialogDescription>
            </DialogHeader>
            <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-2 rounded-lg border bg-muted/40 p-3 text-sm">
              <dt className="text-muted-foreground">Recipient</dt>
              <dd className="break-all font-medium">{quote.ln_address}</dd>
              <dt className="text-muted-foreground">Amount</dt>
              <dd>
                <SatsAmount sats={quote.amount_sats} />
              </dd>
              <dt className="text-muted-foreground">Network fee</dt>
              <dd>
                <SatsAmount sats={quote.fees_sats} />
              </dd>
              <dt className="font-medium">Total</dt>
              <dd className="font-semibold">
                <SatsAmount sats={quote.total_sats} />
              </dd>
              {params.comment && (
                <>
                  <dt className="text-muted-foreground">Comment</dt>
                  <dd className="break-all">{params.comment}</dd>
                </>
              )}
            </dl>
            <div className="flex items-start gap-2 rounded-lg border border-amber-300 bg-amber-50 p-3 text-xs dark:border-amber-800 dark:bg-amber-950/40">
              <TriangleAlert className="mt-0.5 size-3.5 shrink-0 text-amber-600" />
              <p>Lightning payments are final — they cannot be reversed.</p>
            </div>
            <StepUpCodeField
              id="payout-code"
              value={code}
              onChange={setCode}
              disabled={pay.isPending}
              autoFocus
            />
            <DialogFooter>
              <Button
                type="button"
                variant="outline"
                disabled={pay.isPending}
                onClick={() => {
                  setQuote(null)
                  setCode("")
                }}
              >
                <ArrowLeft className="size-4" /> Back
              </Button>
              <Button type="submit" disabled={!isTotpCode(code) || pay.isPending}>
                {pay.isPending ? "Sending…" : "Send payment"}
              </Button>
            </DialogFooter>
          </form>
        ) : (
          <form onSubmit={review} className="space-y-4">
            <DialogHeader>
              <DialogTitle>Pay a Lightning address</DialogTitle>
              <DialogDescription>
                Sends from the Lightning wallet. Network fees come on top of the
                amount — you review them before sending.
              </DialogDescription>
            </DialogHeader>
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
              <PayoutLimits />
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
            <DialogFooter>
              <Button
                type="submit"
                disabled={!addressValid || !satsValid || getQuote.isPending}
              >
                {getQuote.isPending ? "Getting a quote…" : "Review"}
              </Button>
            </DialogFooter>
          </form>
        )}
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
          <PayoutDialog />
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
