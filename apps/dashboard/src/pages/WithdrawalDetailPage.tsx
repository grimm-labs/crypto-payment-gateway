import { ArrowLeft, ShieldAlert } from "lucide-react"
import { Link, useParams } from "react-router-dom"

import { CopyButton } from "@/components/shared/CopyButton"
import { JsonViewer } from "@/components/shared/JsonViewer"
import { ProjectBadge } from "@/components/shared/ProjectBadge"
import { StatusBadge } from "@/components/shared/StatusBadge"
import { TxHash } from "@/components/shared/TxHash"
import { RequesterLabel } from "@/components/withdrawals/RequesterLabel"
import { WithdrawalActions } from "@/components/withdrawals/WithdrawalActions"
import { WithdrawalTimeline } from "@/components/withdrawals/WithdrawalTimeline"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import {
  Card,
  CardAction,
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
import { useNow } from "@/hooks/use-now"
import { useWithdrawal } from "@/hooks/withdrawals"
import { useAuth } from "@/lib/auth"
import { formatAmount, formatDate, formatRaw, humanize, networkLabel, shortHash } from "@/lib/format"
import type { OutboundTx, WithdrawalDetail } from "@/lib/types"
import { DESTINATION_KIND_LABELS, approvalBlock } from "@/lib/withdrawals"

export function WithdrawalDetailPage() {
  const { id } = useParams<{ id: string }>()
  const withdrawal = useWithdrawal(id)

  if (withdrawal.isLoading) {
    return <Skeleton className="m-4 h-96 lg:m-6" />
  }
  if (withdrawal.isError || !withdrawal.data) {
    return (
      <div className="space-y-4 p-4 lg:p-6">
        <BackLink />
        <p className="text-sm text-destructive">
          {withdrawal.error instanceof Error ? withdrawal.error.message : "Withdrawal not found"}
        </p>
      </div>
    )
  }

  const data = withdrawal.data
  return (
    <div className="space-y-6 p-4 lg:p-6">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div className="flex items-center gap-3">
          <BackLink />
          <h1 className="text-2xl font-semibold tracking-tight">Withdrawal</h1>
          <StatusBadge status={data.status} />
          <ProjectBadge projectId={data.project_id} merchantLabel="Merchant funds" />
        </div>
        <WithdrawalActions withdrawal={data} size="default" />
      </div>

      <ApprovalNotice withdrawal={data} />

      <div className="grid gap-6 lg:grid-cols-[2fr_1fr]">
        <SummaryCard withdrawal={data} />
        <Card>
          <CardHeader>
            <CardTitle>Timeline</CardTitle>
          </CardHeader>
          <CardContent>
            <WithdrawalTimeline withdrawal={data} />
          </CardContent>
        </Card>
      </div>

      <TransactionsCard withdrawal={data} />

      {data.metadata && Object.keys(data.metadata).length > 0 && (
        <Card>
          <CardHeader>
            <CardTitle>Metadata</CardTitle>
          </CardHeader>
          <CardContent>
            <JsonViewer value={data.metadata} />
          </CardContent>
        </Card>
      )}
    </div>
  )
}

function BackLink() {
  return (
    <Button asChild variant="ghost" size="sm">
      <Link to="/withdrawals">
        <ArrowLeft className="size-4" /> Back
      </Link>
    </Button>
  )
}

/** Why the signed-in admin cannot approve (self-approval rule). */
function ApprovalNotice({ withdrawal }: { withdrawal: WithdrawalDetail }) {
  const { staff } = useAuth()
  const now = useNow(60_000)
  if (withdrawal.status !== "pending_approval" || staff?.role !== "admin") return null
  const block = approvalBlock(withdrawal, staff.id, now)
  if (!block) return null
  return (
    <div className="flex items-start gap-2 rounded-lg border border-amber-300 bg-amber-50 p-3 text-sm dark:border-amber-800 dark:bg-amber-950/40">
      <ShieldAlert className="mt-0.5 size-4 shrink-0 text-amber-600" />
      <p>{block}</p>
    </div>
  )
}

function SummaryCard({ withdrawal: w }: { withdrawal: WithdrawalDetail }) {
  return (
    <Card>
      <CardHeader>
        <CardTitle className="tabular-nums">{formatAmount(w.amount, w.token)}</CardTitle>
        <CardDescription>
          {networkLabel(w.network)} · {w.token}
        </CardDescription>
        {w.approval_required && (
          <CardAction>
            <Badge variant="outline">Approval required</Badge>
          </CardAction>
        )}
      </CardHeader>
      <CardContent className="space-y-4">
        <div className="space-y-1">
          <p className="text-xs text-muted-foreground">
            Destination · {DESTINATION_KIND_LABELS[w.destination_kind] ?? w.destination_kind}
            {w.address_book_entry_id ? " · address book" : ""}
          </p>
          <div className="flex items-center justify-between gap-2 rounded-lg border bg-muted/40 p-3">
            <code className="break-all text-sm">{w.destination}</code>
            <CopyButton value={w.destination} />
          </div>
        </div>
        <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-2 text-sm">
          <dt className="text-muted-foreground">Fee (estimate)</dt>
          <dd className="tabular-nums">
            {w.fee_estimate_raw !== null ? formatRaw(w.fee_estimate_raw, w.fee_token) : "—"}
          </dd>
          <dt className="text-muted-foreground">Fee (actual)</dt>
          <dd className="tabular-nums">
            {w.fee_raw !== null ? formatRaw(w.fee_raw, w.fee_token) : "Not settled yet"}
          </dd>
          <dt className="text-muted-foreground">
            {w.network === "lightning" ? "Payment id" : "Tx hash"}
          </dt>
          <dd>
            <TxHash network={w.network} hash={w.tx_hash} />
          </dd>
          {w.network !== "lightning" && (
            <>
              <dt className="text-muted-foreground">Confirmations</dt>
              <dd>{w.confirmations ?? "—"}</dd>
            </>
          )}
          <dt className="text-muted-foreground">Requested by</dt>
          <dd>
            <RequesterLabel requester={w.requested_by} />
          </dd>
          {w.approval_expires_at && w.status === "pending_approval" && (
            <>
              <dt className="text-muted-foreground">Approval expires</dt>
              <dd>{formatDate(w.approval_expires_at)}</dd>
            </>
          )}
          {w.self_approval_not_before && (
            <>
              <dt className="text-muted-foreground">Self-approval from</dt>
              <dd>{formatDate(w.self_approval_not_before)}</dd>
            </>
          )}
          {w.reference && (
            <>
              <dt className="text-muted-foreground">Reference</dt>
              <dd className="break-all">{w.reference}</dd>
            </>
          )}
          {w.comment && (
            <>
              <dt className="text-muted-foreground">Comment</dt>
              <dd className="break-all">{w.comment}</dd>
            </>
          )}
          {w.failure_reason && (
            <>
              <dt className="text-muted-foreground">Failure</dt>
              <dd className="text-destructive">{humanize(w.failure_reason)}</dd>
            </>
          )}
          {w.cancel_reason && (
            <>
              <dt className="text-muted-foreground">Cancelled</dt>
              <dd>{humanize(w.cancel_reason)}</dd>
            </>
          )}
          <dt className="text-muted-foreground">Updated</dt>
          <dd>{formatDate(w.updated_at)}</dd>
          <dt className="text-muted-foreground">ID</dt>
          <dd className="break-all font-mono text-xs">{w.id}</dd>
        </dl>
      </CardContent>
    </Card>
  )
}

function TransactionsCard({ withdrawal }: { withdrawal: WithdrawalDetail }) {
  const txs = withdrawal.transactions ?? []
  return (
    <Card>
      <CardHeader>
        <CardTitle>Outbound transactions</CardTitle>
        <CardDescription>
          Every transaction signed for this withdrawal — fee bumps appear as
          replacements of the previous one.
        </CardDescription>
      </CardHeader>
      <CardContent>
        {txs.length === 0 ? (
          <p className="py-8 text-center text-sm text-muted-foreground">Nothing signed yet.</p>
        ) : (
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>Transaction</TableHead>
                <TableHead>Status</TableHead>
                <TableHead>From → to</TableHead>
                <TableHead>Nonce</TableHead>
                <TableHead>Fee</TableHead>
                <TableHead>Block</TableHead>
                <TableHead>Times</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {txs.map((tx) => (
                <TxRow key={tx.id} tx={tx} withdrawal={withdrawal} />
              ))}
            </TableBody>
          </Table>
        )}
      </CardContent>
    </Card>
  )
}

function TxRow({ tx, withdrawal }: { tx: OutboundTx; withdrawal: WithdrawalDetail }) {
  return (
    <TableRow>
      <TableCell>
        <TxHash network={withdrawal.network} hash={tx.tx_hash} keep={6} />
        <p className="text-xs capitalize text-muted-foreground">
          {humanize(tx.purpose)}
          {tx.replaces_id && ` · replaces ${shortHash(tx.replaces_id, 4)}`}
        </p>
        {tx.last_error && (
          <p className="max-w-56 truncate text-xs text-destructive" title={tx.last_error}>
            {tx.last_error}
          </p>
        )}
      </TableCell>
      <TableCell>
        <StatusBadge status={tx.status} />
      </TableCell>
      <TableCell className="font-mono text-xs">
        <span title={tx.from_address ?? undefined}>
          {tx.from_address ? shortHash(tx.from_address, 6) : "—"}
        </span>
        {" → "}
        <span title={tx.to_address ?? undefined}>
          {tx.to_address ? shortHash(tx.to_address, 6) : "—"}
        </span>
      </TableCell>
      <TableCell className="tabular-nums">{tx.nonce ?? "—"}</TableCell>
      <TableCell className="tabular-nums">
        {tx.fee_raw !== null ? formatRaw(tx.fee_raw, withdrawal.fee_token) : "—"}
      </TableCell>
      <TableCell className="tabular-nums">
        {tx.block_number ?? "—"}
        {tx.confirmations !== null && (
          <p className="text-xs text-muted-foreground">{tx.confirmations} conf.</p>
        )}
      </TableCell>
      <TableCell className="text-xs text-muted-foreground">
        <p>Signed {formatDate(tx.created_at)}</p>
        {tx.broadcast_at && <p>Broadcast {formatDate(tx.broadcast_at)}</p>}
        {tx.confirmed_at && <p>Confirmed {formatDate(tx.confirmed_at)}</p>}
      </TableCell>
    </TableRow>
  )
}
