import { useState } from "react"
import { KeyRound, Pencil, Plus, ShieldAlert, TriangleAlert, Wand2, Webhook } from "lucide-react"
import { toast } from "sonner"

import { CopyButton } from "@/components/shared/CopyButton"
import { QueryError } from "@/components/shared/QueryError"
import { StatusBadge } from "@/components/shared/StatusBadge"
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "@/components/ui/alert-dialog"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card"
import { Checkbox } from "@/components/ui/checkbox"
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
import {
  useCreateWebhookEndpoint,
  useToggleWebhookEndpoint,
  useUpdateWebhookEndpoint,
  useWebhookEndpoints,
} from "@/hooks/queries"
import { toastError } from "@/lib/errors"
import { formatDate } from "@/lib/format"
import type { WebhookEndpoint, WebhookEventType } from "@/lib/types"

const EVENT_OPTIONS: Array<{
  value: WebhookEventType
  label: string
  description: string
}> = [
  { value: "pending", label: "Pending", description: "A payment address is created" },
  { value: "paid", label: "Paid", description: "A deposit is confirmed on-chain" },
  { value: "expired", label: "Expired", description: "An address expires unpaid" },
  {
    value: "payout_succeeded",
    label: "Payout succeeded",
    description: "An outgoing Lightning wallet payment completed (any sender)",
  },
  {
    value: "payout_failed",
    label: "Payout failed",
    description: "An outgoing Lightning wallet payment failed (any sender)",
  },
]

const SECRET_ALPHABET = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789"
const SECRET_LENGTH = 32
/** Largest multiple of the alphabet size that fits in a byte (248 = 4 × 62):
 *  bytes at or above it are discarded, since `byte % 62` over the full
 *  0–255 range would favour the first 8 characters (#38). */
const UNBIASED_BYTE_LIMIT = 256 - (256 % SECRET_ALPHABET.length)

/** 32 uniformly random alphanumeric chars (rejection sampling). */
function generateSecret(): string {
  let secret = ""
  const bytes = new Uint8Array(64)
  while (secret.length < SECRET_LENGTH) {
    crypto.getRandomValues(bytes)
    for (const byte of bytes) {
      if (byte >= UNBIASED_BYTE_LIMIT) continue
      secret += SECRET_ALPHABET[byte % SECRET_ALPHABET.length]
      if (secret.length === SECRET_LENGTH) break
    }
  }
  return secret
}

const SECRET_RULES = "Secret must be 16-128 printable characters without spaces"

function validSecret(secret: string): boolean {
  return secret.length >= 16 && secret.length <= 128 && /^[\x21-\x7e]+$/.test(secret)
}

/** Client-side pre-checks; the API also requires a publicly resolvable host. */
function urlProblem(raw: string): string | null {
  let url: URL
  try {
    url = new URL(raw)
  } catch {
    return "Enter a valid URL, e.g. https://example.com/webhooks"
  }
  if (url.protocol !== "https:" && url.protocol !== "http:") {
    return "URL must start with https://"
  }
  if (url.username || url.password) {
    return "Credentials (user:password@) are not allowed in the URL"
  }
  return null
}

