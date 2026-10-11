import { useEffect, useState } from "react"
import { Braces, Search, X } from "lucide-react"
import { useSearchParams } from "react-router-dom"

import { CopyButton } from "@/components/shared/CopyButton"
import { JsonViewer } from "@/components/shared/JsonViewer"
import { Pagination } from "@/components/shared/Pagination"
import { QueryError } from "@/components/shared/QueryError"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { Skeleton } from "@/components/ui/skeleton"
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table"
import { useAuditLog } from "@/hooks/queries"
import { formatDate, shortHash } from "@/lib/format"
import type { AuditActorType, AuditLogEntry } from "@/lib/types"
import { cn } from "@/lib/utils"

const PAGE_SIZE = 25
const FILTER_DEBOUNCE_MS = 300

/** Suggestions for the free-text filter. */
const KNOWN_ACTIONS = [
  "setup.completed",
  "auth.password_changed",
  "auth.totp_enabled",
  "auth.totp_disabled",
  "auth.sessions_revoked",
  "staff.created",
  "staff.updated",
  "staff.totp_reset",
  "api_key.created",
  "api_key.updated",
  "api_key.revoked",
  "settings.updated",
  "webhook_endpoint.created",
  "webhook_endpoint.updated",
  "webhook_endpoint.deleted",
  "payout.requested",
  "payout.sent",
  "payout.failed",
  "webhook.retried",
]

const ACTOR_LABELS: Record<AuditActorType, string> = {
  staff: "Staff",
  api_key: "API key",
  system: "System",
}

const ACTION_STYLES: Array<[RegExp, string]> = [
  [/^payout\.failed$/, "bg-red-100 text-red-900 dark:bg-red-900/40 dark:text-red-200"],
  [/^payout\./, "bg-amber-100 text-amber-900 dark:bg-amber-900/40 dark:text-amber-200"],
  [/^(auth|setup)\./, "bg-violet-100 text-violet-900 dark:bg-violet-900/40 dark:text-violet-200"],
  [/^(staff|api_key)\./, "bg-sky-100 text-sky-900 dark:bg-sky-900/40 dark:text-sky-200"],
]

function actionClass(action: string): string | undefined {
  return ACTION_STYLES.find(([pattern]) => pattern.test(action))?.[1]
}

function Actor({ entry }: { entry: AuditLogEntry }) {
  const label = entry.actor_name ?? (entry.actor_id ? shortHash(entry.actor_id, 6) : null)
  return (
    <div className="leading-tight">
      <p className={cn("text-sm", !label && "text-muted-foreground")}>
        {label ?? ACTOR_LABELS[entry.actor_type] ?? entry.actor_type}
      </p>
      {label && (
        <p className="text-xs text-muted-foreground">
          {ACTOR_LABELS[entry.actor_type] ?? entry.actor_type}
        </p>
      )}
    </div>
  )
}

function Target({ entry }: { entry: AuditLogEntry }) {
  if (!entry.target_type && !entry.target_id) {
    return <span className="text-xs text-muted-foreground">—</span>
  }
  return (
    <div className="leading-tight">
      {entry.target_type && <p className="text-sm">{entry.target_type.replace(/_/g, " ")}</p>}
      {entry.target_id && (
        <div className="flex items-center gap-1">
          <code className="text-xs text-muted-foreground" title={entry.target_id}>
            {shortHash(entry.target_id, 6)}
          </code>
          <CopyButton value={entry.target_id} />
        </div>
      )}
    </div>
  )
}

