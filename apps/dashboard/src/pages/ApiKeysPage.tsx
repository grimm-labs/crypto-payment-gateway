import { useEffect, useState } from "react"
import { KeyRound, Plus, ShieldOff, TriangleAlert } from "lucide-react"
import { toast } from "sonner"

import { CopyButton } from "@/components/shared/CopyButton"
import { QueryError } from "@/components/shared/QueryError"
import { StatusBadge } from "@/components/shared/StatusBadge"
import { StepUpCodeField, StepUpDialog } from "@/components/shared/StepUpCodeField"
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
  AlertDialogTrigger,
} from "@/components/ui/alert-dialog"
import { Button } from "@/components/ui/button"
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card"
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog"
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
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table"
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip"
import {
  useApiKeys,
  useCreateApiKey,
  useRevokeApiKey,
  useSettings,
  useUpdateApiKey,
  useUpdateSettings,
} from "@/hooks/queries"
import { toastError } from "@/lib/errors"
import { formatDate } from "@/lib/format"
import type { ApiKey, CreatedApiKey } from "@/lib/types"
import { isTotpCode } from "@/lib/validation"

const NO_EXPIRY_SEND_TOOLTIP = "Only keys with an expiry can be allowed to send payouts"

export function ApiKeysPage() {
  const keys = useApiKeys()
  const revokeKey = useRevokeApiKey()
  const updateKey = useUpdateApiKey()
  // Granting the payout permission needs a step-up code: confirm in a
  // dialog. The key stays set while the dialog animates out.
  const [granting, setGranting] = useState<ApiKey | null>(null)
  const [grantOpen, setGrantOpen] = useState(false)

  function openGrant(key: ApiKey) {
    setGranting(key)
    setGrantOpen(true)
  }

  async function grantCanSend(key: ApiKey, code: string) {
    try {
      await updateKey.mutateAsync({ id: key.id, can_send: true, totp_code: code })
      toast.success(`“${key.name}” can now send Lightning payments`)
      setGrantOpen(false)
    } catch (error) {
      toastError(error, "Update failed")
    }
  }

  // Withdrawing the permission needs no code.
  async function withdrawCanSend(key: ApiKey) {
    try {
      await updateKey.mutateAsync({ id: key.id, can_send: false })
      toast.success(`“${key.name}” can no longer send payments`)
    } catch (error) {
      toastError(error, "Update failed")
    }
  }

  async function handleRevoke(id: string) {
    try {
      await revokeKey.mutateAsync(id)
      toast.success("API key revoked")
    } catch (error) {
      toastError(error, "Revocation failed")
    }
  }

  return (
    <div className="space-y-6 p-4 lg:p-6">
      <div className="flex flex-wrap items-end justify-between gap-4">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">API keys</h1>
          <p className="text-sm text-muted-foreground">
            Merchant keys for the <code className="text-xs">/api/v1</code> endpoints
            (sent via the <code className="text-xs">x-api-key</code> header).
          </p>
        </div>
        <CreateApiKeyDialog />
      </div>

      {keys.isError ? (
        <QueryError error={keys.error} />
      ) : keys.isLoading ? (
        <Skeleton className="h-64 w-full" />
      ) : (
        <div className="rounded-lg border">
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>Name</TableHead>
                <TableHead>Prefix</TableHead>
                <TableHead>Status</TableHead>
                <TableHead>Expires</TableHead>
                <TableHead>Payouts</TableHead>
                <TableHead>Created by</TableHead>
                <TableHead>Last used</TableHead>
                <TableHead>Created</TableHead>
                <TableHead className="text-right">Actions</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {keys.data?.data.length === 0 && (
                <TableRow>
                  <TableCell colSpan={9} className="h-24 text-center text-muted-foreground">
                    No API keys yet. Merchant calls to /api/v1 require one.
                  </TableCell>
                </TableRow>
              )}
              {keys.data?.data.map((key) => (
                <TableRow key={key.id}>
                  <TableCell className="font-medium">
                    <div className="flex items-center gap-2">
                      <KeyRound className="size-3.5 text-muted-foreground" />
                      {key.name}
                    </div>
                  </TableCell>
                  <TableCell>
                    <code className="text-xs">{key.prefix}_…</code>
                  </TableCell>
                  <TableCell>
                    <StatusBadge status={key.status} />
                  </TableCell>
                  <TableCell className="text-muted-foreground">
                    {key.expires_at ? formatDate(key.expires_at) : "Never"}
                  </TableCell>
                  <TableCell>
                    <CanSendSwitch
                      apiKey={key}
                      pending={updateKey.isPending}
                      onGrant={() => openGrant(key)}
                      onWithdraw={() => withdrawCanSend(key)}
                    />
                  </TableCell>
                  <TableCell className="text-muted-foreground">
                    {key.created_by ?? "—"}
                  </TableCell>
                  <TableCell className="text-muted-foreground">
                    {formatDate(key.last_used_at)}
                  </TableCell>
                  <TableCell className="text-muted-foreground">
                    {formatDate(key.created_at)}
                  </TableCell>
                  <TableCell className="text-right">
                    {key.status === "active" && (
                      <AlertDialog>
                        <AlertDialogTrigger asChild>
                          <Button variant="outline" size="sm" disabled={revokeKey.isPending}>
                            <ShieldOff className="size-3.5" /> Revoke
                          </Button>
                        </AlertDialogTrigger>
                        <AlertDialogContent>
                          <AlertDialogHeader>
                            <AlertDialogTitle>Revoke “{key.name}”?</AlertDialogTitle>
                            <AlertDialogDescription>
                              Requests using this key will be rejected immediately.
                              This cannot be undone.
                            </AlertDialogDescription>
                          </AlertDialogHeader>
                          <AlertDialogFooter>
                            <AlertDialogCancel>Cancel</AlertDialogCancel>
                            <AlertDialogAction onClick={() => handleRevoke(key.id)}>
                              Revoke key
                            </AlertDialogAction>
                          </AlertDialogFooter>
                        </AlertDialogContent>
                      </AlertDialog>
                    )}
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        </div>
      )}

      <StepUpDialog
        open={grantOpen}
        onOpenChange={setGrantOpen}
        title={`Allow “${granting?.name ?? ""}” to send payouts?`}
        description="The key will be able to pay any Lightning address from the wallet, within the payout limits set in Settings, until it expires or the permission is withdrawn."
        confirmLabel="Allow payouts"
        pending={updateKey.isPending}
        onConfirm={(code) => (granting ? grantCanSend(granting, code) : Promise.resolve())}
      />

      <ApiKeyPrefixCard />
    </div>
  )
}

function CanSendSwitch({
  apiKey,
  pending,
  onGrant,
  onWithdraw,
}: {
  apiKey: ApiKey
  pending: boolean
  onGrant: () => void
  onWithdraw: () => void
}) {
  // A key without expiry may still have its (legacy) permission withdrawn,
  // never granted.
  const blocked = !apiKey.can_send && apiKey.expires_at === null
  const toggle = (
    <Switch
      checked={apiKey.can_send}
      disabled={apiKey.status !== "active" || pending || blocked}
      aria-label={`Allow ${apiKey.name} to send Lightning payments`}
      onCheckedChange={(checked) => (checked ? onGrant() : onWithdraw())}
    />
  )
  if (!blocked || apiKey.status !== "active") return toggle
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        {/* Disabled controls swallow pointer events: the wrapper carries the tooltip. */}
        <span tabIndex={0} className="inline-flex">
          {toggle}
        </span>
      </TooltipTrigger>
      <TooltipContent>{NO_EXPIRY_SEND_TOOLTIP}</TooltipContent>
    </Tooltip>
  )
}

function ApiKeyPrefixCard() {
  const settings = useSettings()
  const updateSettings = useUpdateSettings()
  const [prefix, setPrefix] = useState("")

  useEffect(() => {
    if (settings.data) setPrefix(settings.data.api_key_prefix ?? "")
  }, [settings.data])

  const saved = settings.data?.api_key_prefix ?? ""
  const dirty = prefix !== saved
  const preview = `${prefix ? `${prefix}_` : ""}xxxxxxxx_${"·".repeat(24)}`

  async function save() {
    try {
      await updateSettings.mutateAsync({ api_key_prefix: prefix })
      toast.success(
        prefix
          ? `New API keys will start with “${prefix}_”`
          : "API key prefix removed — new keys have no tag",
      )
    } catch (error) {
      toastError(error, "Update failed")
    }
  }

  return (
    <Card>
      <CardHeader>
        <CardTitle>API key prefix</CardTitle>
        <CardDescription>
          Optional tag prepended to newly generated API keys (max 5 alphanumeric
          characters). Existing keys keep working unchanged.
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
                className="w-32"
                maxLength={5}
                placeholder="none"
                value={prefix}
                onChange={(event) =>
                  setPrefix(event.target.value.replace(/[^a-zA-Z0-9]/g, ""))
                }
                aria-label="API key prefix"
              />
              <Button onClick={save} disabled={!dirty || updateSettings.isPending} size="sm">
                {updateSettings.isPending ? "Saving…" : "Save"}
              </Button>
            </div>
            <div className="flex items-center gap-2 text-sm text-muted-foreground">
              <KeyRound className="size-3.5 shrink-0" />
              <span>
                New keys will look like <code className="text-xs">{preview}</code>
              </span>
            </div>
          </div>
        )}
      </CardContent>
    </Card>
  )
}

