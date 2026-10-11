import { useEffect, useState } from "react"
import { Activity, ShieldCheck, Zap } from "lucide-react"
import { toast } from "sonner"

import { QueryError } from "@/components/shared/QueryError"
import { StepUpCodeField } from "@/components/shared/StepUpCodeField"
import { Button } from "@/components/ui/button"
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select"
import { Skeleton } from "@/components/ui/skeleton"
import { Switch } from "@/components/ui/switch"
import { useSettings, useUpdateSettings } from "@/hooks/queries"
import { toastError } from "@/lib/errors"
import { satsToBtc } from "@/lib/format"
import type { AppSettingsUpdate } from "@/lib/types"
import { isTotpCode } from "@/lib/validation"

const EXPIRY_OPTIONS = [
  { value: 15, label: "15 minutes" },
  { value: 30, label: "30 minutes" },
  { value: 60, label: "1 hour" },
  { value: 120, label: "2 hours" },
  { value: 360, label: "6 hours" },
  { value: 720, label: "12 hours" },
  { value: 1440, label: "24 hours" },
]

export function SettingsPage() {
  return (
    <div className="space-y-6 p-4 lg:p-6">
      <div>
        <h1 className="text-2xl font-semibold tracking-tight">Settings</h1>
        <p className="text-sm text-muted-foreground">
          System configuration — payment expiry, Lightning payout limits,
          two-factor authentication, API traffic retention.
        </p>
      </div>
      <PaymentExpiryCard />
      <PayoutLimitsCard />
      <TotpIssuerCard />
      <TrafficRetentionCard />
    </div>
  )
}

const RETENTION_MAX_DAYS = 3650
const RETENTION_PRESETS = [
  { days: 30, label: "30 days" },
  { days: 90, label: "90 days" },
  { days: 365, label: "1 year" },
  { days: 730, label: "2 years" },
]

function TrafficRetentionCard() {
  const settings = useSettings()
  const updateSettings = useUpdateSettings()
  const [forever, setForever] = useState(false)
  const [days, setDays] = useState("90")

  useEffect(() => {
    if (!settings.data) return
    const saved = settings.data.api_traffic_retention_days
    setForever(saved === null)
    if (saved !== null) setDays(String(saved))
  }, [settings.data])

  const saved = settings.data?.api_traffic_retention_days
  const parsed = Number(days)
  const validDays = Number.isInteger(parsed) && parsed >= 1 && parsed <= RETENTION_MAX_DAYS
  const next = forever ? null : validDays ? parsed : undefined
  const dirty = next !== undefined && next !== saved

  async function save() {
    if (next === undefined) return
    try {
      const updated = await updateSettings.mutateAsync({ api_traffic_retention_days: next })
      const value = updated.api_traffic_retention_days
      toast.success(
        value === null
          ? "API traffic log is now kept forever"
          : `API traffic log is now kept ${value} day${value === 1 ? "" : "s"}`,
      )
    } catch (error) {
      toastError(error, "Update failed")
    }
  }

  return (
    <Card>
      <CardHeader>
        <CardTitle>API traffic retention</CardTitle>
        <CardDescription>
          How long the request log behind the Traffic page is kept. Older
          entries are deleted hourly in the background; 90 days by default.
          Switch on “Keep forever” to never delete anything.
        </CardDescription>
      </CardHeader>
      <CardContent>
        {settings.isError ? (
          <QueryError error={settings.error} />
        ) : settings.isLoading ? (
          <Skeleton className="h-9 w-72" />
        ) : (
          <div className="space-y-4">
            <div className="flex items-center gap-2">
              <Switch
                id="traffic-retention-forever"
                checked={forever}
                onCheckedChange={setForever}
                disabled={updateSettings.isPending}
              />
              <Label htmlFor="traffic-retention-forever">Keep forever</Label>
            </div>
            <div className="flex flex-wrap items-center gap-2">
              <Input
                className="w-28"
                type="number"
                inputMode="numeric"
                min={1}
                max={RETENTION_MAX_DAYS}
                value={days}
                disabled={forever || updateSettings.isPending}
                onChange={(event) => setDays(event.target.value)}
                aria-label="Retention in days"
                aria-invalid={!forever && !validDays}
              />
              <span className="text-sm text-muted-foreground">days</span>
              <div className="flex flex-wrap gap-1">
                {RETENTION_PRESETS.map((preset) => (
                  <Button
                    key={preset.days}
                    variant={!forever && parsed === preset.days ? "secondary" : "ghost"}
                    size="sm"
                    disabled={forever || updateSettings.isPending}
                    onClick={() => setDays(String(preset.days))}
                  >
                    {preset.label}
                  </Button>
                ))}
              </div>
              <Button onClick={save} disabled={!dirty || updateSettings.isPending} size="sm">
                {updateSettings.isPending ? "Saving…" : "Save"}
              </Button>
            </div>
            <div className="flex items-center gap-2 text-sm text-muted-foreground">
              <Activity className="size-3.5 shrink-0" />
              <span>
                {forever
                  ? "Requests are never deleted — disk usage grows with traffic."
                  : validDays
                    ? `Requests older than ${parsed} day${parsed === 1 ? "" : "s"} are deleted.`
                    : `Enter a whole number between 1 and ${RETENTION_MAX_DAYS}.`}
              </span>
            </div>
          </div>
        )}
      </CardContent>
    </Card>
  )
}

