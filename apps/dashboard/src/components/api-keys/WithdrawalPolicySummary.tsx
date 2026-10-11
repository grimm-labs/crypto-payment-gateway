import { Link } from "react-router-dom"

import { StatusBadge } from "@/components/shared/StatusBadge"
import { networkLabel, rawToDecimal } from "@/lib/format"
import type { ApiKey } from "@/lib/types"

/** Compact view of a key's withdrawal grant for the key list. */
export function WithdrawalPolicySummary({ apiKey }: { apiKey: ApiKey }) {
  if (apiKey.withdrawal_policy === "none") {
    return <StatusBadge status="none" />
  }
  return (
    <div className="space-y-0.5">
      <div className="flex items-center gap-1.5">
        <StatusBadge status={apiKey.withdrawal_policy} />
        <span className="text-xs">{apiKey.withdrawal_tokens.join(", ")}</span>
      </div>
      <p className="text-xs text-muted-foreground">
        {apiKey.withdrawal_networks === null
          ? "All networks of these tokens"
          : apiKey.withdrawal_networks.map(networkLabel).join(", ")}
      </p>
      {apiKey.withdrawal_limits.map((limit) => (
        <p key={limit.token} className="text-xs tabular-nums text-muted-foreground">
          {limit.token}: {limit.max_per_withdrawal ?? rawToDecimal(limit.max_per_withdrawal_raw, limit.token)}{" "}
          max · {limit.daily_max ?? rawToDecimal(limit.daily_max_raw, limit.token)} / 24 h
        </p>
      ))}
      <Link
        to={`/withdrawals?api_key_id=${encodeURIComponent(apiKey.id)}`}
        className="text-xs underline-offset-4 hover:underline"
      >
        Withdrawals →
      </Link>
    </div>
  )
}
