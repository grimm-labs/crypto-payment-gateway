import { useState } from "react"
import { Play, TriangleAlert } from "lucide-react"
import { Link } from "react-router-dom"
import { toast } from "sonner"

import { Pagination } from "@/components/shared/Pagination"
import { QueryError } from "@/components/shared/QueryError"
import { StatusBadge } from "@/components/shared/StatusBadge"
import { StepUpCodeField } from "@/components/shared/StepUpCodeField"
import { Button } from "@/components/ui/button"
import {
  Card,
  CardAction,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card"
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog"
import { Label } from "@/components/ui/label"
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
import { useRunSweeps, useSweeps } from "@/hooks/withdrawals"
import { useIsAdmin } from "@/lib/auth"
import { toastError } from "@/lib/errors"
import { formatAmount, formatDate, humanize, networkLabel, shortHash, toBigInt } from "@/lib/format"
import type { Network, SweepStatus } from "@/lib/types"
import { isTotpCode } from "@/lib/validation"

const PAGE_SIZE = 10
const ALL = "all"
/** Networks the sweeper plans for (`jobs::sweeper::SWEPT_NETWORKS`). */
const SWEPT_NETWORKS: Network[] = ["ethereum", "tron", "solana", "bitcoin"]
const SWEEP_STATUSES: SweepStatus[] = [
  "planned",
  "topup_sent",
  "ready",
  "broadcast",
  "confirmed",
  "failed",
  "skipped",
]

/** Sweep history (deposit address → treasury) and the manual run. */
export function SweepsCard({ frozen }: { frozen: boolean }) {
  const isAdmin = useIsAdmin()
  const [network, setNetwork] = useState(ALL)
  const [status, setStatus] = useState(ALL)
  const [offset, setOffset] = useState(0)
  const sweeps = useSweeps({
    network: network === ALL ? undefined : network,
    status: status === ALL ? undefined : status,
    limit: PAGE_SIZE,
    offset,
  })

  return (
    <Card>
      <CardHeader>
        <CardTitle>Sweeps</CardTitle>
        <CardDescription>
          Confirmed funds moved from deposit addresses into the treasury (every
          5 minutes, above each asset&apos;s sweep minimum). Gas top-ups and sweep
          fees are gateway costs.
        </CardDescription>
        {isAdmin && (
          <CardAction>
            <RunSweepsDialog frozen={frozen} />
          </CardAction>
        )}
      </CardHeader>
      <CardContent className="space-y-4">
        <div className="flex flex-wrap gap-3">
          <Select
            value={network}
            onValueChange={(value) => {
              setNetwork(value)
              setOffset(0)
            }}
          >
            <SelectTrigger className="w-36">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value={ALL}>All networks</SelectItem>
              {SWEPT_NETWORKS.map((value) => (
                <SelectItem key={value} value={value}>
                  {networkLabel(value)}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          <Select
            value={status}
            onValueChange={(value) => {
              setStatus(value)
              setOffset(0)
            }}
          >
            <SelectTrigger className="w-36">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value={ALL}>All statuses</SelectItem>
              {SWEEP_STATUSES.map((value) => (
                <SelectItem key={value} value={value} className="capitalize">
                  {humanize(value)}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div>

        {sweeps.isError ? (
          <QueryError error={sweeps.error} />
        ) : sweeps.isLoading ? (
          <Skeleton className="h-40 w-full" />
        ) : (
          <>
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>Created</TableHead>
                  <TableHead>Asset</TableHead>
                  <TableHead>From</TableHead>
                  <TableHead className="text-right">Amount</TableHead>
                  <TableHead className="text-right">Unattributed</TableHead>
                  <TableHead className="text-right">Fee</TableHead>
                  <TableHead>Status</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {sweeps.data?.data.length === 0 && (
                  <TableRow>
                    <TableCell colSpan={7} className="h-20 text-center text-muted-foreground">
                      No sweeps yet.
                    </TableCell>
                  </TableRow>
                )}
                {sweeps.data?.data.map((sweep) => (
                  <TableRow key={sweep.id}>
                    <TableCell className="text-muted-foreground">
                      {formatDate(sweep.created_at)}
                    </TableCell>
                    <TableCell>
                      {networkLabel(sweep.network)} · {sweep.token}
                    </TableCell>
                    <TableCell>
                      {sweep.deposit_address_id ? (
                        <Link
                          to={`/payments/${sweep.deposit_address_id}`}
                          className="font-mono text-xs underline-offset-4 hover:underline"
                          title={sweep.from_address}
                        >
                          {shortHash(sweep.from_address, 6)}
                        </Link>
                      ) : (
                        <code className="text-xs" title={sweep.from_address}>
                          {shortHash(sweep.from_address, 6)}
                        </code>
                      )}
                    </TableCell>
                    <TableCell className="text-right tabular-nums">
                      {formatAmount(sweep.amount, sweep.token)}
                    </TableCell>
                    <TableCell className="text-right tabular-nums">
                      {(toBigInt(sweep.unattributed_raw) ?? 0n) > 0n ? (
                        <span className="inline-flex items-center gap-1 text-amber-700 dark:text-amber-400">
                          <TriangleAlert className="size-3.5" />
                          {formatAmount(sweep.unattributed, sweep.token)}
                        </span>
                      ) : (
                        <span className="text-muted-foreground">—</span>
                      )}
                    </TableCell>
                    <TableCell className="text-right tabular-nums text-muted-foreground">
                      {sweep.fee ? formatAmount(sweep.fee, sweep.fee_token) : "—"}
                    </TableCell>
                    <TableCell>
                      <StatusBadge status={sweep.status} />
                      {sweep.last_error && (
                        <p
                          className="max-w-48 truncate text-xs text-destructive"
                          title={sweep.last_error}
                        >
                          {sweep.last_error}
                        </p>
                      )}
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
            {sweeps.data && sweeps.data.total > PAGE_SIZE && (
              <Pagination
                total={sweeps.data.total}
                limit={PAGE_SIZE}
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

function RunSweepsDialog({ frozen }: { frozen: boolean }) {
  const [open, setOpen] = useState(false)
  const [network, setNetwork] = useState(ALL)
  const [code, setCode] = useState("")
  const run = useRunSweeps()

  function handleOpenChange(next: boolean) {
    if (!next && run.isPending) return
    setOpen(next)
    if (!next) {
      setNetwork(ALL)
      setCode("")
    }
  }

  async function submit(event: React.FormEvent) {
    event.preventDefault()
    if (!isTotpCode(code)) return
    try {
      const { planned } = await run.mutateAsync({
        network: network === ALL ? undefined : (network as Network),
        totp_code: code.trim(),
      })
      toast.success(
        planned === 0
          ? "Nothing to sweep right now"
          : `${planned} sweep${planned === 1 ? "" : "s"} planned`,
      )
      handleOpenChange(false)
    } catch (error) {
      toastError(error, "Could not run the sweeps")
    } finally {
      setCode("")
    }
  }

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      <DialogTrigger asChild>
        <Button
          size="sm"
          variant="outline"
          disabled={frozen}
          title={frozen ? "Outflows are frozen" : undefined}
        >
          <Play className="size-3.5" /> Run sweeps
        </Button>
      </DialogTrigger>
      <DialogContent className="sm:max-w-md">
        <form onSubmit={submit} className="space-y-4">
          <DialogHeader>
            <DialogTitle>Run sweeps now</DialogTitle>
            <DialogDescription>
              Plans a sweep for every deposit address holding confirmed funds above
              the minimum; the engine then executes them (with gas top-ups where
              needed).
            </DialogDescription>
          </DialogHeader>
          <div className="space-y-2">
            <Label>Network</Label>
            <Select value={network} onValueChange={setNetwork}>
              <SelectTrigger className="w-full">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value={ALL}>All networks</SelectItem>
                {SWEPT_NETWORKS.map((value) => (
                  <SelectItem key={value} value={value}>
                    {networkLabel(value)}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </div>
          <StepUpCodeField
            id="sweeps-code"
            value={code}
            onChange={setCode}
            disabled={run.isPending}
            autoFocus
          />
          <DialogFooter>
            <Button type="submit" disabled={!isTotpCode(code) || run.isPending}>
              {run.isPending ? "Planning…" : "Run sweeps"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}
