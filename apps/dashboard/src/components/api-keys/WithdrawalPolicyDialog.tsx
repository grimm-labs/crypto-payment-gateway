import { useState } from "react"
import { TriangleAlert } from "lucide-react"
import { toast } from "sonner"

import { StepUpCodeField } from "@/components/shared/StepUpCodeField"
import { Button } from "@/components/ui/button"
import { Checkbox } from "@/components/ui/checkbox"
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
import { Switch } from "@/components/ui/switch"
import { useUpdateApiKeyGrant } from "@/hooks/queries"
import { toastError } from "@/lib/errors"
import { isValidDecimalAmount, networkLabel, rawToDecimal, tokenDecimals } from "@/lib/format"
import { KEY_WITHDRAWAL_TOKENS, networksForTokens } from "@/lib/networks"
import type { ApiKey, KeyWithdrawalToken, Network, WithdrawalPolicy } from "@/lib/types"
import { isTotpCode } from "@/lib/validation"

const POLICIES: Array<{ value: WithdrawalPolicy; label: string; description: string }> = [
  { value: "none", label: "None", description: "The key cannot withdraw." },
  {
    value: "book",
    label: "Book mode",
    description: "Only to active address-book entries. Per-key limits are optional.",
  },
  {
    value: "open",
    label: "Open mode",
    description: "To any valid destination, within the key's own limits (required per token).",
  },
]

type LimitDraft = { max: string; daily: string }

/** Edit a merchant key's destination policy, tokens, networks and limits. */
export function WithdrawalPolicyDialog({
  apiKey,
  onClose,
}: {
  apiKey: ApiKey | null
  onClose: () => void
}) {
  const update = useUpdateApiKeyGrant()
  return (
    <Dialog
      open={apiKey !== null}
      onOpenChange={(open) => {
        if (!open && !update.isPending) onClose()
      }}
    >
      <DialogContent className="max-h-[90vh] overflow-y-auto sm:max-w-lg">
        {/* Keyed per key: each opening starts from the saved grant. */}
        {apiKey && <PolicyForm key={apiKey.id} apiKey={apiKey} update={update} onDone={onClose} />}
      </DialogContent>
    </Dialog>
  )
}

function initialLimits(apiKey: ApiKey): Record<KeyWithdrawalToken, LimitDraft> {
  const limits = {} as Record<KeyWithdrawalToken, LimitDraft>
  for (const token of KEY_WITHDRAWAL_TOKENS) {
    const saved = apiKey.withdrawal_limits.find((limit) => limit.token === token)
    limits[token] = saved
      ? {
          max: saved.max_per_withdrawal ?? rawToDecimal(saved.max_per_withdrawal_raw, token),
          daily: saved.daily_max ?? rawToDecimal(saved.daily_max_raw, token),
        }
      : { max: "", daily: "" }
  }
  return limits
}

