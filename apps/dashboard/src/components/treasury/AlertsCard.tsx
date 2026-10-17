import { useState } from "react"
import { BellRing } from "lucide-react"
import { Link } from "react-router-dom"

import { Pagination } from "@/components/shared/Pagination"
import { QueryError } from "@/components/shared/QueryError"
import { Badge } from "@/components/ui/badge"
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card"
import { Skeleton } from "@/components/ui/skeleton"
import { useAlerts } from "@/hooks/withdrawals"
import { formatDate, humanize, networkLabel } from "@/lib/format"
import type { Alert, AlertType } from "@/lib/types"
import { cn } from "@/lib/utils"
import { ALERT_LABELS } from "@/lib/withdrawals"

const PAGE_SIZE = 10

const TONES: Record<AlertType, string> = {
  gas_reserve_low: "bg-amber-100 text-amber-900 dark:bg-amber-900/40 dark:text-amber-200",
  treasury_cap_exceeded: "bg-orange-100 text-orange-900 dark:bg-orange-900/40 dark:text-orange-200",
  address_book_entry_added: "bg-sky-100 text-sky-900 dark:bg-sky-900/40 dark:text-sky-200",
  withdrawal_stuck: "bg-red-100 text-red-900 dark:bg-red-900/40 dark:text-red-200",
  ledger_shortfall: "bg-red-100 text-red-900 dark:bg-red-900/40 dark:text-red-200",
}

function detailValue(value: unknown): string {
  if (value === null || value === undefined) return "—"
  return typeof value === "string" ? value : JSON.stringify(value)
}

function AlertDetails({ alert }: { alert: Alert }) {
  const entries = Object.entries(alert.details ?? {})
  if (entries.length === 0) return null
  return (
    <dl className="mt-1 grid grid-cols-[auto_1fr] gap-x-3 gap-y-0.5 text-xs">
      {entries.map(([key, value]) => (
        <div key={key} className="contents">
          <dt className="text-muted-foreground">{humanize(key)}</dt>
          <dd className="min-w-0 break-all font-mono">
            {key === "withdrawal_id" && typeof value === "string" ? (
              <Link to={`/withdrawals/${value}`} className="underline underline-offset-4">
                {value}
              </Link>
            ) : (
              detailValue(value)
            )}
          </dd>
        </div>
      ))}
    </dl>
  )
}

/** Operational alerts (deduplicated server-side), newest first. */
export function AlertsCard() {
  const [offset, setOffset] = useState(0)
  const alerts = useAlerts({ limit: PAGE_SIZE, offset })

  return (
    <Card>
      <CardHeader>
        <CardTitle>Recent alerts</CardTitle>
        <CardDescription>
          Low gas reserves, exceeded hot-wallet caps, new address-book entries and
          stuck withdrawals — also sent to webhook endpoints subscribed to them.
        </CardDescription>
      </CardHeader>
      <CardContent>
        {alerts.isError ? (
          <QueryError error={alerts.error} />
        ) : alerts.isLoading ? (
          <Skeleton className="h-32 w-full" />
        ) : alerts.data?.data.length === 0 ? (
          <p className="py-6 text-center text-sm text-muted-foreground">No alerts.</p>
        ) : (
          <>
            <ul className="divide-y">
              {alerts.data?.data.map((alert) => (
                <li key={alert.alert_id} className="flex gap-3 py-3">
                  <BellRing className="mt-0.5 size-4 shrink-0 text-muted-foreground" />
                  <div className="min-w-0 flex-1">
                    <div className="flex flex-wrap items-center gap-2">
                      <Badge variant="secondary" className={cn(TONES[alert.type])}>
                        {ALERT_LABELS[alert.type] ?? humanize(alert.type)}
                      </Badge>
                      {alert.network && (
                        <span className="text-sm">
                          {networkLabel(alert.network)}
                          {alert.token ? ` · ${alert.token}` : ""}
                        </span>
                      )}
                      <span className="ml-auto text-xs text-muted-foreground">
                        {formatDate(alert.created_at)}
                      </span>
                    </div>
                    <AlertDetails alert={alert} />
                  </div>
                </li>
              ))}
            </ul>
            {alerts.data && alerts.data.total > PAGE_SIZE && (
              <Pagination
                total={alerts.data.total}
                limit={PAGE_SIZE}
                offset={offset}
                onOffsetChange={setOffset}
              />
            )}
          </>
        )}
      </CardContent>
    </Card>
  )
}
