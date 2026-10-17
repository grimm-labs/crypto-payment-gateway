import { useState } from "react"
import { Hourglass, Plus, Snowflake, Trash2 } from "lucide-react"
import { useSearchParams } from "react-router-dom"
import { toast } from "sonner"

import { CopyButton } from "@/components/shared/CopyButton"
import { ProjectBadge, ProjectDot } from "@/components/shared/ProjectBadge"
import { QueryError } from "@/components/shared/QueryError"
import { StatusBadge } from "@/components/shared/StatusBadge"
import { StepUpCodeField, StepUpDialog } from "@/components/shared/StepUpCodeField"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
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
import { useNow } from "@/hooks/use-now"
import {
  useAddressBook,
  useCreateAddressBookEntry,
  useDeleteAddressBookEntry,
} from "@/hooks/withdrawals"
import { useIsAdmin } from "@/lib/auth"
import { toastError } from "@/lib/errors"
import { formatCountdown, formatDate, networkLabel } from "@/lib/format"
import { NETWORKS, isNetwork } from "@/lib/networks"
import { useProjectScope } from "@/lib/project-scope"
import { MERCHANT_OWNER, type AddressBookEntry, type Network } from "@/lib/types"
import { isTotpCode } from "@/lib/validation"
import { DESTINATION_KIND_LABELS } from "@/lib/withdrawals"

const ALL = "all"
const LABEL_MAX_CHARS = 100
/** Mirrors the API check: `user@domain` with a dotted domain. */
const LN_ADDRESS = /^[A-Za-z0-9._-]+@([A-Za-z0-9-]+\.)+[A-Za-z]{2,}$/