const EXPIRY_CHOICES = [
  { value: "never", label: "Indefinite — until revoked", days: null },
  { value: "30", label: "1 month", days: 30 },
  { value: "90", label: "3 months", days: 90 },
  { value: "180", label: "6 months", days: 180 },
  { value: "365", label: "1 year", days: 365 },
] as const

/** Expiry preselected when "can send" is ticked on a never-expiring draft. */
const DEFAULT_SEND_EXPIRY = "30"

function CreateApiKeyDialog() {
  const [open, setOpen] = useState(false)
  const [name, setName] = useState("")
  const [expiry, setExpiry] = useState<string>("never")
  const [canSend, setCanSend] = useState(false)
  const [code, setCode] = useState("")
  const [created, setCreated] = useState<CreatedApiKey | null>(null)
  const createKey = useCreateApiKey()

  const choice = EXPIRY_CHOICES.find((c) => c.value === expiry)
  // A key allowed to send payouts must expire and needs a step-up code.
  const ready =
    name.trim() !== "" && (!canSend || ((choice?.days ?? null) !== null && isTotpCode(code)))

  function handleCanSend(checked: boolean) {
    setCanSend(checked)
    setCode("")
    if (checked && expiry === "never") setExpiry(DEFAULT_SEND_EXPIRY)
  }

  async function handleCreate() {
    if (!name.trim()) {
      toast.error("Name is required")
      return
    }
    if (!ready) return
    try {
      const key = await createKey.mutateAsync({
        name: name.trim(),
        expires_in_days: choice?.days ?? null,
        can_send: canSend,
        ...(canSend ? { totp_code: code.trim() } : {}),
      })
      setCreated(key)
    } catch (error) {
      toastError(error, "Creation failed")
    } finally {
      setCode("")
    }
  }

  function handleOpenChange(next: boolean) {
    setOpen(next)
    if (!next) {
      setCreated(null)
      setName("")
      setExpiry("never")
      setCanSend(false)
      setCode("")
    }
  }

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      <DialogTrigger asChild>
        <Button>
          <Plus className="size-4" /> New API key
        </Button>
      </DialogTrigger>
      <DialogContent className="sm:max-w-md">
        {created ? (
          <>
            <DialogHeader>
              <DialogTitle>API key created</DialogTitle>
              <DialogDescription>“{created.name}”</DialogDescription>
            </DialogHeader>
            <div className="space-y-3">
              <div className="flex items-center gap-3 rounded-lg border border-amber-300 bg-amber-50 p-3 text-sm dark:border-amber-800 dark:bg-amber-950/40">
                <TriangleAlert className="size-4 shrink-0 text-amber-600" />
                <p>
                  Copy this key now — it is shown <span className="font-semibold">only once</span>.
                  Only a hash is stored on the server.
                </p>
              </div>
              <div className="flex items-center justify-between gap-2 rounded-lg border bg-muted/40 p-3">
                <code className="break-all text-sm">{created.key}</code>
                <CopyButton value={created.key} />
              </div>
              <p className="text-sm text-muted-foreground">
                {created.expires_at
                  ? `Expires on ${formatDate(created.expires_at)}.`
                  : "Never expires — valid until revoked."}
              </p>
            </div>
            <DialogFooter>
              <Button onClick={() => handleOpenChange(false)}>Done</Button>
            </DialogFooter>
          </>
        ) : (
          <>
            <DialogHeader>
              <DialogTitle>New API key</DialogTitle>
              <DialogDescription>
                The key grants full access to the merchant /api/v1 endpoints.
              </DialogDescription>
            </DialogHeader>
            <div className="space-y-4">
              <div className="space-y-2">
                <Label htmlFor="key-name">Name</Label>
                <Input
                  id="key-name"
                  placeholder="e.g. production-shop"
                  value={name}
                  onChange={(event) => setName(event.target.value)}
                  onKeyDown={(event) => {
                    if (event.key === "Enter") handleCreate()
                  }}
                />
              </div>
              <div className="space-y-2">
                <Label>Expiration</Label>
                <Select value={expiry} onValueChange={setExpiry}>
                  <SelectTrigger className="w-full">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    {EXPIRY_CHOICES.map((option) => (
                      <SelectItem
                        key={option.value}
                        value={option.value}
                        disabled={canSend && option.days === null}
                      >
                        {option.label}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
                <p className="text-xs text-muted-foreground">
                  {canSend
                    ? "A key allowed to send payouts must expire."
                    : "An expired key stops working immediately and cannot be reactivated."}
                </p>
              </div>
              <div className="flex items-start gap-3">
                <Switch id="key-can-send" checked={canSend} onCheckedChange={handleCanSend} />
                <div className="grid gap-0.5 leading-none">
                  <Label htmlFor="key-can-send">Can send Lightning payments</Label>
                  <p className="text-xs text-muted-foreground">
                    Lets this key pay any Lightning address from the wallet, within
                    the payout limits set in Settings. Leave off unless the
                    integration needs it.
                  </p>
                </div>
              </div>
              {canSend && (
                <StepUpCodeField id="key-create-code" value={code} onChange={setCode} />
              )}
            </div>
            <DialogFooter>
              <Button onClick={handleCreate} disabled={!ready || createKey.isPending}>
                {createKey.isPending ? "Creating…" : "Create key"}
              </Button>
            </DialogFooter>
          </>
        )}
      </DialogContent>
    </Dialog>
  )
}