function PolicyForm({
  apiKey,
  update,
  onDone,
}: {
  apiKey: ApiKey
  update: ReturnType<typeof useUpdateApiKeyGrant>
  onDone: () => void
}) {
  const [policy, setPolicy] = useState<WithdrawalPolicy>(apiKey.withdrawal_policy)
  const [tokens, setTokens] = useState<KeyWithdrawalToken[]>(
    KEY_WITHDRAWAL_TOKENS.filter((token) => apiKey.withdrawal_tokens.includes(token)),
  )
  const [allNetworks, setAllNetworks] = useState(apiKey.withdrawal_networks === null)
  const [networks, setNetworks] = useState<Network[]>(apiKey.withdrawal_networks ?? [])
  const [limits, setLimits] = useState(() => initialLimits(apiKey))
  const [code, setCode] = useState("")

  const granting = policy !== "none"
  const canGrant = apiKey.expires_at !== null
  const networkOptions = networksForTokens(tokens)
  const selectedNetworks = networks.filter((network) => networkOptions.includes(network))

  function limitProblem(token: KeyWithdrawalToken): string | null {
    const { max, daily } = limits[token]
    const empty = max.trim() === "" && daily.trim() === ""
    if (empty) return policy === "open" ? "Required in open mode" : null
    const ok = (value: string) => isValidDecimalAmount(value, token, { allowZero: true })
    if (!ok(max) || !ok(daily)) {
      return `Both amounts, ≥ 0 with at most ${tokenDecimals(token)} decimals`
    }
    return null
  }

  const valid =
    !granting ||
    (canGrant &&
      tokens.length > 0 &&
      (allNetworks || selectedNetworks.length > 0) &&
      tokens.every((token) => limitProblem(token) === null) &&
      isTotpCode(code))

  function toggleToken(token: KeyWithdrawalToken, checked: boolean) {
    setTokens((current) =>
      checked
        ? KEY_WITHDRAWAL_TOKENS.filter((t) => t === token || current.includes(t))
        : current.filter((t) => t !== token),
    )
  }

  function toggleNetwork(network: Network, checked: boolean) {
    setNetworks((current) =>
      checked ? [...current.filter((n) => n !== network), network] : current.filter((n) => n !== network),
    )
  }

  async function submit(event: React.FormEvent) {
    event.preventDefault()
    if (!valid) return
    try {
      if (granting) {
        await update.mutateAsync({
          id: apiKey.id,
          withdrawal_policy: policy,
          withdrawal_tokens: tokens,
          withdrawal_networks: allNetworks ? null : selectedNetworks,
          withdrawal_limits: tokens
            .filter((token) => limits[token].max.trim() !== "" || limits[token].daily.trim() !== "")
            .map((token) => ({
              token,
              max_per_withdrawal: limits[token].max.trim(),
              daily_max: limits[token].daily.trim(),
            })),
          totp_code: code.trim(),
        })
        toast.success(`“${apiKey.name}” may now withdraw (${policy} mode)`)
      } else {
        // Revoking the permission needs no code.
        await update.mutateAsync({
          id: apiKey.id,
          withdrawal_policy: "none",
          withdrawal_tokens: [],
          withdrawal_networks: null,
          withdrawal_limits: [],
        })
        toast.success(`“${apiKey.name}” can no longer withdraw`)
      }
      onDone()
    } catch (error) {
      toastError(error, "Update failed")
    } finally {
      setCode("")
    }
  }

  return (
    <form onSubmit={submit} className="space-y-5">
      <DialogHeader>
        <DialogTitle>Withdrawals — “{apiKey.name}”</DialogTitle>
        <DialogDescription>
          The destination policy and limits of this merchant key. Gas assets (TRX,
          SOL) are admin-only; amounts above an asset&apos;s approval threshold still
          need an admin&apos;s approval.
        </DialogDescription>
      </DialogHeader>

      <div className="space-y-2">
        <Label>Destination policy</Label>
        <Select value={policy} onValueChange={(value) => setPolicy(value as WithdrawalPolicy)}>
          <SelectTrigger className="w-full">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {POLICIES.map((option) => (
              <SelectItem key={option.value} value={option.value}>
                {option.label}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
        <p className="text-xs text-muted-foreground">
          {POLICIES.find((option) => option.value === policy)?.description}
        </p>
      </div>

      {granting && !canGrant && (
        <div className="flex items-start gap-2 rounded-lg border border-amber-300 bg-amber-50 p-3 text-sm dark:border-amber-800 dark:bg-amber-950/40">
          <TriangleAlert className="mt-0.5 size-4 shrink-0 text-amber-600" />
          <p>Only keys with an expiry can be allowed to withdraw — create a new expiring key.</p>
        </div>
      )}

      {granting && canGrant && (
        <>
          <div className="space-y-2">
            <Label>Tokens</Label>
            <div className="flex flex-wrap gap-4">
              {KEY_WITHDRAWAL_TOKENS.map((token) => (
                <div key={token} className="flex items-center gap-2">
                  <Checkbox
                    id={`grant-token-${token}`}
                    checked={tokens.includes(token)}
                    onCheckedChange={(checked) => toggleToken(token, checked === true)}
                  />
                  <Label htmlFor={`grant-token-${token}`}>{token}</Label>
                </div>
              ))}
            </div>
            {tokens.length === 0 && (
              <p className="text-xs text-destructive">Select at least one token.</p>
            )}
          </div>

          <div className="space-y-2">
            <div className="flex items-center gap-2">
              <Switch id="grant-all-networks" checked={allNetworks} onCheckedChange={setAllNetworks} />
              <Label htmlFor="grant-all-networks">All networks of these tokens</Label>
            </div>
            {!allNetworks && (
              <div className="flex flex-wrap gap-4 pt-1">
                {networkOptions.map((network) => (
                  <div key={network} className="flex items-center gap-2">
                    <Checkbox
                      id={`grant-network-${network}`}
                      checked={networks.includes(network)}
                      onCheckedChange={(checked) => toggleNetwork(network, checked === true)}
                    />
                    <Label htmlFor={`grant-network-${network}`}>{networkLabel(network)}</Label>
                  </div>
                ))}
                {selectedNetworks.length === 0 && (
                  <p className="w-full text-xs text-destructive">Select at least one network.</p>
                )}
              </div>
            )}
          </div>

          {tokens.length > 0 && (
            <div className="space-y-3">
              <div>
                <Label>Per-key limits</Label>
                <p className="text-xs text-muted-foreground">
                  On top of the global asset limits. Token amounts; 0 disables the token
                  for this key.{policy === "book" ? " Leave both empty for no per-key limit." : ""}
                </p>
              </div>
              {tokens.map((token) => {
                const problem = limitProblem(token)
                return (
                  <div key={token} className="space-y-1">
                    <div className="grid grid-cols-[3rem_1fr_1fr] items-center gap-2">
                      <span className="text-sm font-medium">{token}</span>
                      <Input
                        inputMode="decimal"
                        placeholder="Per withdrawal"
                        aria-label={`${token} max per withdrawal`}
                        value={limits[token].max}
                        aria-invalid={problem !== null}
                        onChange={(event) =>
                          setLimits((current) => ({
                            ...current,
                            [token]: { ...current[token], max: event.target.value },
                          }))
                        }
                      />
                      <Input
                        inputMode="decimal"
                        placeholder="Per 24 h"
                        aria-label={`${token} rolling 24 h max`}
                        value={limits[token].daily}
                        aria-invalid={problem !== null}
                        onChange={(event) =>
                          setLimits((current) => ({
                            ...current,
                            [token]: { ...current[token], daily: event.target.value },
                          }))
                        }
                      />
                    </div>
                    {problem && <p className="pl-14 text-xs text-destructive">{problem}</p>}
                  </div>
                )
              })}
            </div>
          )}

          <StepUpCodeField
            id="grant-code"
            value={code}
            onChange={setCode}
            disabled={update.isPending}
          />
        </>
      )}

      <DialogFooter>
        <Button type="button" variant="outline" disabled={update.isPending} onClick={onDone}>
          Cancel
        </Button>
        <Button
          type="submit"
          variant={granting ? "default" : "destructive"}
          disabled={!valid || update.isPending || (!granting && apiKey.withdrawal_policy === "none")}
        >
          {update.isPending ? "Saving…" : granting ? "Save policy" : "Remove withdrawal permission"}
        </Button>
      </DialogFooter>
    </form>
  )
}
