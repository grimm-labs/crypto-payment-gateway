import { AlertTriangleIcon, GaugeIcon, KeyRoundIcon, XIcon, ZapIcon } from "lucide-react"
import { useSearchParams } from "react-router-dom"

import { TrafficChart } from "@/components/traffic-chart"
import { Pagination } from "@/components/shared/Pagination"
import { QueryError } from "@/components/shared/QueryError"
import { StatusBadge } from "@/components/shared/StatusBadge"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import {
  Card,
  CardAction,
  CardContent,
  CardDescription,
  CardFooter,
  CardHeader,
  CardTitle,
} from "@/components/ui/card"
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select"
import { Skeleton } from "@/components/ui/skeleton"
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table"
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group"
import { useSettings, useTrafficOverview, useTrafficRequests } from "@/hooks/queries"
import { useAuth } from "@/lib/auth"
import { formatDate } from "@/lib/format"
import type {
  StatusClass,
  TrafficApiKeyStat,
  TrafficRequest,
  TrafficSurface,
  TrafficWindow,
} from "@/lib/types"

const PAGE_SIZE = 20
const ALL = "all"
const SURFACES: TrafficSurface[] = ["merchant", "admin", "all"]
const WINDOWS: TrafficWindow[] = ["15m", "1h", "24h"]

const SURFACE_LABELS: Record<TrafficSurface, string> = {
  merchant: "Merchant API",
  admin: "Admin API (dashboard)",
  all: "All surfaces",
}
const WINDOW_LABELS: Record<TrafficWindow, string> = {
  "15m": "Last 15 min",
  "1h": "Last hour",
  "24h": "Last 24 h",
}

function statusClass(status: number): StatusClass | undefined {
  if (status >= 200 && status < 300) return "2xx"
  if (status >= 300 && status < 400) return "3xx"
  if (status >= 400 && status < 500) return "4xx"
  if (status >= 500 && status < 600) return "5xx"
  return undefined
}

function formatMs(value: number | null | undefined): string {
  if (value === null || value === undefined) return "—"
  return `${value < 10 ? value.toFixed(1) : Math.round(value)} ms`
}

function formatPercent(fraction: number): string {
  return `${(fraction * 100).toFixed(fraction < 0.1 ? 2 : 1)}%`
}

function keyLabel(key: TrafficApiKeyStat | TrafficRequest["api_key"]): string {
  if (!key) return "No valid key"
  return key.name ?? key.prefix ?? "Unknown key"
}

function retentionLabel(days: number | null | undefined): string | null {
  if (days === undefined) return null
  if (days === null) return "kept forever"
  if (days % 365 === 0) return `kept ${days / 365} year${days === 365 ? "" : "s"}`
  return `kept ${days} days`
}

