import { useEffect, useState } from "react"
import { Eye } from "lucide-react"
import { toast } from "sonner"

import { SettingsTabs } from "@/components/settings/SettingsTabs"
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
import { Skeleton } from "@/components/ui/skeleton"
import { Switch } from "@/components/ui/switch"
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table"
import { useAssetSettings, useUpdateAssetSettings } from "@/hooks/withdrawals"
import { useIsAdmin } from "@/lib/auth"
import { toastError } from "@/lib/errors"
import { decimalToRaw, formatDate, isValidDecimalAmount, networkLabel, tokenDecimals } from "@/lib/format"
import type {
  AmountSetting,
  AssetSettings,
  AssetSettingsUpdate,
  AssetSettingsView,
  SweepSetting,
  Token,
} from "@/lib/types"
import { isTotpCode } from "@/lib/validation"

type RequiredField = "max_per_withdrawal" | "daily_max" | "approval_threshold"
type OptionalField = "hot_wallet_cap" | "gas_reserve_floor" | "gas_alert_below"
type Field = RequiredField | OptionalField

const REQUIRED_FIELDS: Array<{ key: RequiredField; label: string; hint: string }> = [
  { key: "max_per_withdrawal", label: "Max per withdrawal", hint: "0 disables withdrawals of this asset." },
  {
    key: "daily_max",
    label: "Rolling 24 h",
    hint: "Sum over the last 24 h, every network of the asset together.",
  },
  {
    key: "approval_threshold",
    label: "Approval threshold",
    hint: "At or above: another admin must approve, to an address-book entry.",
  },
]

const OPTIONAL_FIELDS: Array<{ key: OptionalField; label: string; hint: string; gasOnly: boolean }> = [
  { key: "hot_wallet_cap", label: "Hot-wallet cap", hint: "Alert above it. Empty = no cap.", gasOnly: false },
  {
    key: "gas_reserve_floor",
    label: "Gas reserve floor",
    hint: "Below it, gas top-ups and withdrawals of this asset stop. Empty = none.",
    gasOnly: true,
  },
  {
    key: "gas_alert_below",
    label: "Gas alert below",
    hint: "Raises a gas-reserve-low alert. Empty = none.",
    gasOnly: true,
  },
]

type AssetDraft = Record<Field, string>
type SweepDraft = { minimum: string; enabled: boolean }

function sweepKey(sweep: { network: string; token: string }) {
  return `${sweep.network}:${sweep.token}`
}

function draftsFrom(view: AssetSettingsView) {
  const assets: Partial<Record<Token, AssetDraft>> = {}
  for (const asset of view.assets) {
    assets[asset.token] = {
      max_per_withdrawal: asset.max_per_withdrawal.amount,
      daily_max: asset.daily_max.amount,
      approval_threshold: asset.approval_threshold.amount,
      hot_wallet_cap: asset.hot_wallet_cap?.amount ?? "",
      gas_reserve_floor: asset.gas_reserve_floor?.amount ?? "",
      gas_alert_below: asset.gas_alert_below?.amount ?? "",
    }
  }
  const sweeps: Record<string, SweepDraft> = {}
  for (const sweep of view.sweeps) {
    sweeps[sweepKey(sweep)] = { minimum: sweep.minimum, enabled: sweep.enabled }
  }
  return { assets, sweeps }
}

/** Same value in base units (so "1.50" equals "1.5"). */
function sameAmount(draft: string, saved: AmountSetting | null, token: Token): boolean {
  const raw = decimalToRaw(draft, token)
  return raw !== null && saved !== null && raw.toString() === saved.raw
}

function fieldValid(asset: AssetSettings, field: Field, value: string): boolean {
  const optional = field === "hot_wallet_cap" || field === "gas_reserve_floor" || field === "gas_alert_below"
  if (optional && value.trim() === "") return true
  return isValidDecimalAmount(value, asset.token, { allowZero: true })
}