/** Total bitcoin supply in sats — the API's upper bound for both limits. */
const MAX_LIMIT_SATS = 2_100_000_000_000_000

function parseLimit(value: string): number | null {
  const trimmed = value.trim()
  if (!/^\d+$/.test(trimmed)) return null
  const parsed = Number(trimmed)
  return Number.isSafeInteger(parsed) && parsed <= MAX_LIMIT_SATS ? parsed : null
}

function limitHint(value: string): string {
  const parsed = parseLimit(value)
  if (parsed === null) return `Whole number of sats, 0 to ${MAX_LIMIT_SATS.toLocaleString()}`
  if (parsed === 0) return "0 — payouts disabled"
  return `≈ ${satsToBtc(String(parsed))} BTC`
}

function PayoutLimitsCard() {
  const settings = useSettings()
  const updateSettings = useUpdateSettings()
  const [perPayout, setPerPayout] = useState("")
  const [daily, setDaily] = useState("")
  const [code, setCode] = useState("")

  useEffect(() => {
    if (!settings.data) return
    setPerPayout(String(settings.data.payout_max_sats))
    setDaily(String(settings.data.payout_daily_max_sats))
  }, [settings.data])

  const nextPerPayout = parseLimit(perPayout)
  const nextDaily = parseLimit(daily)
  const changes: AppSettingsUpdate = {}
  if (nextPerPayout !== null && nextPerPayout !== settings.data?.payout_max_sats) {
    changes.payout_max_sats = nextPerPayout
  }
  if (nextDaily !== null && nextDaily !== settings.data?.payout_daily_max_sats) {
    changes.payout_daily_max_sats = nextDaily
  }
  const valid = nextPerPayout !== null && nextDaily !== null
  const dirty = Object.keys(changes).length > 0
  const disabled = settings.data
    ? settings.data.payout_max_sats === 0 || settings.data.payout_daily_max_sats === 0
    : false

  async function save(event: React.FormEvent) {
    event.preventDefault()
    if (!valid || !dirty || !isTotpCode(code)) return
    try {
      const updated = await updateSettings.mutateAsync({ ...changes, totp_code: code.trim() })
      toast.success(
        updated.payout_max_sats === 0 || updated.payout_daily_max_sats === 0
          ? "Lightning payouts are now disabled"
          : "Payout limits updated",
      )
    } catch (error) {
      toastError(error, "Update failed")
    } finally {
      // Single-use codes: every save asks for a fresh one.
      setCode("")
    }
  }

  return (
    <Card>
      <CardHeader>
        <CardTitle>Lightning payouts</CardTitle>
        <CardDescription>
          Caps on what leaves the Lightning wallet — dashboard payouts and API
          keys allowed to send alike. Amounts in sats, fees excluded. Set either
          limit to 0 to disable payouts.
        </CardDescription>
      </CardHeader>
      <CardContent>
        {settings.isError ? (
          <QueryError error={settings.error} />
        ) : settings.isLoading ? (
          <Skeleton className="h-40 w-full max-w-md" />
        ) : (
          <form onSubmit={save} className="max-w-md space-y-4">
            <div className="grid gap-4 sm:grid-cols-2">
              <div className="space-y-2">
                <Label htmlFor="payout-max-sats">Per payout (sats)</Label>
                <Input
                  id="payout-max-sats"
                  inputMode="numeric"
                  value={perPayout}
                  disabled={updateSettings.isPending}
                  onChange={(event) => setPerPayout(event.target.value)}
                  aria-invalid={nextPerPayout === null}
                />
                <p
                  className={
                    nextPerPayout === null
                      ? "text-xs text-destructive"
                      : "text-xs text-muted-foreground"
                  }
                >
                  {limitHint(perPayout)}
                </p>
              </div>
              <div className="space-y-2">
                <Label htmlFor="payout-daily-max-sats">Per 24 hours (sats)</Label>
                <Input
                  id="payout-daily-max-sats"
                  inputMode="numeric"
                  value={daily}
                  disabled={updateSettings.isPending}
                  onChange={(event) => setDaily(event.target.value)}
                  aria-invalid={nextDaily === null}
                />
                <p
                  className={
                    nextDaily === null
                      ? "text-xs text-destructive"
                      : "text-xs text-muted-foreground"
                  }
                >
                  {limitHint(daily)}
                </p>
              </div>
            </div>
            {dirty && (
              <StepUpCodeField
                id="payout-limits-code"
                value={code}
                onChange={setCode}
                disabled={updateSettings.isPending}
              />
            )}
            <div className="flex items-center gap-3">
              <Button
                type="submit"
                size="sm"
                disabled={!valid || !dirty || !isTotpCode(code) || updateSettings.isPending}
              >
                {updateSettings.isPending ? "Saving…" : "Save"}
              </Button>
              <span className="flex items-center gap-2 text-sm text-muted-foreground">
                <Zap className="size-3.5 shrink-0" />
                {disabled ? "Payouts are currently disabled." : "Rolling 24 h window."}
              </span>
            </div>
          </form>
        )}
      </CardContent>
    </Card>
  )
}