export function TrafficPage() {
  const { staff } = useAuth()
  const [searchParams, setSearchParams] = useSearchParams()

  const surfaceParam = searchParams.get("surface")
  const surface: TrafficSurface = SURFACES.includes(surfaceParam as TrafficSurface)
    ? (surfaceParam as TrafficSurface)
    : "merchant"
  const windowParam = searchParams.get("window")
  const window: TrafficWindow = WINDOWS.includes(windowParam as TrafficWindow)
    ? (windowParam as TrafficWindow)
    : "15m"
  const status = searchParams.get("status") ?? ALL
  const route = searchParams.get("route") ?? undefined
  const apiKeyId = searchParams.get("key") ?? undefined
  const offset = Number(searchParams.get("offset") ?? 0)

  const overview = useTrafficOverview({ surface, window })
  const requests = useTrafficRequests({
    surface,
    status: status === ALL ? undefined : status,
    route,
    api_key_id: apiKeyId,
    limit: PAGE_SIZE,
    offset,
  })
  // Retention lives in the admin-only settings endpoint.
  const settings = useSettings(staff?.role === "admin")
  const retention = retentionLabel(settings.data?.api_traffic_retention_days)

  function updateParams(updates: Record<string, string | undefined>) {
    const next = new URLSearchParams(searchParams)
    for (const [key, value] of Object.entries(updates)) {
      if (value === undefined || value === ALL) next.delete(key)
      else next.set(key, value)
    }
    setSearchParams(next)
  }

  const summary = overview.data?.summary
  const serverErrors = summary?.server_errors ?? 0
  const dropped = summary?.dropped_events ?? 0

  return (
    <div className="space-y-6 p-4 lg:p-6">
      <div className="flex flex-wrap items-end justify-between gap-4">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">Traffic</h1>
          <p className="text-sm text-muted-foreground">
            Live view of HTTP requests hitting the API — refreshed every{" "}
            {window === "24h" ? "30" : "5"} seconds.
            {retention ? ` Request log ${retention}.` : ""}
          </p>
        </div>
        <div className="flex flex-wrap items-center gap-2">
          <Select
            value={surface}
            onValueChange={(value) =>
              updateParams({ surface: value, key: undefined, route: undefined, offset: undefined })
            }
          >
            <SelectTrigger className="w-52" aria-label="Surface">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {SURFACES.map((value) => (
                <SelectItem key={value} value={value}>
                  {SURFACE_LABELS[value]}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          <ToggleGroup
            type="single"
            variant="outline"
            value={window}
            onValueChange={(value) => value && updateParams({ window: value })}
          >
            {WINDOWS.map((value) => (
              <ToggleGroupItem key={value} value={value} className="px-3">
                {value}
              </ToggleGroupItem>
            ))}
          </ToggleGroup>
        </div>
      </div>

      {overview.isError && <QueryError error={overview.error} />}

      <div className="grid grid-cols-1 gap-4 *:data-[slot=card]:bg-linear-to-t *:data-[slot=card]:from-primary/5 *:data-[slot=card]:to-card *:data-[slot=card]:shadow-xs @xl/main:grid-cols-2 @5xl/main:grid-cols-4 dark:*:data-[slot=card]:bg-card">
        <Card className="@container/card">
          <CardHeader>
            <CardDescription>Requests / min</CardDescription>
            <CardTitle className="text-2xl font-semibold tabular-nums @[250px]/card:text-3xl">
              {summary ? summary.requests_last_minute : <Skeleton className="h-8 w-16" />}
            </CardTitle>
            <CardAction>
              <Badge variant="outline">
                <ZapIcon />
                {summary?.requests_total ?? 0} total
              </Badge>
            </CardAction>
          </CardHeader>
          <CardFooter className="flex-col items-start gap-1.5 text-sm">
            <div className="font-medium">Last 60 seconds</div>
            <div className="text-muted-foreground">
              avg {summary?.requests_per_minute_avg ?? 0} / min over {WINDOW_LABELS[window].toLowerCase()}
            </div>
          </CardFooter>
        </Card>
        <Card className="@container/card">
          <CardHeader>
            <CardDescription>Error rate</CardDescription>
            <CardTitle className="text-2xl font-semibold tabular-nums @[250px]/card:text-3xl">
              {summary ? formatPercent(summary.error_rate) : <Skeleton className="h-8 w-16" />}
            </CardTitle>
            <CardAction>
              <Badge variant={serverErrors > 0 ? "destructive" : "outline"}>
                <AlertTriangleIcon />
                {serverErrors} 5xx
              </Badge>
            </CardAction>
          </CardHeader>
          <CardFooter className="flex-col items-start gap-1.5 text-sm">
            <div className="font-medium">{summary?.client_errors ?? 0} client errors (4xx)</div>
            <div className="text-muted-foreground">
              {serverErrors > 0 ? "Server errors need attention" : "No server errors"}
            </div>
          </CardFooter>
        </Card>
        <Card className="@container/card">
          <CardHeader>
            <CardDescription>p95 latency</CardDescription>
            <CardTitle className="text-2xl font-semibold tabular-nums @[250px]/card:text-3xl">
              {summary ? formatMs(summary.p95_ms) : <Skeleton className="h-8 w-20" />}
            </CardTitle>
            <CardAction>
              <Badge variant="outline">
                <GaugeIcon />
                p50 {formatMs(summary?.p50_ms)}
              </Badge>
            </CardAction>
          </CardHeader>
          <CardFooter className="flex-col items-start gap-1.5 text-sm">
            <div className="font-medium">Server-side processing time</div>
            <div className="text-muted-foreground">Measured inside the API, excluding network</div>
          </CardFooter>
        </Card>
        <Card className="@container/card">
          <CardHeader>
            <CardDescription>Active API keys</CardDescription>
            <CardTitle className="text-2xl font-semibold tabular-nums @[250px]/card:text-3xl">
              {summary ? (
                surface === "admin" ? "—" : summary.active_keys
              ) : (
                <Skeleton className="h-8 w-12" />
              )}
            </CardTitle>
            <CardAction>
              <Badge variant="outline">
                <KeyRoundIcon />
                {WINDOW_LABELS[window]}
              </Badge>
            </CardAction>
          </CardHeader>
          <CardFooter className="flex-col items-start gap-1.5 text-sm">
            <div className="font-medium">Distinct keys with authenticated calls</div>
            <div className="text-muted-foreground">
              {dropped > 0
                ? `${dropped} event${dropped === 1 ? "" : "s"} dropped under load since boot`
                : "No events dropped"}
            </div>
          </CardFooter>
        </Card>
      </div>

      <Card>
        <CardHeader>
          <CardTitle>Requests over time</CardTitle>
          <CardDescription>
            {SURFACE_LABELS[surface]} · {WINDOW_LABELS[window].toLowerCase()} ·{" "}
            {overview.data?.bucket_seconds ?? 0}s buckets, stacked by status class
          </CardDescription>
        </CardHeader>
        <CardContent className="px-2 sm:px-6">
          <TrafficChart
            data={overview.data?.timeseries}
            bucketSeconds={overview.data?.bucket_seconds ?? 60}
            loading={overview.isLoading}
          />
        </CardContent>
      </Card>

      <div className="grid grid-cols-1 gap-4 @4xl/main:grid-cols-2">
        <Card>
          <CardHeader>
            <CardTitle>Top routes</CardTitle>
            <CardDescription>Click a route to filter the request log.</CardDescription>
          </CardHeader>
          <CardContent>
            {overview.isLoading && !overview.data ? (
              <Skeleton className="h-40 w-full" />
            ) : (
              <Table>
                <TableHeader>
                  <TableRow>
                    <TableHead>Route</TableHead>
                    <TableHead className="text-right">Requests</TableHead>
                    <TableHead className="text-right">4xx / 5xx</TableHead>
                    <TableHead className="text-right">p95</TableHead>
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {overview.data?.top_routes.length === 0 && (
                    <TableRow>
                      <TableCell colSpan={4} className="h-20 text-center text-muted-foreground">
                        No requests in this window.
                      </TableCell>
                    </TableRow>
                  )}
                  {overview.data?.top_routes.map((r) => (
                    <TableRow
                      key={`${r.method} ${r.route}`}
                      className="cursor-pointer"
                      data-state={route === r.route ? "selected" : undefined}
                      onClick={() =>
                        updateParams({ route: route === r.route ? undefined : r.route, offset: undefined })
                      }
                    >
                      <TableCell>
                        <Badge variant="secondary" className="mr-2 font-mono text-[10px]">
                          {r.method}
                        </Badge>
                        <code className="text-xs">{r.route}</code>
                      </TableCell>
                      <TableCell className="text-right tabular-nums">{r.count}</TableCell>
                      <TableCell className="text-right tabular-nums">
                        <span className={r.client_errors > 0 ? "text-amber-600 dark:text-amber-400" : ""}>
                          {r.client_errors}
                        </span>
                        {" / "}
                        <span className={r.server_errors > 0 ? "text-destructive" : ""}>
                          {r.server_errors}
                        </span>
                      </TableCell>
                      <TableCell className="text-right tabular-nums">{formatMs(r.p95_ms)}</TableCell>
                    </TableRow>
                  ))}
                </TableBody>
              </Table>
            )}
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle>Top API keys</CardTitle>
            <CardDescription>
              Merchant surface only. Click a key to filter the request log.
            </CardDescription>
          </CardHeader>
          <CardContent>
            {overview.isLoading && !overview.data ? (
              <Skeleton className="h-40 w-full" />
            ) : (
              <Table>
                <TableHeader>
                  <TableRow>
                    <TableHead>Key</TableHead>
                    <TableHead className="text-right">Requests</TableHead>
                    <TableHead className="text-right">4xx / 5xx</TableHead>
                    <TableHead className="text-right">p95</TableHead>
                    <TableHead>Last seen</TableHead>
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {overview.data?.top_api_keys.length === 0 && (
                    <TableRow>
                      <TableCell colSpan={5} className="h-20 text-center text-muted-foreground">
                        {surface === "admin"
                          ? "API keys are not used on the admin surface."
                          : "No API-key traffic in this window."}
                      </TableCell>
                    </TableRow>
                  )}
                  {overview.data?.top_api_keys.map((k) => {
                    const id = k.api_key_id ?? "none"
                    const selected = apiKeyId === id
                    return (
                      <TableRow
                        key={id}
                        className={k.api_key_id ? "cursor-pointer" : ""}
                        data-state={selected ? "selected" : undefined}
                        onClick={() =>
                          k.api_key_id &&
                          updateParams({ key: selected ? undefined : k.api_key_id, offset: undefined })
                        }
                      >
                        <TableCell>
                          <div className="font-medium">{keyLabel(k)}</div>
                          <div className="text-xs text-muted-foreground">
                            {k.prefix ? <code>{k.prefix}…</code> : "Missing, invalid or expired"}
                            {k.status && k.status !== "active" && (
                              <StatusBadge status={k.status} />
                            )}
                          </div>
                        </TableCell>
                        <TableCell className="text-right tabular-nums">{k.count}</TableCell>
                        <TableCell className="text-right tabular-nums">
                          <span className={k.client_errors > 0 ? "text-amber-600 dark:text-amber-400" : ""}>
                            {k.client_errors}
                          </span>
                          {" / "}
                          <span className={k.server_errors > 0 ? "text-destructive" : ""}>
                            {k.server_errors}
                          </span>
                        </TableCell>
                        <TableCell className="text-right tabular-nums">{formatMs(k.p95_ms)}</TableCell>
                        <TableCell className="text-xs text-muted-foreground">
                          {formatDate(k.last_seen_at)}
                        </TableCell>
                      </TableRow>
                    )
                  })}
                </TableBody>
              </Table>
            )}
          </CardContent>
        </Card>
      </div>

      <Card>
        <CardHeader>
          <CardTitle>Recent requests</CardTitle>
          <CardDescription>
            Newest first. Bodies and raw API keys are never stored.
          </CardDescription>
          <CardAction>
            <Select
              value={status}
              onValueChange={(value) => updateParams({ status: value, offset: undefined })}
            >
              <SelectTrigger className="w-36" aria-label="Status class">
                <SelectValue placeholder="Status" />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value={ALL}>All statuses</SelectItem>
                <SelectItem value="2xx">2xx</SelectItem>
                <SelectItem value="3xx">3xx</SelectItem>
                <SelectItem value="4xx">4xx</SelectItem>
                <SelectItem value="5xx">5xx</SelectItem>
              </SelectContent>
            </Select>
          </CardAction>
        </CardHeader>
        <CardContent className="space-y-4">
          {(route || apiKeyId) && (
            <div className="flex flex-wrap items-center gap-2 text-sm">
              <span className="text-muted-foreground">Filters:</span>
              {route && (
                <Button
                  variant="secondary"
                  size="sm"
                  className="h-7 gap-1 font-mono text-xs"
                  onClick={() => updateParams({ route: undefined, offset: undefined })}
                >
                  {route} <XIcon className="size-3" />
                </Button>
              )}
              {apiKeyId && (
                <Button
                  variant="secondary"
                  size="sm"
                  className="h-7 gap-1 text-xs"
                  onClick={() => updateParams({ key: undefined, offset: undefined })}
                >
                  key {keyLabel(overview.data?.top_api_keys.find((k) => k.api_key_id === apiKeyId) ?? null)}{" "}
                  <XIcon className="size-3" />
                </Button>
              )}
            </div>
          )}

          {requests.isError ? (
            <QueryError error={requests.error} />
          ) : requests.isLoading && !requests.data ? (
            <Skeleton className="h-64 w-full" />
          ) : (
            <>
              <div className="rounded-lg border">
                <Table>
                  <TableHeader>
                    <TableRow>
                      <TableHead>Time</TableHead>
                      <TableHead>Method</TableHead>
                      <TableHead>Route</TableHead>
                      <TableHead>Status</TableHead>
                      <TableHead className="text-right">Duration</TableHead>
                      <TableHead>Identity</TableHead>
                      <TableHead>IP</TableHead>
                    </TableRow>
                  </TableHeader>
                  <TableBody>
                    {requests.data?.data.length === 0 && (
                      <TableRow>
                        <TableCell colSpan={7} className="h-24 text-center text-muted-foreground">
                          No requests match these filters.
                        </TableCell>
                      </TableRow>
                    )}
                    {requests.data?.data.map((r) => {
                      const cls = statusClass(r.status)
                      return (
                        <TableRow key={r.id}>
                          <TableCell className="text-xs text-muted-foreground whitespace-nowrap">
                            {formatDate(r.created_at)}
                          </TableCell>
                          <TableCell>
                            <Badge variant="secondary" className="font-mono text-[10px]">
                              {r.method}
                            </Badge>
                          </TableCell>
                          <TableCell>
                            <code className="text-xs">{r.route}</code>
                            {surface === "all" && (
                              <span className="ml-2 text-[10px] uppercase text-muted-foreground">
                                {r.surface}
                              </span>
                            )}
                          </TableCell>
                          <TableCell>
                            {cls ? (
                              <StatusBadge status={cls} />
                            ) : (
                              <span className="text-xs">{r.status}</span>
                            )}
                            <span className="ml-1.5 text-xs text-muted-foreground tabular-nums">
                              {r.status}
                            </span>
                          </TableCell>
                          <TableCell className="text-right tabular-nums text-xs">
                            {formatMs(r.duration_ms)}
                          </TableCell>
                          <TableCell className="text-xs">
                            {r.api_key ? (
                              <>
                                <span>{keyLabel(r.api_key)}</span>
                                {r.api_key.prefix && (
                                  <code className="ml-1 text-muted-foreground">{r.api_key.prefix}…</code>
                                )}
                              </>
                            ) : r.staff ? (
                              <span>{r.staff.name ?? "Staff"}</span>
                            ) : (
                              <span className="text-muted-foreground">—</span>
                            )}
                          </TableCell>
                          <TableCell className="font-mono text-xs text-muted-foreground">
                            {r.ip ?? "—"}
                          </TableCell>
                        </TableRow>
                      )
                    })}
                  </TableBody>
                </Table>
              </div>
              {requests.data && (
                <Pagination
                  total={requests.data.total}
                  limit={PAGE_SIZE}
                  offset={offset}
                  onOffsetChange={(next) => updateParams({ offset: next ? String(next) : undefined })}
                />
              )}
            </>
          )}
        </CardContent>
      </Card>
    </div>
  )
}