/** Only what changed — the PATCH is partial. */
function buildUpdate(
  view: AssetSettingsView,
  assets: Partial<Record<Token, AssetDraft>>,
  sweeps: Record<string, SweepDraft>,
): Omit<AssetSettingsUpdate, "totp_code"> {
  const update: Omit<AssetSettingsUpdate, "totp_code"> = { assets: {}, sweeps: [] }
  for (const asset of view.assets) {
    const draft = assets[asset.token]
    if (!draft) continue
    const patch: NonNullable<AssetSettingsUpdate["assets"][Token]> = {}
    for (const { key } of REQUIRED_FIELDS) {
      if (!sameAmount(draft[key], asset[key], asset.token)) patch[key] = draft[key].trim()
    }
    for (const { key, gasOnly } of OPTIONAL_FIELDS) {
      if (gasOnly && !asset.is_gas_asset) continue
      const value = draft[key].trim()
      if (value === "") {
        if (asset[key] !== null) patch[key] = null
      } else if (!sameAmount(value, asset[key], asset.token)) {
        patch[key] = value
      }
    }
    if (Object.keys(patch).length > 0) update.assets[asset.token] = patch
  }
  for (const sweep of view.sweeps) {
    const draft = sweeps[sweepKey(sweep)]
    if (!draft) continue
    const change: AssetSettingsUpdate["sweeps"][number] = { network: sweep.network, token: sweep.token }
    const minimumRaw = decimalToRaw(draft.minimum, sweep.token)
    if (minimumRaw === null || minimumRaw.toString() !== sweep.minimum_raw) {
      change.minimum = draft.minimum.trim()
    }
    if (draft.enabled !== sweep.enabled) change.enabled = draft.enabled
    if (change.minimum !== undefined || change.enabled !== undefined) update.sweeps.push(change)
  }
  return update
}

export function AssetSettingsPage() {
  const settings = useAssetSettings()
  const save = useUpdateAssetSettings()
  // Operators see the settings read-only.
  const isAdmin = useIsAdmin()
  const [assets, setAssets] = useState<Partial<Record<Token, AssetDraft>>>({})
  const [sweeps, setSweeps] = useState<Record<string, SweepDraft>>({})
  const [code, setCode] = useState("")

  useEffect(() => {
    if (!settings.data) return
    const drafts = draftsFrom(settings.data)
    setAssets(drafts.assets)
    setSweeps(drafts.sweeps)
  }, [settings.data])

  const view = settings.data
  const valid =
    !!view &&
    view.assets.every((asset) => {
      const draft = assets[asset.token]
      return (
        !draft ||
        [...REQUIRED_FIELDS, ...OPTIONAL_FIELDS].every(({ key }) => fieldValid(asset, key, draft[key]))
      )
    }) &&
    view.sweeps.every((sweep) => {
      const draft = sweeps[sweepKey(sweep)]
      return !draft || isValidDecimalAmount(draft.minimum, sweep.token, { allowZero: true })
    })
  const update = view && valid ? buildUpdate(view, assets, sweeps) : null
  const dirty = !!update && (Object.keys(update.assets).length > 0 || update.sweeps.length > 0)

  async function submit(event: React.FormEvent) {
    event.preventDefault()
    if (!update || !dirty || !isTotpCode(code)) return
    try {
      await save.mutateAsync({ ...update, totp_code: code.trim() })
      toast.success("Asset settings saved")
    } catch (error) {
      toastError(error, "Update failed")
    } finally {
      // Single-use codes: every save asks for a fresh one.
      setCode("")
    }
  }

  function setField(token: Token, field: Field, value: string) {
    setAssets((current) => {
      const draft = current[token]
      return draft ? { ...current, [token]: { ...draft, [field]: value } } : current
    })
  }

  return (
    <div className="space-y-6 p-4 lg:p-6">
      <div className="space-y-3">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">Settings</h1>
          <p className="text-sm text-muted-foreground">
            Withdrawal limits, approval thresholds, hot-wallet caps, gas reserves and
            sweep minimums — per asset, every network of the asset together. Amounts
            in token units; fees are never counted.
          </p>
        </div>
        <SettingsTabs />
      </div>

      {!isAdmin && (
        <div className="flex items-center gap-2 rounded-lg border bg-muted/40 p-3 text-sm text-muted-foreground">
          <Eye className="size-4 shrink-0" />
          Read-only — only admins can change these settings.
        </div>
      )}

      {settings.isError ? (
        <QueryError error={settings.error} />
      ) : settings.isLoading || !view ? (
        <Skeleton className="h-96 w-full" />
      ) : (
        <form onSubmit={submit} className="space-y-6">
          <div className="grid gap-6 xl:grid-cols-2">
            {view.assets.map((asset) => (
              <AssetCard
                key={asset.token}
                asset={asset}
                draft={assets[asset.token]}
                readOnly={!isAdmin || save.isPending}
                onChange={(field, value) => setField(asset.token, field, value)}
              />
            ))}
          </div>

          <SweepSettingsCard
            sweeps={view.sweeps}
            drafts={sweeps}
            readOnly={!isAdmin || save.isPending}
            onChange={(key, draft) => setSweeps((current) => ({ ...current, [key]: draft }))}
          />

          {isAdmin && (
            <Card>
              <CardContent className="space-y-4">
                {dirty && (
                  <StepUpCodeField
                    id="asset-settings-code"
                    value={code}
                    onChange={setCode}
                    disabled={save.isPending}
                  />
                )}
                <div className="flex items-center gap-3">
                  <Button
                    type="submit"
                    disabled={!dirty || !isTotpCode(code) || save.isPending}
                  >
                    {save.isPending ? "Saving…" : "Save changes"}
                  </Button>
                  {!valid && (
                    <span className="text-sm text-destructive">Fix the highlighted amounts.</span>
                  )}
                  {valid && dirty && (
                    <Button
                      type="button"
                      variant="ghost"
                      disabled={save.isPending}
                      onClick={() => {
                        const drafts = draftsFrom(view)
                        setAssets(drafts.assets)
                        setSweeps(drafts.sweeps)
                        setCode("")
                      }}
                    >
                      Discard
                    </Button>
                  )}
                </div>
              </CardContent>
            </Card>
          )}
        </form>
      )}
    </div>
  )
}

