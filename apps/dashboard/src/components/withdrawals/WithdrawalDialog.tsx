import { useState } from "react"
import { ArrowLeft } from "lucide-react"
import { useNavigate } from "react-router-dom"
import { toast } from "sonner"

import { StepUpCodeField } from "@/components/shared/StepUpCodeField"
import { QuoteSummary } from "@/components/withdrawals/QuoteSummary"
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
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group"
import { useAddressBook, useCreateWithdrawal, useWithdrawalQuote } from "@/hooks/withdrawals"
import { useNow } from "@/hooks/use-now"
import { toastError } from "@/lib/errors"
import {
  formatCountdown,
  isValidDecimalAmount,
  networkLabel,
  rawToDecimal,
  satsToBtc,
  shortHash,
  tokenDecimals,
} from "@/lib/format"
import { NETWORKS, WITHDRAWABLE_TOKENS, isBtcNetwork } from "@/lib/networks"
import type { Network, Token, WithdrawalQuote, WithdrawalRequest } from "@/lib/types"
import { isTotpCode } from "@/lib/validation"

const REFERENCE_MAX_CHARS = 128
const COMMENT_MAX_CHARS = 255
/** Total bitcoin supply, in sats. */
const MAX_SATS = 2_100_000_000_000_000n

/**
 * Idempotency-Key per withdrawal parameter set. Module-level on purpose: it
 * outlives the dialog, so retrying the same submission after an error — even
 * after closing the dialog — reuses the same key and the API answers with the
 * original withdrawal instead of sending twice. Editing a parameter selects
 * (or creates) another key; a key is dropped once its withdrawal is accepted.
 */
const withdrawalKeys = new Map<string, string>()

function signature(request: WithdrawalRequest): string {
  return JSON.stringify(request)
}

function idempotencyKeyFor(request: WithdrawalRequest): string {
  const key = signature(request)
  let value = withdrawalKeys.get(key)
  if (!value) {
    value = crypto.randomUUID()
    withdrawalKeys.set(key, value)
  }
  return value
}

export interface WithdrawalPreset {
  network?: Network
  token?: Token
  /** Base units — rendered into the amount field (sats on bitcoin/lightning). */
  amountRaw?: string
  addressBookEntryId?: string
}

interface WithdrawalDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
  preset?: WithdrawalPreset
  /** The network cannot be changed (e.g. the Lightning page). */
  lockNetwork?: boolean
  title?: string
}

/** Create a withdrawal: network → token → destination or address-book entry
 *  → amount → quote review → fresh code → submit. ADMIN only (the API
 *  refuses operators). */
export function WithdrawalDialog({ open, onOpenChange, ...props }: WithdrawalDialogProps) {
  const create = useCreateWithdrawal()

  function handleOpenChange(next: boolean) {
    // Never drop a submission in flight on the floor.
    if (!next && create.isPending) return
    onOpenChange(next)
  }

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      <DialogContent className="max-h-[90vh] overflow-y-auto sm:max-w-lg">
        {/* Mounted per opening: each one starts from its preset. */}
        <WithdrawalForm {...props} create={create} onDone={() => onOpenChange(false)} />
      </DialogContent>
    </Dialog>
  )
}

function initialAmount(preset: WithdrawalPreset | undefined, network: Network, token: Token) {
  if (!preset?.amountRaw) return ""
  return isBtcNetwork(network) ? preset.amountRaw : rawToDecimal(preset.amountRaw, token)
}

