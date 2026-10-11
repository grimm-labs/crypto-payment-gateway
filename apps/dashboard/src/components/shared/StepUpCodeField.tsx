import { useState, type ReactNode } from "react"
import { ShieldAlert } from "lucide-react"
import { Link } from "react-router-dom"

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
import { useAuth } from "@/lib/auth"
import { ACCOUNT_SECURITY_PATH } from "@/lib/errors"
import { isTotpCode } from "@/lib/validation"

interface TotpCodeInputProps {
  id: string
  value: string
  onChange: (value: string) => void
  error?: string
  disabled?: boolean
  autoFocus?: boolean
  label?: string
  description?: ReactNode
}

/** Labelled 6-digit authenticator code input (digits only). */
export function TotpCodeInput({
  id,
  value,
  onChange,
  error,
  disabled,
  autoFocus,
  label = "Authentication code",
  description,
}: TotpCodeInputProps) {
  return (
    <div className="space-y-2">
      <Label htmlFor={id}>{label}</Label>
      <Input
        id={id}
        inputMode="numeric"
        autoComplete="one-time-code"
        maxLength={6}
        placeholder="123456"
        autoFocus={autoFocus}
        disabled={disabled}
        value={value}
        onChange={(event) => onChange(event.target.value.replace(/\D/g, "").slice(0, 6))}
        aria-invalid={Boolean(error)}
        className="text-center font-mono text-lg tracking-[0.5em]"
      />
      {error ? (
        <p className="text-xs text-destructive">{error}</p>
      ) : (
        description && <p className="text-xs text-muted-foreground">{description}</p>
      )}
    </div>
  )
}

/**
 * "Authentication code" input for step-up protected actions (payouts,
 * payout permissions and limits, staff changes). Codes are single-use, so
 * callers clear the value after every attempt. Admins without 2FA get a
 * notice pointing to Account security instead — the API refuses them.
 */
export function StepUpCodeField({
  description = "A fresh code from your authenticator app — each code works once.",
  ...props
}: TotpCodeInputProps) {
  const { staff } = useAuth()

  if (staff && !staff.totp_enabled) return <TwoFactorRequiredNotice />

  return <TotpCodeInput description={description} {...props} />
}

export function TwoFactorRequiredNotice() {
  return (
    <div className="flex items-start gap-2 rounded-lg border border-amber-300 bg-amber-50 p-3 text-sm dark:border-amber-800 dark:bg-amber-950/40">
      <ShieldAlert className="mt-0.5 size-4 shrink-0 text-amber-600" />
      <div className="space-y-1">
        <p>
          Two-factor authentication must be enabled on your account to perform
          this action.
        </p>
        <Link
          to={ACCOUNT_SECURITY_PATH}
          className="font-medium underline underline-offset-4"
        >
          Set it up in Account security →
        </Link>
      </div>
    </div>
  )
}

interface StepUpDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
  title: ReactNode
  description: ReactNode
  confirmLabel: string
  pendingLabel?: string
  destructive?: boolean
  pending: boolean
  /** Runs the action with the code; the caller closes the dialog on success. */
  onConfirm: (code: string) => Promise<void>
}

/** Confirmation dialog that asks for a step-up authentication code. */
export function StepUpDialog({ open, onOpenChange, ...props }: StepUpDialogProps) {
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-md">
        {/* Mounted per opening, so the code never carries over. */}
        <StepUpDialogForm {...props} onCancel={() => onOpenChange(false)} />
      </DialogContent>
    </Dialog>
  )
}

function StepUpDialogForm({
  title,
  description,
  confirmLabel,
  pendingLabel = "Confirming…",
  destructive,
  pending,
  onConfirm,
  onCancel,
}: Omit<StepUpDialogProps, "open" | "onOpenChange"> & { onCancel: () => void }) {
  const [code, setCode] = useState("")

  async function submit(event: React.FormEvent) {
    event.preventDefault()
    if (!isTotpCode(code)) return
    try {
      await onConfirm(code.trim())
    } finally {
      // Single-use codes: a retry needs a fresh one.
      setCode("")
    }
  }

  return (
    <form onSubmit={submit} className="space-y-4">
      <DialogHeader>
        <DialogTitle>{title}</DialogTitle>
        <DialogDescription>{description}</DialogDescription>
      </DialogHeader>
      <StepUpCodeField id="step-up-code" value={code} onChange={setCode} autoFocus />
      <DialogFooter>
        <Button type="button" variant="outline" onClick={onCancel}>
          Cancel
        </Button>
        <Button
          type="submit"
          variant={destructive ? "destructive" : "default"}
          disabled={!isTotpCode(code) || pending}
        >
          {pending ? pendingLabel : confirmLabel}
        </Button>
      </DialogFooter>
    </form>
  )
}