export function AuditLogPage() {
  const [searchParams, setSearchParams] = useSearchParams()
  const action = searchParams.get("action") ?? ""
  const offset = Math.max(0, Number(searchParams.get("offset") ?? 0) || 0)
  const [filter, setFilter] = useState(action)
  const [syncedAction, setSyncedAction] = useState(action)
  const [detail, setDetail] = useState<AuditLogEntry | null>(null)

  // Follow URL changes made elsewhere (back/forward navigation).
  if (action !== syncedAction) {
    setSyncedAction(action)
    setFilter(action)
  }

  const log = useAuditLog({ action: action || undefined, limit: PAGE_SIZE, offset })

  // Typing updates the URL (and the query) once the user pauses.
  useEffect(() => {
    const next = filter.trim()
    if (next === action) return
    const timer = setTimeout(() => {
      const params = new URLSearchParams(searchParams)
      if (next) params.set("action", next)
      else params.delete("action")
      params.delete("offset")
      setSearchParams(params, { replace: true })
    }, FILTER_DEBOUNCE_MS)
    return () => clearTimeout(timer)
  }, [filter, action, searchParams, setSearchParams])

  function goTo(next: number) {
    const params = new URLSearchParams(searchParams)
    if (next > 0) params.set("offset", String(next))
    else params.delete("offset")
    setSearchParams(params)
  }

  return (
    <div className="space-y-6 p-4 lg:p-6">
      <div>
        <h1 className="text-2xl font-semibold tracking-tight">Audit log</h1>
        <p className="text-sm text-muted-foreground">
          Sensitive actions — sign-in security, staff and API key changes,
          settings, webhook endpoints, payouts and manual retries — newest first.
        </p>
      </div>

      <div className="relative w-full max-w-xs">
        <Search className="pointer-events-none absolute top-1/2 left-2.5 size-3.5 -translate-y-1/2 text-muted-foreground" />
        <Input
          className="pr-8 pl-8"
          placeholder="Filter by action, e.g. payout.sent"
          list="audit-actions"
          value={filter}
          onChange={(event) => setFilter(event.target.value)}
          aria-label="Filter by action"
        />
        {filter && (
          <Button
            variant="ghost"
            size="icon-xs"
            className="absolute top-1/2 right-1 -translate-y-1/2"
            onClick={() => setFilter("")}
            aria-label="Clear filter"
          >
            <X />
          </Button>
        )}
        <datalist id="audit-actions">
          {KNOWN_ACTIONS.map((value) => (
            <option key={value} value={value} />
          ))}
        </datalist>
      </div>

      {log.isError ? (
        <QueryError error={log.error} />
      ) : log.isLoading ? (
        <Skeleton className="h-64 w-full" />
      ) : (
        <>
          <div className="rounded-lg border">
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>Date</TableHead>
                  <TableHead>Actor</TableHead>
                  <TableHead>Action</TableHead>
                  <TableHead>Target</TableHead>
                  <TableHead>IP</TableHead>
                  <TableHead className="text-right">Details</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {log.data?.data.length === 0 && (
                  <TableRow>
                    <TableCell colSpan={6} className="h-24 text-center text-muted-foreground">
                      {action ? `No entries for “${action}”.` : "No audit entries yet."}
                    </TableCell>
                  </TableRow>
                )}
                {log.data?.data.map((entry) => {
                  const hasDetails = Object.keys(entry.details ?? {}).length > 0
                  return (
                    <TableRow key={entry.id}>
                      <TableCell className="whitespace-nowrap text-muted-foreground">
                        {formatDate(entry.created_at)}
                      </TableCell>
                      <TableCell>
                        <Actor entry={entry} />
                      </TableCell>
                      <TableCell>
                        <Badge
                          variant="secondary"
                          className={cn("font-mono text-[11px]", actionClass(entry.action))}
                        >
                          {entry.action}
                        </Badge>
                      </TableCell>
                      <TableCell>
                        <Target entry={entry} />
                      </TableCell>
                      <TableCell>
                        {entry.ip ? (
                          <code className="text-xs">{entry.ip}</code>
                        ) : (
                          <span className="text-xs text-muted-foreground">—</span>
                        )}
                      </TableCell>
                      <TableCell className="text-right">
                        <Button
                          variant="outline"
                          size="sm"
                          disabled={!hasDetails}
                          title={hasDetails ? undefined : "No details recorded"}
                          onClick={() => setDetail(entry)}
                        >
                          <Braces className="size-3.5" /> Details
                        </Button>
                      </TableCell>
                    </TableRow>
                  )
                })}
              </TableBody>
            </Table>
          </div>
          {log.data && (
            <Pagination
              total={log.data.total}
              limit={PAGE_SIZE}
              offset={offset}
              onOffsetChange={goTo}
            />
          )}
        </>
      )}

      <Dialog open={detail !== null} onOpenChange={(open) => !open && setDetail(null)}>
        <DialogContent className="sm:max-w-2xl">
          <DialogHeader>
            <DialogTitle>
              <code className="text-base">{detail?.action}</code>
            </DialogTitle>
            <DialogDescription>
              {detail && `${formatDate(detail.created_at)} · ${detail.actor_name ?? ACTOR_LABELS[detail.actor_type] ?? detail.actor_type}`}
            </DialogDescription>
          </DialogHeader>
          {detail && (
            <>
              <div className="max-h-[60vh] overflow-auto">
                <JsonViewer value={detail.details} />
              </div>
              <div className="flex justify-end">
                <CopyButton value={JSON.stringify(detail.details, null, 2)} label="Copy JSON" />
              </div>
            </>
          )}
        </DialogContent>
      </Dialog>
    </div>
  )
}