function WithdrawalForm({
  preset,
  lockNetwork,
  title = "New withdrawal",
  create,
  onDone,
}: Omit<WithdrawalDialogProps, "open" | "onOpenChange"> & {
  create: ReturnType<typeof useCreateWithdrawal>
  onDone: () => void
}) {
  const navigate = useNavigate()
  const now = useNow(60_000)
  const initialNetwork = preset?.network ?? "tron"
  const initialToken = preset?.token ?? WITHDRAWABLE_TOKENS[initialNetwork][0]
  const [network, setNetwork] = useState<Network>(initialNetwork)
  const [token, setToken] = useState<Token>(initialToken)
  const [mode, setMode] = useState<"book" | "destination">(
    preset?.addressBookEntryId ? "book" : "destination",
  )
  const [entryId, setEntryId] = useState(preset?.addressBookEntryId ?? "")
  const [destination, setDestination] = useState("")
  const [amount, setAmount] = useState(() => initialAmount(preset, initialNetwork, initialToken))
  const [reference, setReference] = useState("")
  const [comment, setComment] = useState("")
  const [quote, setQuote] = useState<WithdrawalQuote | null>(null)
  const [code, setCode] = useState("")
  const getQuote = useWithdrawalQuote()
  const book = useAddressBook(network)

  const isBtc = isBtcNetwork(network)
  const isLightning = network === "lightning"
  const entries = book.data?.data ?? []
  const entry = entries.find((e) => e.id === entryId)

  const trimmedAmount = amount.trim()
  const amountValid = isBtc
    ? /^[1-9]\d*$/.test(trimmedAmount) && BigInt(trimmedAmount) <= MAX_SATS
    : isValidDecimalAmount(trimmedAmount, token)
  const destinationValid = mode === "book" ? Boolean(entryId) : destination.trim() !== ""
  const referenceValid = reference.trim().length <= REFERENCE_MAX_CHARS
  const commentValid = comment.trim().length <= COMMENT_MAX_CHARS
  const formValid = amountValid && destinationValid && referenceValid && commentValid

  const request: WithdrawalRequest = {
    network,
    token,
    ...(isBtc ? { amount_sats: trimmedAmount } : { amount: trimmedAmount }),
    ...(mode === "book" ? { address_book_entry_id: entryId } : { destination: destination.trim() }),
    ...(reference.trim() ? { reference: reference.trim() } : {}),
    ...(isLightning && comment.trim() ? { comment: comment.trim() } : {}),
  }

  function changeNetwork(next: Network) {
    setNetwork(next)
    setToken(WITHDRAWABLE_TOKENS[next][0])
    // Units, destinations and book entries are all per network.
    setAmount("")
    setEntryId("")
    setDestination("")
    setComment("")
  }

  async function review(event: React.FormEvent) {
    event.preventDefault()
    if (!formValid) return
    try {
      setQuote(await getQuote.mutateAsync(request))
      setCode("")
    } catch (error) {
      toastError(error, "Could not get a quote")
    }
  }

  async function submit(event: React.FormEvent) {
    event.preventDefault()
    if (!quote || !quote.destination_acceptable || !isTotpCode(code)) return
    const idempotencyKey = idempotencyKeyFor(request)
    try {
      const { withdrawal, replayed } = await create.mutateAsync({
        ...request,
        totp_code: code.trim(),
        idempotencyKey,
      })
      withdrawalKeys.delete(signature(request))
      if (replayed) {
        toast.info("This withdrawal was already submitted — nothing new was created")
      } else if (withdrawal.status === "pending_approval") {
        toast.success("Withdrawal created — it now awaits another admin's approval")
      } else {
        toast.success("Withdrawal queued — it will be signed and broadcast shortly")
      }
      onDone()
      navigate(`/withdrawals/${withdrawal.id}`)
    } catch (error) {
      // The key is kept: retrying these parameters can never send twice.
      // The code was consumed (or wrong) — a fresh one is needed.
      setCode("")
      toastError(error, "Withdrawal failed")
    }
  }

  if (quote) {
    return (
      <form onSubmit={submit} className="space-y-4">
        <DialogHeader>
          <DialogTitle>Review withdrawal</DialogTitle>
          <DialogDescription>
            Check the destination, the amount and the fee before submitting.
          </DialogDescription>
        </DialogHeader>
        <QuoteSummary
          quote={quote}
          entry={mode === "book" ? entry : undefined}
          reference={reference.trim()}
          comment={isLightning ? comment.trim() : ""}
        />
        {quote.destination_acceptable && (
          <StepUpCodeField
            id="withdrawal-code"
            value={code}
            onChange={setCode}
            disabled={create.isPending}
            autoFocus
          />
        )}
        <DialogFooter>
          <Button
            type="button"
            variant="outline"
            disabled={create.isPending}
            onClick={() => {
              setQuote(null)
              setCode("")
            }}
          >
            <ArrowLeft className="size-4" /> Back
          </Button>
          <Button
            type="submit"
            disabled={!quote.destination_acceptable || !isTotpCode(code) || create.isPending}
          >
            {create.isPending ? "Submitting…" : "Submit withdrawal"}
          </Button>
        </DialogFooter>
      </form>
    )
  }

  const btcHint = isBtc && amountValid ? satsToBtc(trimmedAmount) : null

  return (
    <form onSubmit={review} className="space-y-4">
      <DialogHeader>
        <DialogTitle>{title}</DialogTitle>
        <DialogDescription>
          {isLightning
            ? "Pays from the Lightning wallet."
            : "On-chain withdrawals leave from the network's treasury."}{" "}
          Network fees come on top of the amount — you review them before submitting.
        </DialogDescription>
      </DialogHeader>

      <div className="grid gap-4 sm:grid-cols-2">
        <div className="space-y-2">
          <Label>Network</Label>
          <Select
            value={network}
            onValueChange={(value) => changeNetwork(value as Network)}
            disabled={lockNetwork}
          >
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
            disabled={WITHDRAWABLE_TOKENS[network].length === 1}
          >
            <SelectTrigger className="w-full">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {WITHDRAWABLE_TOKENS[network].map((value) => (
                <SelectItem key={value} value={value}>
                  {value}
                  {value === "TRX" || value === "SOL" ? " (gas reserve)" : ""}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div>
      </div>

      <div className="space-y-2">
        <Label>Destination</Label>
        <ToggleGroup
          type="single"
          variant="outline"
          size="sm"
          value={mode}
          onValueChange={(value) => value && setMode(value as "book" | "destination")}
        >
          <ToggleGroupItem value="destination">Destination</ToggleGroupItem>
          <ToggleGroupItem value="book">Address book</ToggleGroupItem>
        </ToggleGroup>
        {mode === "book" ? (
          <>
            <Select value={entryId} onValueChange={setEntryId}>
              <SelectTrigger className="w-full">
                <SelectValue
                  placeholder={book.isLoading ? "Loading…" : "Select an address-book entry"}
                />
              </SelectTrigger>
              <SelectContent>
                {entries.map((item) => {
                  const countdown = item.active ? null : formatCountdown(item.active_at, now)
                  return (
                    <SelectItem key={item.id} value={item.id} disabled={!item.active}>
                      {item.label} · {shortHash(item.destination, 6)}
                      {item.is_cold_storage ? " · cold storage" : ""}
                      {!item.active ? ` · usable in ${countdown ?? "a moment"}` : ""}
                    </SelectItem>
                  )
                })}
              </SelectContent>
            </Select>
            {!book.isLoading && entries.length === 0 && (
              <p className="text-xs text-muted-foreground">
                No {networkLabel(network)} entry in the address book yet.
              </p>
            )}
          </>
        ) : (
          <Input
            placeholder={
              isLightning ? "alice@example.com or lnbc1… invoice" : `${networkLabel(network)} address`
            }
            className="font-mono text-xs"
            autoComplete="off"
            spellCheck={false}
            value={destination}
            onChange={(event) => setDestination(event.target.value)}
          />
        )}
        <p className="text-xs text-muted-foreground">
          {isLightning
            ? "A Lightning address, or a BOLT11 invoice for exactly this amount. "
            : ""}
          At or above the approval threshold, only active address-book entries are accepted.
        </p>
      </div>

      <div className="space-y-2">
        <Label htmlFor="withdrawal-amount">Amount ({isBtc ? "sats" : token})</Label>
        <Input
          id="withdrawal-amount"
          inputMode={isBtc ? "numeric" : "decimal"}
          placeholder={isBtc ? "150000" : "25.00"}
          value={amount}
          onChange={(event) => setAmount(event.target.value)}
          aria-invalid={trimmedAmount !== "" && !amountValid}
        />
        {btcHint && <p className="text-xs text-muted-foreground">≈ {btcHint} BTC</p>}
        {trimmedAmount !== "" && !amountValid && (
          <p className="text-xs text-destructive">
            {isBtc
              ? "Whole number of sats, at least 1"
              : `Positive amount, up to ${tokenDecimals(token)} decimals`}
          </p>
        )}
      </div>

      <div className="grid gap-4 sm:grid-cols-2">
        <div className="space-y-2">
          <Label htmlFor="withdrawal-reference">Reference (optional)</Label>
          <Input
            id="withdrawal-reference"
            maxLength={REFERENCE_MAX_CHARS}
            placeholder="Invoice #1234"
            value={reference}
            onChange={(event) => setReference(event.target.value)}
          />
        </div>
        {isLightning && (
          <div className="space-y-2">
            <Label htmlFor="withdrawal-comment">Comment (optional)</Label>
            <Input
              id="withdrawal-comment"
              maxLength={COMMENT_MAX_CHARS}
              placeholder="Sent to the Lightning address"
              value={comment}
              onChange={(event) => setComment(event.target.value)}
            />
          </div>
        )}
      </div>

      <DialogFooter>
        <Button type="submit" disabled={!formValid || getQuote.isPending}>
          {getQuote.isPending ? "Getting a quote…" : "Review"}
        </Button>
      </DialogFooter>
    </form>
  )
}