export function WebhooksPage() {
  const endpoints = useWebhookEndpoints()
  const toggleEndpoint = useToggleWebhookEndpoint()
  const [deactivating, setDeactivating] = useState<WebhookEndpoint | null>(null)
  const [editing, setEditing] = useState<WebhookEndpoint | null>(null)

  async function setActive(endpoint: WebhookEndpoint, isActive: boolean) {
    try {
      await toggleEndpoint.mutateAsync({ id: endpoint.id, is_active: isActive })
      toast.success(isActive ? "Endpoint activated" : "Endpoint deactivated")
    } catch (error) {
      toastError(error, "Update failed")
    } finally {
      setDeactivating(null)
    }
  }

  return (
    <div className="space-y-6 p-4 lg:p-6">
      <div className="flex flex-wrap items-end justify-between gap-4">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">Webhooks</h1>
          <p className="text-sm text-muted-foreground">
            Each active endpoint receives a POST for the events it subscribes
            to — pending (address created), paid (deposit confirmed), expired,
            and payout succeeded / failed (outgoing Lightning payments).
          </p>
        </div>
        <AddEndpointDialog />
      </div>

      {endpoints.isError ? (
        <QueryError error={endpoints.error} />
      ) : endpoints.isLoading ? (
        <Skeleton className="h-40 w-full" />
      ) : (
        <div className="rounded-lg border">
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>URL</TableHead>
                <TableHead>Events</TableHead>
                <TableHead>Status</TableHead>
                <TableHead>Created by</TableHead>
                <TableHead>Created</TableHead>
                <TableHead>Updated</TableHead>
                <TableHead className="text-right">Active</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {endpoints.data?.data.length === 0 && (
                <TableRow>
                  <TableCell colSpan={7} className="h-24 text-center text-muted-foreground">
                    No webhook endpoints yet — outgoing webhooks are disabled.
                  </TableCell>
                </TableRow>
              )}
              {endpoints.data?.data.map((endpoint) => (
                <TableRow key={endpoint.id}>
                  <TableCell>
                    <div className="flex items-center gap-2">
                      <Webhook className="size-3.5 shrink-0 text-muted-foreground" />
                      <code className="max-w-96 truncate text-xs">{endpoint.url}</code>
                      {endpoint.has_secret ? (
                        <KeyRound
                          className="size-3.5 shrink-0 text-muted-foreground"
                          aria-label="Signed deliveries"
                        >
                          <title>Deliveries are signed (X-Webhook-Signature-V2)</title>
                        </KeyRound>
                      ) : (
                        <Badge
                          asChild
                          variant="secondary"
                          className="cursor-pointer bg-amber-100 text-amber-900 hover:bg-amber-200 dark:bg-amber-900/40 dark:text-amber-200"
                        >
                          <button type="button" onClick={() => setEditing(endpoint)}>
                            <ShieldAlert />
                            Unsigned — add a secret
                          </button>
                        </Badge>
                      )}
                    </div>
                  </TableCell>
                  <TableCell>
                    <div className="flex items-center gap-1.5">
                      <div className="flex flex-wrap gap-1">
                        {endpoint.events.map((event) => (
                          <Badge key={event} variant="secondary" className="capitalize">
                            {event}
                          </Badge>
                        ))}
                      </div>
                      <Button
                        variant="ghost"
                        size="icon"
                        className="size-6"
                        onClick={() => setEditing(endpoint)}
                        aria-label={`Edit ${endpoint.url}`}
                      >
                        <Pencil className="size-3" />
                      </Button>
                    </div>
                  </TableCell>
                  <TableCell>
                    <StatusBadge status={endpoint.is_active ? "active" : "inactive"} />
                  </TableCell>
                  <TableCell className="text-muted-foreground">
                    {endpoint.created_by ?? "—"}
                  </TableCell>
                  <TableCell className="text-muted-foreground">
                    {formatDate(endpoint.created_at)}
                  </TableCell>
                  <TableCell className="text-muted-foreground">
                    {formatDate(endpoint.updated_at)}
                  </TableCell>
                  <TableCell className="text-right">
                    <Switch
                      checked={endpoint.is_active}
                      disabled={toggleEndpoint.isPending}
                      onCheckedChange={(checked) => {
                        if (checked) {
                          setActive(endpoint, true)
                        } else {
                          setDeactivating(endpoint)
                        }
                      }}
                      aria-label={`Toggle ${endpoint.url}`}
                    />
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        </div>
      )}

      <ReceiverDocsCard />

      <EditEndpointDialog endpoint={editing} onClose={() => setEditing(null)} />

      <AlertDialog
        open={deactivating !== null}
        onOpenChange={(open) => !open && setDeactivating(null)}
      >
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>Deactivate this endpoint?</AlertDialogTitle>
            <AlertDialogDescription className="break-all">
              {deactivating?.url} will stop receiving webhooks immediately —
              queued deliveries towards it will be skipped. You can reactivate
              it at any time.
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel>Cancel</AlertDialogCancel>
            <AlertDialogAction
              onClick={() => deactivating && setActive(deactivating, false)}
            >
              Deactivate
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </div>
  )
}

function EventChecklist({
  selected,
  onToggle,
}: {
  selected: WebhookEventType[]
  onToggle: (event: WebhookEventType, checked: boolean) => void
}) {
  return (
    <div className="space-y-3">
      {EVENT_OPTIONS.map((option) => (
        <div key={option.value} className="flex items-start gap-3">
          <Checkbox
            id={`event-${option.value}`}
            checked={selected.includes(option.value)}
            onCheckedChange={(checked) => onToggle(option.value, checked === true)}
          />
          <div className="grid gap-0.5 leading-none">
            <Label htmlFor={`event-${option.value}`}>{option.label}</Label>
            <p className="text-xs text-muted-foreground">{option.description}</p>
          </div>
        </div>
      ))}
    </div>
  )
}

function toggleEvent(
  events: WebhookEventType[],
  event: WebhookEventType,
  checked: boolean,
): WebhookEventType[] {
  const without = events.filter((e) => e !== event)
  return checked ? [...without, event] : without
}

function AddEndpointDialog() {
  const [open, setOpen] = useState(false)
  const [url, setUrl] = useState("")
  const [events, setEvents] = useState<WebhookEventType[]>(["paid"])
  const [secret, setSecret] = useState("")
  const createEndpoint = useCreateWebhookEndpoint()

  const plainHttp = url.trim().toLowerCase().startsWith("http://")

  function handleOpenChange(next: boolean) {
    setOpen(next)
    if (!next) {
      // The secret is write-only: never keep it around once the dialog is gone.
      setUrl("")
      setEvents(["paid"])
      setSecret("")
    }
  }

  async function handleCreate() {
    const trimmed = url.trim()
    const trimmedSecret = secret.trim()
    const problem = urlProblem(trimmed)
    if (problem) {
      toast.error(problem)
      return
    }
    if (events.length === 0) {
      toast.error("Select at least one event")
      return
    }
    if (!trimmedSecret) {
      toast.error("A signing secret is required — generate one")
      return
    }
    if (!validSecret(trimmedSecret)) {
      toast.error(SECRET_RULES)
      return
    }
    try {
      await createEndpoint.mutateAsync({ url: trimmed, events, secret: trimmedSecret })
      toast.success("Webhook endpoint added")
      handleOpenChange(false)
    } catch (error) {
      toastError(error, "Creation failed")
    }
  }

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      <DialogTrigger asChild>
        <Button size="sm">
          <Plus className="size-4" /> Add endpoint
        </Button>
      </DialogTrigger>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>Add webhook endpoint</DialogTitle>
          <DialogDescription>
            This URL will receive a signed POST for each selected event, with
            automatic retries on failure.
          </DialogDescription>
        </DialogHeader>
        <div className="space-y-4">
          <div className="space-y-2">
            <Label htmlFor="endpoint-url">URL</Label>
            <Input
              id="endpoint-url"
              placeholder="https://example.com/webhooks/deposits"
              value={url}
              onChange={(event) => setUrl(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === "Enter") handleCreate()
              }}
            />
            {plainHttp ? (
              <p className="flex items-start gap-1.5 text-xs text-amber-700 dark:text-amber-400">
                <TriangleAlert className="mt-0.5 size-3 shrink-0" />
                Plain http:// is only accepted when the API runs in development —
                use https://.
              </p>
            ) : (
              <p className="text-xs text-muted-foreground">
                Must use https:// and resolve to a public address.
              </p>
            )}
          </div>
          <div className="space-y-2">
            <Label>Events</Label>
            <EventChecklist
              selected={events}
              onToggle={(event, checked) =>
                setEvents((current) => toggleEvent(current, event, checked))
              }
            />
          </div>
          <div className="space-y-2">
            <Label htmlFor="endpoint-secret">Signing secret</Label>
            <div className="flex items-center gap-2">
              <Input
                id="endpoint-secret"
                className="font-mono text-xs"
                placeholder="Generate or paste a secret"
                value={secret}
                onChange={(event) => setSecret(event.target.value)}
                autoComplete="off"
                spellCheck={false}
              />
              {secret && <CopyButton value={secret} />}
              <Button
                type="button"
                variant="outline"
                size="sm"
                onClick={() => setSecret(generateSecret())}
              >
                <Wand2 className="size-3.5" /> Generate
              </Button>
            </div>
            <p className="text-xs text-muted-foreground">
              Required. Every delivery carries an X-Webhook-Signature-V2 header
              computed with it — see “Verifying deliveries” below. Store it in
              your receiver now: it cannot be read back later.
            </p>
          </div>
        </div>
        <DialogFooter>
          <Button onClick={handleCreate} disabled={createEndpoint.isPending}>
            {createEndpoint.isPending ? "Adding…" : "Add endpoint"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

function EditEndpointDialog({
  endpoint,
  onClose,
}: {
  endpoint: WebhookEndpoint | null
  onClose: () => void
}) {
  const [events, setEvents] = useState<WebhookEventType[]>([])
  const [secret, setSecret] = useState("")
  const [editingId, setEditingId] = useState<string | null>(null)
  const updateEndpoint = useUpdateWebhookEndpoint()

  // Reset the local state when a different endpoint is opened.
  if (endpoint && endpoint.id !== editingId) {
    setEditingId(endpoint.id)
    setEvents(endpoint.events)
    setSecret("")
  }

  async function handleSave() {
    if (!endpoint) return
    if (events.length === 0) {
      toast.error("Select at least one event")
      return
    }
    const trimmedSecret = secret.trim()
    if (trimmedSecret && !validSecret(trimmedSecret)) {
      toast.error(SECRET_RULES)
      return
    }
    // A new value rotates the secret; empty leaves it as-is server-side.
    // A secret can no longer be removed.
    try {
      await updateEndpoint.mutateAsync({
        id: endpoint.id,
        events,
        ...(trimmedSecret ? { secret: trimmedSecret } : {}),
      })
      toast.success(
        trimmedSecret
          ? endpoint.has_secret
            ? "Endpoint updated — signing secret rotated"
            : "Endpoint updated — deliveries are now signed"
          : "Endpoint updated",
      )
      setEditingId(null)
      setSecret("")
      onClose()
    } catch (error) {
      toastError(error, "Update failed")
    }
  }

  return (
    <Dialog
      open={endpoint !== null}
      onOpenChange={(open) => {
        if (!open) {
          setEditingId(null)
          setSecret("")
          onClose()
        }
      }}
    >
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>Edit endpoint</DialogTitle>
          <DialogDescription className="break-all">
            {endpoint?.url}
          </DialogDescription>
        </DialogHeader>
        <div className="space-y-4">
          <div className="space-y-2">
            <Label>Events</Label>
            <EventChecklist
              selected={events}
              onToggle={(event, checked) =>
                setEvents((current) => toggleEvent(current, event, checked))
              }
            />
          </div>
          <div className="space-y-2">
            <Label htmlFor="edit-endpoint-secret">Signing secret</Label>
            <div className="flex items-center gap-2">
              <Input
                id="edit-endpoint-secret"
                className="font-mono text-xs"
                placeholder={
                  endpoint?.has_secret ? "Unchanged — type to rotate" : "Add a secret to sign deliveries"
                }
                value={secret}
                onChange={(event) => setSecret(event.target.value)}
                autoComplete="off"
                spellCheck={false}
              />
              {secret && <CopyButton value={secret} />}
              <Button
                type="button"
                variant="outline"
                size="sm"
                onClick={() => setSecret(generateSecret())}
              >
                <Wand2 className="size-3.5" /> Generate
              </Button>
            </div>
            {endpoint && !endpoint.has_secret ? (
              <p className="flex items-start gap-1.5 text-xs text-amber-700 dark:text-amber-400">
                <ShieldAlert className="mt-0.5 size-3 shrink-0" />
                Deliveries to this endpoint are unsigned, so the receiver cannot
                tell them from forged requests. Add a secret and configure it in
                the receiver.
              </p>
            ) : (
              <p className="text-xs text-muted-foreground">
                The current secret cannot be read back — set a new one to rotate
                it. Update the receiver at the same time.
              </p>
            )}
          </div>
        </div>
        <DialogFooter>
          <Button onClick={handleSave} disabled={updateEndpoint.isPending}>
            {updateEndpoint.isPending ? "Saving…" : "Save"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

const VERIFY_SNIPPET = `import crypto from "node:crypto"

// rawBody: the exact bytes received (Buffer) — never re-serialized JSON.
export function verifyWebhook(rawBody, headers, secret) {
  const parts = Object.fromEntries(
    (headers["x-webhook-signature-v2"] ?? "").split(",").map((part) => part.split("=")),
  )
  const timestamp = Number(parts.t)
  // Reject stale deliveries: a captured request cannot be replayed later.
  if (!Number.isInteger(timestamp) || Math.abs(Date.now() / 1000 - timestamp) > 300) {
    return false
  }
  const expected = crypto
    .createHmac("sha256", secret)
    .update(\`\${parts.t}.\`)
    .update(rawBody)
    .digest()
  const received = Buffer.from(parts.v1 ?? "", "hex")
  return received.length === expected.length && crypto.timingSafeEqual(received, expected)
}`

const DELIVERY_HEADERS: Array<{ name: string; description: string }> = [
  {
    name: "X-Webhook-Signature-V2",
    description:
      "t=<timestamp>,v1=<hex> — HMAC-SHA256 of \"<timestamp>.<raw body>\" keyed with the endpoint secret. Verify this one.",
  },
  {
    name: "X-Webhook-Timestamp",
    description: "Unix seconds when the delivery was signed (same value as t).",
  },
  {
    name: "X-Webhook-Id",
    description:
      "Delivery id, also event_id in the body — use it to ignore deliveries already processed.",
  },
  {
    name: "X-Webhook-Signature",
    description:
      "Legacy sha256=<hex> of the body alone. It does not cover the timestamp, so prefer V2.",
  },
]

function ReceiverDocsCard() {
  return (
    <Card>
      <CardHeader>
        <CardTitle>Verifying deliveries</CardTitle>
        <CardDescription>
          Every POST is signed with the endpoint secret. Payloads also carry
          event_id and occurred_at (RFC 3339) at the top level. Answer with a
          2xx quickly; anything else is retried.
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-4">
        <dl className="grid gap-x-4 gap-y-2 text-sm sm:grid-cols-[auto_1fr]">
          {DELIVERY_HEADERS.map((header) => (
            <div key={header.name} className="contents">
              <dt>
                <code className="text-xs font-medium">{header.name}</code>
              </dt>
              <dd className="text-muted-foreground">{header.description}</dd>
            </div>
          ))}
        </dl>
        <div className="space-y-2">
          <div className="flex items-center justify-between">
            <p className="text-sm font-medium">Node.js</p>
            <CopyButton value={VERIFY_SNIPPET} label="Copy" />
          </div>
          <pre className="overflow-auto rounded-md border bg-muted/50 p-3 font-mono text-xs leading-relaxed">
            {VERIFY_SNIPPET}
          </pre>
        </div>
      </CardContent>
    </Card>
  )
}
