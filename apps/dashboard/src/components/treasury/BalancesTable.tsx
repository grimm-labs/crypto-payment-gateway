import { TriangleAlert } from "lucide-react"

import { CopyButton } from "@/components/shared/CopyButton"
import { StatusBadge } from "@/components/shared/StatusBadge"
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table"
import { formatRaw, networkLabel, shortHash, toBigInt } from "@/lib/format"
import type { TreasuryRow } from "@/lib/types"

function Address({ label, value }: { label?: string; value: string }) {
  return (
    <div className="flex items-center gap-1">
      {label && <span className="text-xs text-muted-foreground">{label}</span>}
      <code className="text-xs" title={value}>
        {shortHash(value, 6)}
      </code>
      <CopyButton value={value} />
    </div>
  )
}

/** Per (network, token): treasury address and its confirmed / reserved /
 *  available balances, plus what still sits on deposit addresses. */
export function BalancesTable({ rows }: { rows: TreasuryRow[] }) {
  return (
    <div className="rounded-lg border">
      <Table>
        <TableHeader>
          <TableRow>
            <TableHead>Asset</TableHead>
            <TableHead>Treasury</TableHead>
            <TableHead className="text-right">Confirmed</TableHead>
            <TableHead className="text-right">Reserved</TableHead>
            <TableHead className="text-right">Available</TableHead>
            <TableHead className="text-right">Unswept</TableHead>
            <TableHead className="text-right">Unattributed</TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          {rows.length === 0 && (
            <TableRow>
              <TableCell colSpan={7} className="h-24 text-center text-muted-foreground">
                No treasury configured.
              </TableCell>
            </TableRow>
          )}
          {rows.map((row) => {
            const reservedFees = toBigInt(row.reserved_fees_raw) ?? 0n
            const unattributed = toBigInt(row.unattributed_raw) ?? 0n
            return (
              <TableRow key={`${row.network}-${row.token}`}>
                <TableCell>
                  <p className="font-medium">
                    {networkLabel(row.network)} · {row.token}
                  </p>
                  {!row.available && (
                    <div className="mt-0.5 flex items-center gap-1.5">
                      <StatusBadge status="inactive" />
                      {row.message && (
                        <span className="text-xs text-muted-foreground">{row.message}</span>
                      )}
                    </div>
                  )}
                </TableCell>
                <TableCell>
                  {row.treasury_address ? (
                    <Address value={row.treasury_address} />
                  ) : (
                    <span className="text-xs text-muted-foreground">
                      {row.network === "lightning" ? "Spark wallet" : "—"}
                    </span>
                  )}
                  {row.token_account && <Address label="Token account" value={row.token_account} />}
                </TableCell>
                <TableCell className="text-right tabular-nums">
                  {formatRaw(row.confirmed_raw, row.token)}
                </TableCell>
                <TableCell className="text-right tabular-nums">
                  {formatRaw(row.reserved_raw, row.token)}
                  {reservedFees > 0n && (
                    <p className="text-xs text-muted-foreground">
                      + {formatRaw(reservedFees, row.token)} fees
                    </p>
                  )}
                </TableCell>
                <TableCell className="text-right font-medium tabular-nums">
                  {formatRaw(row.available_raw, row.token)}
                </TableCell>
                <TableCell className="text-right tabular-nums text-muted-foreground">
                  {formatRaw(row.unswept_raw, row.token)}
                </TableCell>
                <TableCell className="text-right tabular-nums">
                  {unattributed > 0n ? (
                    <span
                      className="inline-flex items-center gap-1 text-amber-700 dark:text-amber-400"
                      title="Funds that settled no payment (outside tolerance, after expiry, wrong asset) — swept like the rest"
                    >
                      <TriangleAlert className="size-3.5" />
                      {formatRaw(unattributed, row.token)}
                    </span>
                  ) : (
                    <span className="text-muted-foreground">—</span>
                  )}
                </TableCell>
              </TableRow>
            )
          })}
        </TableBody>
      </Table>
    </div>
  )
}