export function AddressBookPage() {
  const [searchParams, setSearchParams] = useSearchParams()
  const networkParam = searchParams.get("network") ?? ALL
  const network = isNetwork(networkParam) ? networkParam : undefined
  const { scopeParam } = useProjectScope()
  const book = useAddressBook(network, scopeParam)
  const isAdmin = useIsAdmin()
  // Countdowns to the end of the cooling-off delay.
  const now = useNow(30_000)
  const [deleting, setDeleting] = useState<AddressBookEntry | null>(null)
  const [deleteOpen, setDeleteOpen] = useState(false)
  const remove = useDeleteAddressBookEntry()

  async function confirmDelete(code: string) {
    if (!deleting) return
    try {
      await remove.mutateAsync({ id: deleting.id, totp_code: code })
      toast.success(`“${deleting.label}” removed from the address book`)
      setDeleteOpen(false)
    } catch (error) {
      toastError(error, "Deletion failed")
    }
  }

  return (
    <div className="space-y-6 p-4 lg:p-6">
      <div className="flex flex-wrap items-end justify-between gap-4">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">Address book</h1>
          <p className="max-w-3xl text-sm text-muted-foreground">
            External destinations withdrawals may be sent to — required at or above
            an asset&apos;s approval threshold, and for merchant keys in book mode.
            Each entry belongs to a project (its keys and its admin withdrawals) or to
            the merchant funds. A new entry only becomes usable after a 24 h cooling-off
            delay. To change an entry, delete it and add it again (the delay restarts).
          </p>
        </div>
        {isAdmin && <AddEntryDialog defaultNetwork={network} />}
      </div>

      <Select
        value={network ?? ALL}
        onValueChange={(value) => {
          const next = new URLSearchParams(searchParams)
          if (value === ALL) next.delete("network")
          else next.set("network", value)
          setSearchParams(next)
        }}
      >
        <SelectTrigger className="w-40">
          <SelectValue placeholder="Network" />
        </SelectTrigger>
        <SelectContent>
          <SelectItem value={ALL}>All networks</SelectItem>
          {NETWORKS.map((value) => (
            <SelectItem key={value} value={value}>
              {networkLabel(value)}
            </SelectItem>
          ))}
        </SelectContent>
      </Select>

      {book.isError ? (
        <QueryError error={book.error} />
      ) : book.isLoading ? (
        <Skeleton className="h-64 w-full" />
      ) : (
        <div className="rounded-lg border">
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>Label</TableHead>
                <TableHead>Owner</TableHead>
                <TableHead>Network</TableHead>
                <TableHead>Destination</TableHead>
                <TableHead>Status</TableHead>
                <TableHead>Created by</TableHead>
                <TableHead>Created</TableHead>
                {isAdmin && <TableHead className="text-right">Actions</TableHead>}
              </TableRow>
            </TableHeader>
            <TableBody>
              {book.data?.data.length === 0 && (
                <TableRow>
                  <TableCell
                    colSpan={isAdmin ? 8 : 7}
                    className="h-24 text-center text-muted-foreground"
                  >
                    No address-book entries yet.
                  </TableCell>
                </TableRow>
              )}
              {book.data?.data.map((entry) => {
                const countdown = entry.active ? null : formatCountdown(entry.active_at, now)
                return (
                  <TableRow key={entry.id}>
                    <TableCell className="font-medium">
                      <div className="flex items-center gap-2">
                        {entry.label}
                        {entry.is_cold_storage && (
                          <Badge variant="outline" className="gap-1">
                            <Snowflake className="size-3" /> Cold storage
                          </Badge>
                        )}
                      </div>
                    </TableCell>
                    <TableCell>
                      <ProjectBadge projectId={entry.project_id} merchantLabel="Merchant funds" />
                    </TableCell>
                    <TableCell>{networkLabel(entry.network)}</TableCell>
                    <TableCell>
                      <div className="flex items-center gap-1">
                        <code className="max-w-72 truncate text-xs" title={entry.destination}>
                          {entry.destination}
                        </code>
                        <CopyButton value={entry.destination} />
                      </div>
                      <p className="text-xs text-muted-foreground">
                        {DESTINATION_KIND_LABELS[entry.destination_kind] ?? entry.destination_kind}
                      </p>
                    </TableCell>
                    <TableCell>
                      {entry.active ? (
                        <StatusBadge status="active" />
                      ) : (
                        <div className="space-y-0.5">
                          <StatusBadge status="cooling_off" />
                          <p className="flex items-center gap-1 text-xs text-muted-foreground">
                            <Hourglass className="size-3" />
                            {countdown ? `Usable in ${countdown}` : "Usable shortly"}
                          </p>
                        </div>
                      )}
                    </TableCell>
                    <TableCell className="text-muted-foreground">
                      {entry.created_by ?? "—"}
                    </TableCell>
                    <TableCell className="text-muted-foreground">
                      {formatDate(entry.created_at)}
                    </TableCell>
                    {isAdmin && (
                      <TableCell className="text-right">
                        <Button
                          variant="outline"
                          size="sm"
                          disabled={remove.isPending}
                          onClick={() => {
                            setDeleting(entry)
                            setDeleteOpen(true)
                          }}
                        >
                          <Trash2 className="size-3.5" /> Delete
                        </Button>
                      </TableCell>
                    )}
                  </TableRow>
                )
              })}
            </TableBody>
          </Table>
        </div>
      )}

      <StepUpDialog
        open={deleteOpen}
        onOpenChange={setDeleteOpen}
        title={`Delete “${deleting?.label ?? ""}”?`}
        description="Withdrawals can no longer use this entry — including pending ones, which the engine re-checks before signing. Adding it back restarts the 24 h cooling-off."
        confirmLabel="Delete entry"
        pendingLabel="Deleting…"
        destructive
        pending={remove.isPending}
        onConfirm={confirmDelete}
      />
    </div>
  )
}