const DEFAULT_TOTP_ISSUER = "Crypto Payment Gateway"

function TotpIssuerCard() {
  const settings = useSettings()
  const updateSettings = useUpdateSettings()
  const [issuer, setIssuer] = useState("")

  useEffect(() => {
    if (settings.data) setIssuer(settings.data.totp_issuer)
  }, [settings.data])

  const saved = settings.data?.totp_issuer ?? ""
  const dirty = issuer.trim() !== saved

  async function save() {
    // An empty (or default-equal) value resets the stored setting so the
    // backend falls back to its default.
    const next = issuer.trim() === DEFAULT_TOTP_ISSUER ? "" : issuer.trim()
    try {
      const updated = await updateSettings.mutateAsync({ totp_issuer: next })
      toast.success(`Authenticator apps will now show “${updated.totp_issuer}”`)
    } catch (error) {
      toast.error(error instanceof Error ? error.message : "Update failed")
    }
  }

  return (
    <Card>
      <CardHeader>
        <CardTitle>Two-factor issuer</CardTitle>
        <CardDescription>
          Name shown by authenticator apps next to staff 2FA codes. Applies to
          new enrollments only — existing entries keep their label. Leave empty
          to use the default “{DEFAULT_TOTP_ISSUER}”.
        </CardDescription>
      </CardHeader>
      <CardContent>
        {settings.isError ? (
          <QueryError error={settings.error} />
        ) : settings.isLoading ? (
          <Skeleton className="h-9 w-64" />
        ) : (
          <div className="space-y-3">
            <div className="flex items-center gap-2">
              <Input
                className="w-72"
                maxLength={40}
                placeholder={DEFAULT_TOTP_ISSUER}
                value={issuer}
                onChange={(event) => setIssuer(event.target.value)}
                aria-label="Two-factor issuer"
              />
              <Button onClick={save} disabled={!dirty || updateSettings.isPending} size="sm">
                {updateSettings.isPending ? "Saving…" : "Save"}
              </Button>
            </div>
            <div className="flex items-center gap-2 text-sm text-muted-foreground">
              <ShieldCheck className="size-3.5 shrink-0" />
              <span>
                Shown as <code className="text-xs">{issuer.trim() || DEFAULT_TOTP_ISSUER}: user@company.com</code>
              </span>
            </div>
          </div>
        )}
      </CardContent>
    </Card>
  )
}

function PaymentExpiryCard() {
  const settings = useSettings()
  const updateSettings = useUpdateSettings()

  async function onChange(value: string) {
    const minutes = Number(value)
    try {
      await updateSettings.mutateAsync({ deposit_expiry_minutes: minutes })
      const label = EXPIRY_OPTIONS.find((o) => o.value === minutes)?.label ?? `${minutes} min`
      toast.success(`Default payment expiry set to ${label}`)
    } catch (error) {
      toast.error(error instanceof Error ? error.message : "Update failed")
    }
  }

  const current = settings.data?.deposit_expiry_minutes
  const isPreset = EXPIRY_OPTIONS.some((o) => o.value === current)

  return (
    <Card>
      <CardHeader>
        <CardTitle>Payment expiry</CardTitle>
        <CardDescription>
          How long a newly created deposit address stays valid when no explicit
          expiry is provided. Maximum 24 hours.
        </CardDescription>
      </CardHeader>
      <CardContent>
        {settings.isError ? (
          <QueryError error={settings.error} />
        ) : settings.isLoading ? (
          <Skeleton className="h-9 w-48" />
        ) : (
          <Select
            value={current !== undefined ? String(current) : undefined}
            onValueChange={onChange}
            disabled={updateSettings.isPending}
          >
            <SelectTrigger className="w-48">
              <SelectValue placeholder="Select duration" />
            </SelectTrigger>
            <SelectContent>
              {EXPIRY_OPTIONS.map((option) => (
                <SelectItem key={option.value} value={String(option.value)}>
                  {option.label}
                </SelectItem>
              ))}
              {!isPreset && current !== undefined && (
                <SelectItem value={String(current)}>{current} minutes</SelectItem>
              )}
            </SelectContent>
          </Select>
        )}
      </CardContent>
    </Card>
  )
}

