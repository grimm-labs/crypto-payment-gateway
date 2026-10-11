import { CircleCheck, CircleX, ShieldAlert, TriangleAlert } from "lucide-react"

import { formatRaw, networkLabel, toBigInt } from "@/lib/format"
import type { AddressBookEntry, WithdrawalQuote } from "@/lib/types"
import { DESTINATION_KIND_LABELS } from "@/lib/withdrawals"

/** Review step of a withdrawal: what the quote says before anything is reserved. */
export function QuoteSummary({
  quote,
  entry,
  reference,
  comment,
}: {
  quote: WithdrawalQuote
  /** The address-book entry, when the withdrawal goes to one. */
  entry: AddressBookEntry | undefined
  reference: string
  comment: string
}) {
  const amount = toBigInt(quote.amount_raw) ?? 0n
  const fee = toBigInt(quote.fee_estimate_raw) ?? 0n
  const sameToken = quote.fee_token === quote.token
  const available = toBigInt(quote.available_raw)
  const feeAvailable = toBigInt(quote.fee_available_raw)
  // Advisory only: the cached availability may lag a fresh deposit — the
  // API re-reads the balance at admission and is the judge.
  const short = available !== null && available < amount
  const feeShort =
    feeAvailable !== null && feeAvailable < (sameToken ? amount + fee : fee)

  return (
    <div className="space-y-3">
      <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-2 rounded-lg border bg-muted/40 p-3 text-sm">
        <dt className="text-muted-foreground">Network</dt>
        <dd>
          {networkLabel(quote.network)} · {quote.token}
        </dd>
        <dt className="text-muted-foreground">Destination</dt>
        <dd className="min-w-0">
          {entry && <p className="font-medium">{entry.label}</p>}
          <p className="break-all font-mono text-xs">{quote.destination}</p>
          <p className="text-xs text-muted-foreground">
            {DESTINATION_KIND_LABELS[quote.destination_kind] ?? quote.destination_kind}
            {entry ? " · address book" : ""}
            {entry?.is_cold_storage ? " · cold storage" : ""}
          </p>
        </dd>
        <dt className="text-muted-foreground">Amount</dt>
        <dd className="tabular-nums">{formatRaw(quote.amount_raw, quote.token)}</dd>
        <dt className="text-muted-foreground">Network fee (estimate)</dt>
        <dd className="tabular-nums">
          {formatRaw(quote.fee_estimate_raw, quote.fee_token)}
          <span className="block text-xs text-muted-foreground">
            Paid by the gateway in {quote.fee_token}, on top of the amount
          </span>
        </dd>
        {sameToken && (
          <>
            <dt className="font-medium">Total</dt>
            <dd className="font-semibold tabular-nums">{formatRaw(amount + fee, quote.token)}</dd>
          </>
        )}
        <dt className="text-muted-foreground">Available</dt>
        <dd className="tabular-nums">
          {quote.available_raw !== null ? formatRaw(quote.available_raw, quote.token) : "Unknown"}
          {!sameToken && (
            <span className="block text-xs text-muted-foreground">
              Fee asset:{" "}
              {quote.fee_available_raw !== null
                ? formatRaw(quote.fee_available_raw, quote.fee_token)
                : "unknown"}
            </span>
          )}
        </dd>
        {reference && (
          <>
            <dt className="text-muted-foreground">Reference</dt>
            <dd className="break-all">{reference}</dd>
          </>
        )}
        {comment && (
          <>
            <dt className="text-muted-foreground">Comment</dt>
            <dd className="break-all">{comment}</dd>
          </>
        )}
      </dl>

      {quote.destination_acceptable ? (
        <Notice tone="ok" icon={<CircleCheck className="mt-0.5 size-3.5 shrink-0 text-emerald-600" />}>
          Destination accepted by the destination policy.
        </Notice>
      ) : (
        <Notice tone="error" icon={<CircleX className="mt-0.5 size-3.5 shrink-0 text-destructive" />}>
          Destination refused{quote.destination_refusal ? `: ${quote.destination_refusal}` : ""}.
          {quote.approval_required && !entry
            ? " At or above the approval threshold, withdrawals must go to an active address-book entry."
            : ""}
        </Notice>
      )}

      {quote.approval_required && (
        <Notice tone="warn" icon={<ShieldAlert className="mt-0.5 size-3.5 shrink-0 text-amber-600" />}>
          Approval required: the amount reaches the approval threshold. Funds are
          reserved now; another admin must approve it (with their own code) before it
          is sent. Unapproved withdrawals are cancelled after 72 h.
        </Notice>
      )}

      {(short || feeShort) && (
        <Notice tone="warn" icon={<TriangleAlert className="mt-0.5 size-3.5 shrink-0 text-amber-600" />}>
          {short
            ? `The available ${quote.token} balance looks lower than the amount.`
            : `The available ${quote.fee_token} balance looks too low for the fee.`}{" "}
          The gateway re-checks the balance when you submit.
        </Notice>
      )}

      {quote.network === "lightning" && (
        <Notice tone="warn" icon={<TriangleAlert className="mt-0.5 size-3.5 shrink-0 text-amber-600" />}>
          Lightning payments are final — they cannot be reversed.
        </Notice>
      )}
    </div>
  )
}

const NOTICE_TONES = {
  ok: "border-emerald-300 bg-emerald-50 dark:border-emerald-800 dark:bg-emerald-950/40",
  warn: "border-amber-300 bg-amber-50 dark:border-amber-800 dark:bg-amber-950/40",
  error: "border-destructive/40 bg-destructive/5",
}

function Notice({
  tone,
  icon,
  children,
}: {
  tone: keyof typeof NOTICE_TONES
  icon: React.ReactNode
  children: React.ReactNode
}) {
  return (
    <div className={`flex items-start gap-2 rounded-lg border p-3 text-xs ${NOTICE_TONES[tone]}`}>
      {icon}
      <p>{children}</p>
    </div>
  )
}
