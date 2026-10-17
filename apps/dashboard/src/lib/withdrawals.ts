import { formatDate, minBigInt, toBigInt } from "@/lib/format"
import type {
  AlertType,
  DestinationKind,
  TreasuryRow,
  Withdrawal,
  WithdrawalStatus,
} from "@/lib/types"

export const WITHDRAWAL_STATUSES: WithdrawalStatus[] = [
  "pending_approval",
  "queued",
  "signed",
  "broadcast",
  "confirmed",
  "stuck",
  "failed",
  "cancelled",
]

export const DESTINATION_KIND_LABELS: Record<DestinationKind, string> = {
  address: "Address",
  ln_address: "Lightning address",
  bolt11: "BOLT11 invoice",
}

export const ALERT_LABELS: Record<AlertType, string> = {
  gas_reserve_low: "Gas reserve low",
  treasury_cap_exceeded: "Hot-wallet cap exceeded",
  address_book_entry_added: "Address-book entry added",
  withdrawal_stuck: "Withdrawal stuck",
  ledger_shortfall: "Ledger shortfall",
}

export type WithdrawalAction = "approve" | "reject" | "cancel" | "bump" | "abandon"

/** Which admin actions the withdrawal's status allows (the API enforces them too). */
export function availableActions(status: WithdrawalStatus): WithdrawalAction[] {
  switch (status) {
    case "pending_approval":
      return ["approve", "reject", "cancel"]
    case "queued":
      return ["cancel"]
    case "stuck":
      return ["bump", "abandon"]
    default:
      return []
  }
}

/**
 * Self-approval rule: the requesting admin cannot approve their own
 * withdrawal — unless it was created on a single-admin instance and
 * `self_approval_not_before` has passed. Returns why approval is blocked,
 * or null.
 */
export function approvalBlock(
  withdrawal: Withdrawal,
  staffId: string | undefined,
  now: number,
): string | null {
  const own = withdrawal.requested_by?.type === "staff" && withdrawal.requested_by.id === staffId
  if (!own) return null
  const notBefore = withdrawal.self_approval_not_before
  if (notBefore && new Date(notBefore).getTime() <= now) return null
  return notBefore
    ? `You requested it: another admin must approve it — or you, after ${formatDate(notBefore)} (single-admin instance).`
    : "You requested it: another admin must approve it."
}

/**
 * "Send excess to cold storage": `confirmed − cap` in base units (BigInt),
 * capped to what can actually leave (the available balance). null when the
 * treasury is within its cap or a figure is unknown.
 */
export function capExcessRaw(row: TreasuryRow): bigint | null {
  const confirmed = toBigInt(row.confirmed_raw)
  const cap = toBigInt(row.hot_wallet_cap_raw)
  if (confirmed === null || cap === null || confirmed <= cap) return null
  const available = toBigInt(row.available_raw)
  const excess = available === null ? confirmed - cap : minBigInt(confirmed - cap, available)
  return excess > 0n ? excess : null
}
