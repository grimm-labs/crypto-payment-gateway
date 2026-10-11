import { Badge } from "@/components/ui/badge"
import { humanize } from "@/lib/format"
import { cn } from "@/lib/utils"

const AMBER = "bg-amber-100 text-amber-900 dark:bg-amber-900/40 dark:text-amber-200"
const EMERALD = "bg-emerald-100 text-emerald-900 dark:bg-emerald-900/40 dark:text-emerald-200"
const ZINC = "bg-zinc-100 text-zinc-600 dark:bg-zinc-800 dark:text-zinc-400"
const SKY = "bg-sky-100 text-sky-900 dark:bg-sky-900/40 dark:text-sky-200"
const RED = "bg-red-100 text-red-900 dark:bg-red-900/40 dark:text-red-200"
const VIOLET = "bg-violet-100 text-violet-900 dark:bg-violet-900/40 dark:text-violet-200"
const INDIGO = "bg-indigo-100 text-indigo-900 dark:bg-indigo-900/40 dark:text-indigo-200"
const ORANGE = "bg-orange-100 text-orange-900 dark:bg-orange-900/40 dark:text-orange-200"

const STYLES: Record<string, string> = {
  // deposit addresses
  pending: AMBER,
  paid: EMERALD,
  expired: ZINC,
  // deposits
  detected: SKY,
  confirmed: EMERALD,
  // lightning wallet history
  completed: EMERALD,
  sent: EMERALD,
  // webhooks
  delivered: EMERALD,
  failed: RED,
  // staff / api keys
  active: EMERALD,
  inactive: ZINC,
  blocked: RED,
  revoked: RED,
  admin: VIOLET,
  operator: SKY,
  // api traffic (HTTP status classes / surfaces)
  "2xx": EMERALD,
  "3xx": SKY,
  "4xx": AMBER,
  "5xx": RED,
  merchant: SKY,
  // withdrawals & outbound transactions
  pending_approval: AMBER,
  queued: SKY,
  signed: SKY,
  broadcast: INDIGO,
  cancelled: ZINC,
  stuck: ORANGE,
  replaced: ZINC,
  // sweeps
  planned: ZINC,
  topup_sent: SKY,
  ready: SKY,
  skipped: ZINC,
  // destination policies
  none: ZINC,
  book: SKY,
  open: VIOLET,
  // address book
  cooling_off: AMBER,
  // outflows
  frozen: RED,
}

export function StatusBadge({ status }: { status: string }) {
  return (
    <Badge variant="secondary" className={cn("capitalize", STYLES[status])}>
      {humanize(status)}
    </Badge>
  )
}
