import { Link } from "react-router-dom"

import { ProjectBadge } from "@/components/shared/ProjectBadge"
import { QueryError } from "@/components/shared/QueryError"
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
import { useProjectBalance, useProjectBalancesOf } from "@/hooks/projects"
import { formatAmount, networkLabel, toBigInt } from "@/lib/format"
import type { Project, ProjectAssetBalance } from "@/lib/types"
import { cn } from "@/lib/utils"

const STATUS_LABEL: Record<string, string> = {
  ok: "reconciled",
  shortfall: "shortfall",
  unknown: "holdings unknown",
}

/** Signed decimal amount, red when negative. */
function SignedAmount({ value, token, className }: { value: string; token: string; className?: string }) {
  const negative = value.startsWith("-")
  return (
    <span className={cn("tabular-nums", negative && "text-red-600 dark:text-red-400", className)}>
      {formatAmount(value, token)}
    </span>
  )
}

function hasActivity(b: ProjectAssetBalance): boolean {
  return [
    b.deposits_raw,
    b.unattributed_raw,
    b.withdrawn_raw,
    b.fees_raw,
    b.transfers_in_raw,
    b.transfers_out_raw,
    b.reserved_raw,
    b.pending_raw,
  ].some((raw) => (toBigInt(raw) ?? 0n) !== 0n)
}

/** Overview with a project selected (spec §14.3): one card per asset with activity. */
export function ProjectBalanceCards({ project }: { project: Project }) {
  const balances = useProjectBalance(project.id)
  if (balances.isError) return <QueryError error={balances.error} />
  if (balances.isLoading) return <Skeleton className="h-40 w-full" />
  const active = (balances.data?.balances ?? []).filter(hasActivity)

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          Project balances <ProjectBadge projectId={project.id} />
        </CardTitle>
        <CardDescription>
          What the project brought into the gateway and has not taken out, per asset —
          a ledger over the pooled treasuries, recomputed from its deposits, sweeps,
          withdrawals and transfers. A key of the project cannot withdraw beyond
          &quot;available&quot;.
        </CardDescription>
      </CardHeader>
      <CardContent>
        {active.length === 0 ? (
          <p className="text-sm text-muted-foreground">No activity yet on any asset.</p>
        ) : (
          <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
            {active.map((b) => (
              <div key={`${b.network}-${b.token}`} className="rounded-lg border p-4">
                <p className="font-medium">
                  {networkLabel(b.network)} · {b.token}
                </p>
                <p className="mt-1 text-xl font-semibold">
                  <SignedAmount value={b.balance} token={b.token} />
                </p>
                <p className="text-sm text-muted-foreground">
                  Available <SignedAmount value={b.available} token={b.token} className="font-medium text-foreground" />
                </p>
                <dl className="mt-3 grid grid-cols-[auto_1fr] gap-x-3 gap-y-0.5 text-xs text-muted-foreground">
                  <dt>Deposits</dt><dd className="text-right tabular-nums">{b.deposits}</dd>
                  <dt>Unattributed</dt><dd className="text-right tabular-nums">{b.unattributed}</dd>
                  <dt>Withdrawn</dt><dd className="text-right tabular-nums">{b.withdrawn}</dd>
                  <dt>Fees</dt><dd className="text-right tabular-nums">{b.fees}</dd>
                  <dt>Transfers in / out</dt><dd className="text-right tabular-nums">{b.transfers_in} / {b.transfers_out}</dd>
                  <dt>Reserved</dt><dd className="text-right tabular-nums">{b.reserved}</dd>
                  <dt>Pending</dt><dd className="text-right tabular-nums">{b.pending}</dd>
                </dl>
              </div>
            ))}
          </div>
        )}
      </CardContent>
    </Card>
  )
}

/** Overview on "All projects" (spec §14.3): one table per token, a row per
 *  project; the merchant-funds row comes from the ledger when available. */
export function ProjectBalanceTables({
  projects,
  merchantRows,
}: {
  projects: Project[]
  /** Per token, per network: the merchant funds (signed decimal string, "" while unknown) and the status. */
  merchantRows?: Record<string, { network: string; amount: string; status: string }[]>
}) {
  const results = useProjectBalancesOf(projects.map((p) => p.id))
  const loading = results.some((r) => r.isLoading)
  const error = results.find((r) => r.error)?.error
  if (error) return <QueryError error={error} />
  if (loading) return <Skeleton className="h-40 w-full" />

  const byToken = new Map<string, Array<{ project: Project; balance: ProjectAssetBalance }>>()
  results.forEach((result, index) => {
    for (const balance of result.data?.balances ?? []) {
      if (!hasActivity(balance)) continue
      const rows = byToken.get(balance.token) ?? []
      rows.push({ project: projects[index], balance })
      byToken.set(balance.token, rows)
    }
  })
  // A token only the merchant funds hold (no project activity yet) still
  // gets its table, from the ledger rows.
  const tokens = [...new Set([...byToken.keys(), ...Object.keys(merchantRows ?? {})])].sort()

  return (
    <Card>
      <CardHeader>
        <CardTitle>Project balances</CardTitle>
        <CardDescription>
          Per token and project; select a project in the header for the detail.{" "}
          <Link to="/treasury" className="underline underline-offset-4">
            The Treasury page reconciles them with the holdings.
          </Link>
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-6">
        {tokens.length === 0 && (
          <p className="text-sm text-muted-foreground">No project activity yet.</p>
        )}
        {tokens.map((token) => (
          <div key={token}>
            <p className="mb-2 text-sm font-medium">{token}</p>
            <div className="rounded-lg border">
              <Table>
                <TableHeader>
                  <TableRow>
                    <TableHead>Project</TableHead>
                    <TableHead>Network</TableHead>
                    <TableHead className="text-right">Balance</TableHead>
                    <TableHead className="text-right">Available</TableHead>
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {(byToken.get(token) ?? []).map(({ project, balance }) => (
                    <TableRow key={`${project.id}-${balance.network}`}>
                      <TableCell>
                        <ProjectBadge projectId={project.id} />
                      </TableCell>
                      <TableCell>{networkLabel(balance.network)}</TableCell>
                      <TableCell className="text-right">
                        <SignedAmount value={balance.balance} token={token} />
                      </TableCell>
                      <TableCell className="text-right">
                        <SignedAmount value={balance.available} token={token} />
                      </TableCell>
                    </TableRow>
                  ))}
                  {merchantRows?.[token]?.map((row) => (
                    <TableRow key={`merchant-${row.network}`} className="bg-muted/40">
                      <TableCell className="font-medium">Merchant funds</TableCell>
                      <TableCell>
                        {networkLabel(row.network)}{" "}
                        <span
                          className={cn(
                            "text-xs",
                            row.status === "shortfall" ? "text-red-600 dark:text-red-400" : "text-muted-foreground",
                          )}
                        >
                          · {STATUS_LABEL[row.status] ?? row.status}
                        </span>
                      </TableCell>
                      <TableCell className="text-right text-muted-foreground" colSpan={2}>
                        {row.amount ? <SignedAmount value={row.amount} token={token} /> : "—"}
                      </TableCell>
                    </TableRow>
                  ))}
                </TableBody>
              </Table>
            </div>
          </div>
        ))}
      </CardContent>
    </Card>
  )
}
