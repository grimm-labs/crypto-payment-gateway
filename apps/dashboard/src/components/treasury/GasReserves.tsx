import { Fuel } from "lucide-react"

import { Badge } from "@/components/ui/badge"
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card"
import { formatRaw, networkLabel, toBigInt } from "@/lib/format"
import type { TreasuryRow } from "@/lib/types"

/** Gas-asset rows (ETH, TRX, SOL): balance against the floor and the alert threshold. */
export function GasReserves({ rows }: { rows: TreasuryRow[] }) {
  const reserves = rows.filter(
    (row) => row.gas_floor_raw !== null || row.gas_alert_below_raw !== null,
  )

  return (
    <Card>
      <CardHeader>
        <CardTitle>Gas reserves</CardTitle>
        <CardDescription>
          Native coins funded by the operator to pay network fees and gas top-ups.
          Below the floor, gas top-ups and withdrawals of the gas asset stop and
          sweeps wait.
        </CardDescription>
      </CardHeader>
      <CardContent>
        {reserves.length === 0 ? (
          <p className="text-sm text-muted-foreground">
            No gas floor or alert threshold configured — set them in Settings → Assets.
          </p>
        ) : (
          <div className="grid gap-4 sm:grid-cols-3">
            {reserves.map((row) => {
              const balance = toBigInt(row.confirmed_raw)
              const floor = toBigInt(row.gas_floor_raw)
              const belowFloor = balance !== null && floor !== null && balance < floor
              return (
                <div
                  key={`${row.network}-${row.token}`}
                  className={
                    belowFloor
                      ? "rounded-lg border border-red-300 bg-red-50 p-4 dark:border-red-900 dark:bg-red-950/40"
                      : row.gas_low
                        ? "rounded-lg border border-amber-300 bg-amber-50 p-4 dark:border-amber-800 dark:bg-amber-950/40"
                        : "rounded-lg border p-4"
                  }
                >
                  <div className="flex items-center justify-between gap-2">
                    <p className="flex items-center gap-1.5 font-medium">
                      <Fuel className="size-4 text-muted-foreground" />
                      {networkLabel(row.network)} · {row.token}
                    </p>
                    {belowFloor ? (
                      <Badge variant="destructive">Below floor</Badge>
                    ) : row.gas_low ? (
                      <Badge className="bg-amber-500 text-white">Low</Badge>
                    ) : (
                      row.available && <Badge variant="outline">OK</Badge>
                    )}
                  </div>
                  <p className="mt-2 text-xl font-semibold tabular-nums">
                    {formatRaw(row.confirmed_raw, row.token)}
                  </p>
                  <dl className="mt-2 grid grid-cols-[auto_1fr] gap-x-3 gap-y-0.5 text-xs">
                    <dt className="text-muted-foreground">Floor</dt>
                    <dd className="tabular-nums">{formatRaw(row.gas_floor_raw, row.token)}</dd>
                    <dt className="text-muted-foreground">Alert below</dt>
                    <dd className="tabular-nums">
                      {formatRaw(row.gas_alert_below_raw, row.token)}
                    </dd>
                  </dl>
                  {(belowFloor || row.gas_low) && (
                    <p className="mt-2 text-xs">
                      Fund the {networkLabel(row.network)} treasury with {row.token}.
                    </p>
                  )}
                  {!row.available && row.message && (
                    <p className="mt-2 text-xs text-muted-foreground">{row.message}</p>
                  )}
                </div>
              )
            })}
          </div>
        )}
      </CardContent>
    </Card>
  )
}
