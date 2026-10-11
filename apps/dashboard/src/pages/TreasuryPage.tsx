import { OctagonX, RefreshCw, ShieldCheck, SlidersHorizontal } from "lucide-react"
import { Link } from "react-router-dom"

import { FreezeButton, UnfreezeButton } from "@/components/outflows/OutflowControls"
import { QueryError } from "@/components/shared/QueryError"
import { AlertsCard } from "@/components/treasury/AlertsCard"
import { BalancesTable } from "@/components/treasury/BalancesTable"
import { GasReserves } from "@/components/treasury/GasReserves"
import { HotWalletCaps } from "@/components/treasury/HotWalletCaps"
import { SweepsCard } from "@/components/treasury/SweepsCard"
import { Button } from "@/components/ui/button"
import { Card, CardContent } from "@/components/ui/card"
import { Skeleton } from "@/components/ui/skeleton"
import { useOutflows, useTreasury } from "@/hooks/withdrawals"

export function TreasuryPage() {
  const treasury = useTreasury()
  const outflows = useOutflows()
  const frozen = outflows.data?.frozen ?? treasury.data?.outflows_frozen ?? false

  return (
    <div className="space-y-6 p-4 lg:p-6">
      <div className="flex flex-wrap items-end justify-between gap-4">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">Treasury</h1>
          <p className="max-w-3xl text-sm text-muted-foreground">
            One treasury per network holds swept funds; on-chain withdrawals leave
            from it. Available = confirmed − reserved by pending withdrawals (and,
            for a gas asset, its floor and reserved fees).
          </p>
        </div>
        <div className="flex gap-2">
          <Button asChild variant="outline">
            <Link to="/settings/assets">
              <SlidersHorizontal className="size-4" /> Limits &amp; thresholds
            </Link>
          </Button>
          <Button
            variant="outline"
            onClick={() => treasury.refetch()}
            disabled={treasury.isFetching}
          >
            <RefreshCw className={treasury.isFetching ? "size-4 animate-spin" : "size-4"} />
            Refresh
          </Button>
        </div>
      </div>

      <OutflowCard
        frozen={frozen}
        forcedByEnvironment={outflows.data?.forced_by_environment ?? false}
        loading={outflows.isLoading}
      />

      {treasury.isError ? (
        <QueryError error={treasury.error} />
      ) : treasury.isLoading || !treasury.data ? (
        <Skeleton className="h-64 w-full" />
      ) : (
        <>
          <BalancesTable rows={treasury.data.data} />
          <div className="grid gap-6 xl:grid-cols-2">
            <GasReserves rows={treasury.data.data} />
            <HotWalletCaps rows={treasury.data.data} />
          </div>
        </>
      )}

      <SweepsCard frozen={frozen} />
      <AlertsCard />
    </div>
  )
}

/** The outflow freeze, front and centre: any staff member may freeze. */
function OutflowCard({
  frozen,
  forcedByEnvironment,
  loading,
}: {
  frozen: boolean
  forcedByEnvironment: boolean
  loading: boolean
}) {
  if (loading) return <Skeleton className="h-20 w-full" />
  return (
    <Card
      className={
        frozen ? "border-red-300 bg-red-50 dark:border-red-900 dark:bg-red-950/40" : undefined
      }
    >
      <CardContent className="flex flex-wrap items-center justify-between gap-4">
        <div className="flex items-start gap-3">
          {frozen ? (
            <OctagonX className="mt-0.5 size-5 shrink-0 text-red-600" />
          ) : (
            <ShieldCheck className="mt-0.5 size-5 shrink-0 text-emerald-600" />
          )}
          <div>
            <p className="font-medium">
              {frozen ? "Outflows are frozen" : "Outflows are running"}
            </p>
            <p className="text-sm text-muted-foreground">
              {frozen
                ? forcedByEnvironment
                  ? "Forced by OUTFLOWS_FROZEN in the environment — change it to lift the freeze."
                  : "No withdrawal, sweep or gas top-up is signed or broadcast. Quotes, approvals and cancellations still work."
                : "In an emergency, freeze them: nothing leaves the gateway until an admin lifts the freeze."}
            </p>
          </div>
        </div>
        {frozen ? (
          <UnfreezeButton forcedByEnvironment={forcedByEnvironment} />
        ) : (
          <FreezeButton />
        )}
      </CardContent>
    </Card>
  )
}
