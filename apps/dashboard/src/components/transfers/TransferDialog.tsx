import { useState } from "react"
import { ArrowRight } from "lucide-react"
import { toast } from "sonner"

import { ProjectBadge, ProjectDot } from "@/components/shared/ProjectBadge"
import { StepUpCodeField } from "@/components/shared/StepUpCodeField"
import { Button } from "@/components/ui/button"
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
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
import { useProjectBalance } from "@/hooks/projects"
import { useCreateTransfer } from "@/hooks/transfers"
import { toastError } from "@/lib/errors"
import { formatAmount, isValidDecimalAmount, networkLabel, satsToBtc, tokenDecimals } from "@/lib/format"
import { NETWORKS, PAYMENT_TOKENS, isBtcNetwork } from "@/lib/networks"
import { useProjectScope } from "@/lib/project-scope"
import { MERCHANT_OWNER, type Network, type Token } from "@/lib/types"
import { isTotpCode } from "@/lib/validation"

const NOTE_MAX_CHARS = 200
const MAX_SATS = 2_100_000_000_000_000n

/** Idempotency-Key per parameter set, kept across retries (see WithdrawalDialog). */
const transferKeys = new Map<string, string>()

function keyFor(signature: string): string {
  let value = transferKeys.get(signature)
  if (!value) {
    value = crypto.randomUUID()
    transferKeys.set(signature, value)
  }
  return value
}

export interface TransferPreset {
  from?: string
  to?: string
  network?: Network
  token?: Token
}

/**
 * Record an internal transfer (ADMIN + TOTP, spec §9): a ledger move of
 * one asset between two owners (a project, or the merchant funds); nothing
 * moves on chain. Undone with the reverse transfer.
 */
export function TransferDialog({
  open,
  onOpenChange,
  preset,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
  preset?: TransferPreset
}) {
  const create = useCreateTransfer()
  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        if (!next && create.isPending) return
        onOpenChange(next)
      }}
    >
      <DialogContent className="max-h-[90vh] overflow-y-auto sm:max-w-lg">
        {open && <TransferForm preset={preset} create={create} onDone={() => onOpenChange(false)} />}
      </DialogContent>
    </Dialog>
  )
}

