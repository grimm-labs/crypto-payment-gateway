import { RefreshCw } from "lucide-react"
import { Link } from "react-router-dom"
import { toast } from "sonner"

import { BalanceSplit, OtherTotals } from "@/components/shared/BalanceSplit"
import { StatusBadge } from "@/components/shared/StatusBadge"
import { Button } from "@/components/ui/button"
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card"
import { Skeleton } from "@/components/ui/skeleton"
import { useLiveBalances, useWalletBalances } from "@/hooks/queries"
import { useIsAdmin } from "@/lib/auth"
import { formatAmount, formatDate, networkLabel } from "@/lib/format"
import { assetKey, isGasOnlyToken } from "@/lib/networks"
import type { WalletBalances } from "@/lib/types"

export function WalletsPage() {
  const cached = useWalletBalances()
  const live = useLiveBalances()
  // The Lightning history is admin-only.
  const isAdmin = useIsAdmin()

  async function refreshLive() {
    toast.info("Querying balances on-chain — this can take a while…")
    const result = await live.refetch()
    if (result.error) {
      toast.error(result.error instanceof Error ? result.error.message : "Live refresh failed")
    } else {
      toast.success("Live balances loaded")
    }
  }

  const source: WalletBalances | undefined = live.data ?? cached.data
  const isLive = live.data !== undefined

  return (
    <div className="space-y-6 p-4 lg:p-6">
      <div className="flex flex-wrap items-end justify-between gap-4">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">Wallets</h1>
          <p className="text-sm text-muted-foreground">
            Per network and token: the treasury plus deposit addresses not swept yet,
            and the Lightning (Breez/Spark) wallet balance. TRX and SOL are gas
            reserves.
          </p>
        </div>
        <Button onClick={refreshLive} disabled={live.isFetching} variant="outline">
          <RefreshCw className={live.isFetching ? "size-4 animate-spin" : "size-4"} />
          {live.isFetching ? "Refreshing on-chain…" : "Refresh live"}
        </Button>
      </div>

      {cached.isLoading ? (
        <Skeleton className="h-64 w-full" />
      ) : (
        <>
          <Card>
            <CardHeader className="pb-2">
              <CardDescription>
                Total balance{" "}
                {isLive ? "(live)" : source?.updated_at ? `(synced ${formatDate(source.updated_at)})` : "(no sync yet)"}
              </CardDescription>
            </CardHeader>
            <CardContent>
              <p className="text-3xl font-semibold tabular-nums">
                {source ? formatAmount(source.total_usdt, "USDT") : "—"}
              </p>
              {source?.total_btc !== undefined && (
                <p className="mt-1 text-xl font-semibold tabular-nums text-muted-foreground">
                  {formatAmount(source.total_btc, "BTC")}
                </p>
              )}
              {source && <OtherTotals balances={source} />}
            </CardContent>
          </Card>

          <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
            {source?.networks.map((network) => (
              <Card key={assetKey(network)}>
                <CardHeader className="pb-2">
                  <div className="flex items-center justify-between">
                    <CardTitle className="text-base">
                      {networkLabel(network.network)} · {network.token}
                      {isGasOnlyToken(network.token) && (
                        <span className="ml-1 text-xs font-normal text-muted-foreground">
                          gas reserve
                        </span>
                      )}
                    </CardTitle>
                    {!network.available && <StatusBadge status="inactive" />}
                  </div>
                </CardHeader>
                <CardContent>
                  <p className="text-2xl font-semibold tabular-nums">
                    {formatAmount(network.amount, network.token)}
                  </p>
                  <BalanceSplit balance={network} />
                  <p className="mt-1 text-xs text-muted-foreground">
                    {network.network === "lightning"
                      ? "Breez/Spark wallet balance"
                      : network.indices_scanned !== undefined
                        ? `${network.indices_scanned} HD indices scanned`
                        : network.addresses_checked !== undefined &&
                          `${network.addresses_checked} addresses checked`}
                  </p>
                  {network.message && (
                    <p className="mt-1 text-xs text-muted-foreground">{network.message}</p>
                  )}
                  {isAdmin && network.network === "lightning" && network.available && (
                    <Link
                      to="/lightning"
                      className="mt-2 inline-block text-xs underline-offset-4 hover:underline"
                    >
                      View transaction history →
                    </Link>
                  )}
                </CardContent>
              </Card>
            ))}
          </div>

          <p className="text-xs text-muted-foreground">
            Cached balances refresh hourly in the background. “Refresh live” queries every
            address on-chain and can take several seconds per network.
          </p>
        </>
      )}
    </div>
  )
}
