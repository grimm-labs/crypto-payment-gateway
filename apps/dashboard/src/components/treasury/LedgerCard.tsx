import { useState } from "react"
import { ArrowLeftRight } from "lucide-react"

import { ProjectBadge } from "@/components/shared/ProjectBadge"
import { QueryError } from "@/components/shared/QueryError"
import { TransferDialog } from "@/components/transfers/TransferDialog"
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
import { useLedger } from "@/hooks/projects"
import { useIsAdmin } from "@/lib/auth"
import { formatAmount, networkLabel } from "@/lib/format"
import type { LedgerPair } from "@/lib/types"
import { cn } from "@/lib/utils"

function Amount({ value, token, muted }: { value: string | null; token: string; muted?: boolean }) {
  if (value === null) return <span className="text-muted-foreground">unknown</span>
  const negative = value.startsWith("-")
  return (
    <span className={cn("tabular-nums", negative && "text-red-600 dark:text-red-400", muted && "text-muted-foreground")}>
      {formatAmount(value, token)}
    </span>
  )
}

function StatusBadge({ status }: { status: LedgerPair["status"] }) {
  if (status === "shortfall") return <Badge variant="destructive">Shortfall</Badge>
  if (status === "unknown")
    return (
      <Badge variant="outline" className="text-muted-foreground">
        Unknown
      </Badge>
    )
  return <Badge variant="outline">OK</Badge>
}

/**
 * Treasury → Ledger (spec §14.3): what the gateway holds of each payment
 * pair against the project balances. The merchant funds are the
 * difference; a sustained shortfall raises an alert.
 */
export function LedgerCard() {
  const ledger = useLedger()
  const isAdmin = useIsAdmin()
  const [transferring, setTransferring] = useState(false)
  const rows = (ledger.data?.data ?? []).filter(
    (pair) => pair.projects.length > 0 || pair.holdings !== null,
  )

  return (
    <Card>
      <CardHeader>
        <CardTitle>Ledger</CardTitle>
        <CardDescription>
          Holdings (treasury plus unswept deposit addresses) against the sum of the
          project balances. The difference is the merchant funds: gas reserves and
          what no project is charged for. Negative = a shortfall; unknown = a
          disabled network, a provider error, or sweeping disabled for the pair.
        </CardDescription>
        {isAdmin && (
          <CardAction>
            <Button size="sm" variant="outline" onClick={() => setTransferring(true)}>
              <ArrowLeftRight className="size-3.5" /> New internal transfer
            </Button>
          </CardAction>
        )}
      </CardHeader>
      <CardContent>
        {ledger.isError ? (
          <QueryError error={ledger.error} />
        ) : ledger.isLoading ? (
          <Skeleton className="h-40 w-full" />
        ) : rows.length === 0 ? (
          <p className="text-sm text-muted-foreground">No holdings and no project activity yet.</p>
        ) : (
          <div className="rounded-lg border">
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>Asset</TableHead>
                  <TableHead className="text-right">Holdings</TableHead>
                  <TableHead>Projects</TableHead>
                  <TableHead className="text-right">Project total</TableHead>
                  <TableHead className="text-right">Merchant funds</TableHead>
                  <TableHead>Status</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {rows.map((pair) => (
                  <TableRow key={`${pair.network}-${pair.token}`} className={cn(pair.status === "unknown" && "text-muted-foreground")}>
                    <TableCell className="font-medium">
                      {networkLabel(pair.network)} · {pair.token}
                    </TableCell>
                    <TableCell className="text-right">
                      <Amount value={pair.holdings} token={pair.token} />
                    </TableCell>
                    <TableCell>
                      <div className="flex flex-col gap-1">
                        {pair.projects.length === 0 && <span className="text-xs text-muted-foreground">—</span>}
                        {pair.projects.map((project) => (
                          <div key={project.project_id} className="flex items-center justify-between gap-3 text-xs">
                            <ProjectBadge projectId={project.project_id} />
                            <Amount value={project.balance} token={pair.token} />
                          </div>
                        ))}
                      </div>
                    </TableCell>
                    <TableCell className="text-right">
                      <Amount value={pair.project_total} token={pair.token} />
                    </TableCell>
                    <TableCell className="text-right font-medium">
                      <Amount value={pair.merchant_funds} token={pair.token} />
                    </TableCell>
                    <TableCell>
                      <StatusBadge status={pair.status} />
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          </div>
        )}
      </CardContent>
      {isAdmin && <TransferDialog open={transferring} onOpenChange={setTransferring} />}
    </Card>
  )
}