function TransferForm({
  preset,
  create,
  onDone,
}: {
  preset?: TransferPreset
  create: ReturnType<typeof useCreateTransfer>
  onDone: () => void
}) {
  const { projects, scopeParam } = useProjectScope()
  const [from, setFrom] = useState(preset?.from ?? scopeParam ?? "")
  const [to, setTo] = useState(preset?.to ?? "")
  const [network, setNetwork] = useState<Network>(preset?.network ?? "tron")
  const [token, setToken] = useState<Token>(preset?.token ?? PAYMENT_TOKENS[preset?.network ?? "tron"][0])
  const [amount, setAmount] = useState("")
  const [note, setNote] = useState("")
  const [code, setCode] = useState("")

  const isBtc = isBtcNetwork(network)
  const fromProject = projects.find((p) => p.id === from)
  const fromBalance = useProjectBalance(fromProject?.id)
  const fromAvailable = fromBalance.data?.balances.find(
    (b) => b.network === network && b.token === token,
  )?.available
  const trimmedAmount = amount.trim()
  const amountValid = isBtc
    ? /^[1-9]\d*$/.test(trimmedAmount) && BigInt(trimmedAmount) <= MAX_SATS
    : isValidDecimalAmount(trimmedAmount, token)
  const noteValid = note.trim().length > 0 && note.trim().length <= NOTE_MAX_CHARS
  const ready = from !== "" && to !== "" && from !== to && amountValid && noteValid && isTotpCode(code)

  function changeNetwork(next: Network) {
    setNetwork(next)
    setToken(PAYMENT_TOKENS[next][0])
    setAmount("")
  }

  async function submit(event: React.FormEvent) {
    event.preventDefault()
    if (!ready || create.isPending) return
    const body = {
      from_project_id: from,
      to_project_id: to,
      network,
      token,
      ...(isBtc ? { amount_sats: trimmedAmount } : { amount: trimmedAmount }),
      note: note.trim(),
    }
    const signature = JSON.stringify(body)
    try {
      const { transfer, replayed } = await create.mutateAsync({
        ...body,
        totp_code: code.trim(),
        idempotencyKey: keyFor(signature),
      })
      transferKeys.delete(signature)
      toast[replayed ? "info" : "success"](
        replayed
          ? "This transfer was already recorded — nothing new was created"
          : `Transfer recorded: ${formatAmount(transfer.amount, transfer.token)}`,
      )
      onDone()
    } catch (error) {
      setCode("")
      toastError(error, "Transfer failed")
    }
  }

  const ownerItems = (
    <>
      {projects.map((project) => (
        <SelectItem key={project.id} value={project.id}>
          <ProjectDot color={project.color} /> {project.name}
        </SelectItem>
      ))}
      <SelectItem value={MERCHANT_OWNER}>Merchant funds (no project)</SelectItem>
    </>
  )

  return (
    <form onSubmit={submit} className="space-y-4">
      <DialogHeader>
        <DialogTitle>New internal transfer</DialogTitle>
        <DialogDescription>
          Moves an amount of one asset from one owner&apos;s ledger to another&apos;s.
          Nothing moves on chain; a mistake is undone with the reverse transfer.
          Allowed while outflows are frozen.
        </DialogDescription>
      </DialogHeader>

      <div className="grid gap-4 sm:grid-cols-[1fr_auto_1fr] sm:items-end">
        <div className="space-y-2">
          <Label>From</Label>
          <Select value={from} onValueChange={setFrom}>
            <SelectTrigger className="w-full">
              <SelectValue placeholder="Owner debited" />
            </SelectTrigger>
            <SelectContent>{ownerItems}</SelectContent>
          </Select>
        </div>
        <ArrowRight className="mb-2 hidden size-4 text-muted-foreground sm:block" />
        <div className="space-y-2">
          <Label>To</Label>
          <Select value={to} onValueChange={setTo}>
            <SelectTrigger className="w-full">
              <SelectValue placeholder="Owner credited" />
            </SelectTrigger>
            <SelectContent>{ownerItems}</SelectContent>
          </Select>
        </div>
      </div>
      {from !== "" && from === to && (
        <p className="text-xs text-destructive">Choose two different owners.</p>
      )}

      <div className="grid gap-4 sm:grid-cols-2">
        <div className="space-y-2">
          <Label>Network</Label>
          <Select value={network} onValueChange={(value) => changeNetwork(value as Network)}>
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
        </div>
        <div className="space-y-2">
          <Label>Token</Label>
          <Select
            value={token}
            onValueChange={(value) => setToken(value as Token)}
            disabled={PAYMENT_TOKENS[network].length === 1}
          >
            <SelectTrigger className="w-full">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {PAYMENT_TOKENS[network].map((value) => (
                <SelectItem key={value} value={value}>
                  {value}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div>
      </div>

      <div className="space-y-2">
        <Label htmlFor="transfer-amount">Amount ({isBtc ? "sats" : token})</Label>
        <Input
          id="transfer-amount"
          inputMode={isBtc ? "numeric" : "decimal"}
          placeholder={isBtc ? "150000" : "25.00"}
          value={amount}
          onChange={(event) => setAmount(event.target.value)}
          aria-invalid={trimmedAmount !== "" && !amountValid}
        />
        {isBtc && amountValid && (
          <p className="text-xs text-muted-foreground">≈ {satsToBtc(trimmedAmount)} BTC</p>
        )}
        {trimmedAmount !== "" && !amountValid && (
          <p className="text-xs text-destructive">
            {isBtc ? "Whole number of sats, at least 1" : `Positive amount, up to ${tokenDecimals(token)} decimals`}
          </p>
        )}
        <p className="text-xs text-muted-foreground">
          {fromProject ? (
            <>
              <ProjectBadge projectId={fromProject.id} /> available on {networkLabel(network)}:{" "}
              {fromAvailable !== undefined ? formatAmount(fromAvailable, token) : "…"}. A project never goes
              below zero through a transfer.
            </>
          ) : from === MERCHANT_OWNER ? (
            "Limited to the merchant funds of the asset (holdings minus every project balance); refused while the holdings are unknown."
          ) : (
            "Pick the owner debited first."
          )}
        </p>
      </div>

      <div className="space-y-2">
        <Label htmlFor="transfer-note">Note</Label>
        <Input
          id="transfer-note"
          maxLength={NOTE_MAX_CHARS}
          placeholder="Why the funds move (required)"
          value={note}
          onChange={(event) => setNote(event.target.value)}
        />
      </div>

      <StepUpCodeField id="transfer-code" value={code} onChange={setCode} disabled={create.isPending} />

      <DialogFooter>
        <Button type="submit" disabled={!ready || create.isPending}>
          {create.isPending ? "Recording…" : "Record transfer"}
        </Button>
      </DialogFooter>
    </form>
  )
}