function AssetCard({
  asset,
  draft,
  readOnly,
  onChange,
}: {
  asset: AssetSettings
  draft: AssetDraft | undefined
  readOnly: boolean
  onChange: (field: Field, value: string) => void
}) {
  const fields = [
    ...REQUIRED_FIELDS,
    ...OPTIONAL_FIELDS.filter((field) => !field.gasOnly || asset.is_gas_asset),
  ]
  const disabled = asset.max_per_withdrawal.raw === "0" || asset.daily_max.raw === "0"

  return (
    <Card>
      <CardHeader>
        <CardTitle>{asset.token}</CardTitle>
        <CardDescription>
          {asset.is_gas_asset ? "Gas asset · " : ""}
          {tokenDecimals(asset.token)} decimals · updated {formatDate(asset.updated_at)}
          {disabled ? " · withdrawals disabled" : ""}
        </CardDescription>
      </CardHeader>
      <CardContent>
        <div className="grid gap-4 sm:grid-cols-2">
          {fields.map(({ key, label, hint }) => {
            const value = draft?.[key] ?? ""
            const invalid = !fieldValid(asset, key, value)
            const id = `asset-${asset.token}-${key}`
            return (
              <div key={key} className="space-y-1.5">
                <Label htmlFor={id}>
                  {label} ({asset.token})
                </Label>
                <Input
                  id={id}
                  inputMode="decimal"
                  value={value}
                  disabled={readOnly}
                  onChange={(event) => onChange(key, event.target.value)}
                  aria-invalid={invalid}
                />
                <p className={invalid ? "text-xs text-destructive" : "text-xs text-muted-foreground"}>
                  {invalid ? `Amount ≥ 0 with at most ${tokenDecimals(asset.token)} decimals` : hint}
                </p>
              </div>
            )
          })}
        </div>
      </CardContent>
    </Card>
  )
}

function SweepSettingsCard({
  sweeps,
  drafts,
  readOnly,
  onChange,
}: {
  sweeps: SweepSetting[]
  drafts: Record<string, SweepDraft>
  readOnly: boolean
  onChange: (key: string, draft: SweepDraft) => void
}) {
  return (
    <Card>
      <CardHeader>
        <CardTitle>Sweeps</CardTitle>
        <CardDescription>
          A deposit address is swept into the treasury once its confirmed balance
          reaches the minimum. Disable a (network, token) to keep funds on the
          deposit addresses.
        </CardDescription>
      </CardHeader>
      <CardContent>
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Asset</TableHead>
              <TableHead>Minimum</TableHead>
              <TableHead className="text-right">Enabled</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {sweeps.map((sweep) => {
              const key = sweepKey(sweep)
              const draft = drafts[key] ?? { minimum: sweep.minimum, enabled: sweep.enabled }
              const invalid = !isValidDecimalAmount(draft.minimum, sweep.token, { allowZero: true })
              return (
                <TableRow key={key}>
                  <TableCell className="font-medium">
                    {networkLabel(sweep.network)} · {sweep.token}
                  </TableCell>
                  <TableCell>
                    <div className="flex items-center gap-2">
                      <Input
                        className="w-40"
                        inputMode="decimal"
                        value={draft.minimum}
                        disabled={readOnly}
                        aria-label={`Sweep minimum, ${networkLabel(sweep.network)} ${sweep.token}`}
                        aria-invalid={invalid}
                        onChange={(event) => onChange(key, { ...draft, minimum: event.target.value })}
                      />
                      <span className="text-sm text-muted-foreground">{sweep.token}</span>
                    </div>
                  </TableCell>
                  <TableCell className="text-right">
                    <Switch
                      checked={draft.enabled}
                      disabled={readOnly}
                      aria-label={`Sweep ${networkLabel(sweep.network)} ${sweep.token}`}
                      onCheckedChange={(enabled) => onChange(key, { ...draft, enabled })}
                    />
                  </TableCell>
                </TableRow>
              )
            })}
          </TableBody>
        </Table>
      </CardContent>
    </Card>
  )
}