function AddEntryDialog({ defaultNetwork }: { defaultNetwork?: Network }) {
  const [open, setOpen] = useState(false)
  // The owner (required): a project, pre-filled from the header selector,
  // or the merchant funds.
  const { projects, scopeParam } = useProjectScope()
  const [owner, setOwner] = useState(scopeParam ?? "")
  const [network, setNetwork] = useState<Network>(defaultNetwork ?? "tron")
  const [destination, setDestination] = useState("")
  const [label, setLabel] = useState("")
  const [coldStorage, setColdStorage] = useState(false)
  const [code, setCode] = useState("")
  const create = useCreateAddressBookEntry()

  const trimmedDestination = destination.trim()
  const trimmedLabel = label.trim()
  // Lightning entries are Lightning addresses only (invoices are single-use).
  const destinationProblem =
    network === "lightning" && trimmedDestination && !LN_ADDRESS.test(trimmedDestination)
      ? "Lightning entries are Lightning addresses (user@domain)"
      : null
  const ready =
    owner !== "" &&
    trimmedDestination !== "" &&
    !destinationProblem &&
    trimmedLabel !== "" &&
    trimmedLabel.length <= LABEL_MAX_CHARS &&
    isTotpCode(code)

  function handleOpenChange(next: boolean) {
    if (!next && create.isPending) return
    setOpen(next)
    // Start on the network and owner the list is filtered on.
    if (next) {
      setNetwork(defaultNetwork ?? "tron")
      setOwner(scopeParam ?? "")
    }
    if (!next) {
      setDestination("")
      setLabel("")
      setColdStorage(false)
      setCode("")
    }
  }

  async function submit(event: React.FormEvent) {
    event.preventDefault()
    if (!ready) return
    try {
      const entry = await create.mutateAsync({
        network,
        destination: trimmedDestination,
        label: trimmedLabel,
        is_cold_storage: coldStorage,
        totp_code: code.trim(),
        project_id: owner,
      })
      toast.success(`“${entry.label}” added — usable from ${formatDate(entry.active_at)}`)
      handleOpenChange(false)
    } catch (error) {
      toastError(error, "Could not add the entry")
    } finally {
      // Single-use codes: a retry needs a fresh one.
      setCode("")
    }
  }

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      <DialogTrigger asChild>
        <Button>
          <Plus className="size-4" /> Add entry
        </Button>
      </DialogTrigger>
      <DialogContent className="sm:max-w-md">
        <form onSubmit={submit} className="space-y-4">
          <DialogHeader>
            <DialogTitle>Add an address-book entry</DialogTitle>
            <DialogDescription>
              Usable for withdrawals 24 h after creation. Every staff member and
              webhook subscriber is alerted that it was added.
            </DialogDescription>
          </DialogHeader>
          <div className="space-y-2">
            <Label>Owner</Label>
            <Select value={owner} onValueChange={setOwner}>
              <SelectTrigger className="w-full">
                <SelectValue placeholder="Choose a project, or the merchant funds" />
              </SelectTrigger>
              <SelectContent>
                {projects.map((project) => (
                  <SelectItem key={project.id} value={project.id}>
                    <ProjectDot color={project.color} /> {project.name}
                  </SelectItem>
                ))}
                <SelectItem value={MERCHANT_OWNER}>Merchant funds (no project)</SelectItem>
              </SelectContent>
            </Select>
            <p className="text-xs text-muted-foreground">
              Only withdrawals of this owner may use the entry: a project&apos;s keys and
              its admin withdrawals, or merchant-funds withdrawals.
            </p>
          </div>
          <div className="space-y-2">
            <Label>Network</Label>
            <Select value={network} onValueChange={(value) => setNetwork(value as Network)}>
              <SelectTrigger className="w-full">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {NETWORKS.map((value) => (
                  <SelectItem key={value} value={value}>
                    {networkLabel(value)}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
            <p className="text-xs text-muted-foreground">
              An entry covers every token withdrawable on its network.
            </p>
          </div>
          <div className="space-y-2">
            <Label htmlFor="entry-destination">
              {network === "lightning" ? "Lightning address" : "Address"}
            </Label>
            <Input
              id="entry-destination"
              className="font-mono text-xs"
              autoComplete="off"
              spellCheck={false}
              placeholder={network === "lightning" ? "treasury@example.com" : `${networkLabel(network)} address`}
              value={destination}
              onChange={(event) => setDestination(event.target.value)}
            />
            {destinationProblem && <p className="text-xs text-destructive">{destinationProblem}</p>}
          </div>
          <div className="space-y-2">
            <Label htmlFor="entry-label">Label</Label>
            <Input
              id="entry-label"
              maxLength={LABEL_MAX_CHARS}
              placeholder="e.g. Exchange deposit, cold wallet"
              value={label}
              onChange={(event) => setLabel(event.target.value)}
            />
          </div>
          <div className="flex items-start gap-3">
            <Switch id="entry-cold" checked={coldStorage} onCheckedChange={setColdStorage} />
            <div className="grid gap-0.5 leading-none">
              <Label htmlFor="entry-cold">Cold storage</Label>
              <p className="text-xs text-muted-foreground">
                Offered by “Send excess to cold storage” when a treasury exceeds its
                hot-wallet cap.
              </p>
            </div>
          </div>
          <StepUpCodeField
            id="entry-code"
            value={code}
            onChange={setCode}
            disabled={create.isPending}
          />
          <DialogFooter>
            <Button type="submit" disabled={!ready || create.isPending}>
              {create.isPending ? "Adding…" : "Add entry"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}
