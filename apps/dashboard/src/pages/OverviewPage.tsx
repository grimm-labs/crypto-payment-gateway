import { ChartAreaInteractive } from "@/components/chart-area-interactive"
import { ProjectBalanceCards, ProjectBalanceTables } from "@/components/projects/ProjectBalanceCards"
import { SectionCards } from "@/components/section-cards"
import { BalanceSplit } from "@/components/shared/BalanceSplit"
import { StatusBadge } from "@/components/shared/StatusBadge"
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card"
import { Skeleton } from "@/components/ui/skeleton"
import { useLedger } from "@/hooks/projects"
import { useWalletBalances } from "@/hooks/queries"
import { formatAmount, networkLabel } from "@/lib/format"
import { assetKey } from "@/lib/networks"
import { useProjectScope } from "@/lib/project-scope"

export function OverviewPage() {
  const balances = useWalletBalances()
  const { projects, selectedProject } = useProjectScope()
  const ledger = useLedger()
  // Merchant-funds rows per token for the "All projects" tables.
  const merchantRows: Record<string, { network: string; amount: string; status: string }[]> = {}
  for (const pair of ledger.data?.data ?? []) {
    if (pair.merchant_funds === null && pair.projects.length === 0) continue
    ;(merchantRows[pair.token] ??= []).push({
      network: pair.network,
      amount: pair.merchant_funds ?? "",
      status: pair.status,
    })
  }

  return (
    <div className="flex flex-col gap-4 py-4 md:gap-6 md:py-6">
      <SectionCards />
      <div className="px-4 lg:px-6">
        <ChartAreaInteractive />
      </div>
      <div className="px-4 lg:px-6">
        {selectedProject ? (
          <ProjectBalanceCards project={selectedProject} />
        ) : (
          <ProjectBalanceTables projects={projects} merchantRows={merchantRows} />
        )}
      </div>
      {!selectedProject && (
      <div className="px-4 lg:px-6">
        <Card>
          <CardHeader>
            <CardTitle>Wallet balances by network and token</CardTitle>
            <CardDescription>
              From the hourly HD-wallet sync — see the Wallets page for a live refresh.
            </CardDescription>
          </CardHeader>
          <CardContent>
            {balances.isLoading ? (
              <Skeleton className="h-24 w-full" />
            ) : (
              <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
                {balances.data?.networks.map((network) => (
                  <div key={assetKey(network)} className="rounded-lg border p-4">
                    <div className="flex items-center justify-between">
                      <p className="font-medium">
                        {networkLabel(network.network)} · {network.token}
                      </p>
                      {!network.available && <StatusBadge status="inactive" />}
                    </div>
                    <p className="mt-1 text-xl font-semibold tabular-nums">
                      {formatAmount(network.amount, network.token)}
                    </p>
                    <BalanceSplit balance={network} />
                    {network.message && (
                      <p className="mt-1 text-xs text-muted-foreground">{network.message}</p>
                    )}
                  </div>
                ))}
              </div>
            )}
          </CardContent>
        </Card>
      </div>
      )}
    </div>
  )
}
