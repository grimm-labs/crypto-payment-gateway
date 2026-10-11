import { formatAmount } from "@/lib/format"
import { isGasOnlyToken } from "@/lib/networks"
import type { NetworkBalance, WalletBalances } from "@/lib/types"

/** Treasury vs. deposit addresses (not swept yet) for one (network, token). */
export function BalanceSplit({ balance }: { balance: NetworkBalance }) {
  // Lightning has no treasury/deposit split (one Spark wallet).
  if (balance.network === "lightning" || balance.deposits_amount === undefined) return null
  return (
    <dl className="mt-2 grid grid-cols-[auto_1fr] gap-x-3 gap-y-0.5 text-xs">
      <dt className="text-muted-foreground">Treasury</dt>
      <dd className="text-right tabular-nums">
        {balance.treasury_amount != null
          ? formatAmount(balance.treasury_amount, balance.token)
          : "unavailable"}
      </dd>
      {!isGasOnlyToken(balance.token) && (
        <>
          <dt className="text-muted-foreground">Deposit addresses</dt>
          <dd className="text-right tabular-nums">
            {formatAmount(balance.deposits_amount, balance.token)}
          </dd>
        </>
      )}
    </dl>
  )
}

/** ETH, and the TRX / SOL gas reserves, next to the USDT and BTC totals. */
export function OtherTotals({ balances }: { balances: WalletBalances }) {
  const totals = [
    { value: balances.total_eth, token: "ETH" },
    { value: balances.total_trx, token: "TRX", gas: true },
    { value: balances.total_sol, token: "SOL", gas: true },
  ].filter((total) => total.value !== undefined)
  if (totals.length === 0) return null
  return (
    <p className="mt-1 text-sm tabular-nums text-muted-foreground">
      {totals.map((total, index) => (
        <span key={total.token}>
          {index > 0 && " · "}
          {formatAmount(total.value as string, total.token)}
          {total.gas ? " (gas)" : ""}
        </span>
      ))}
    </p>
  )
}
