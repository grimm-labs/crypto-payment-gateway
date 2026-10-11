import { useState } from "react"
import { Ban, Check, Flame, Rocket, X } from "lucide-react"
import { toast } from "sonner"

import { StepUpDialog } from "@/components/shared/StepUpCodeField"
import { Button } from "@/components/ui/button"
import { useNow } from "@/hooks/use-now"
import { useWithdrawalAction } from "@/hooks/withdrawals"
import { useAuth } from "@/lib/auth"
import { toastError } from "@/lib/errors"
import { formatRaw } from "@/lib/format"
import type { Withdrawal } from "@/lib/types"
import { approvalBlock, availableActions, type WithdrawalAction } from "@/lib/withdrawals"

interface ActionSpec {
  label: string
  icon: React.ReactNode
  title: string
  description: string
  confirmLabel: string
  pendingLabel: string
  success: string
  destructive?: boolean
}

const ACTIONS: Record<WithdrawalAction, ActionSpec> = {
  approve: {
    label: "Approve",
    icon: <Check className="size-3.5" />,
    title: "Approve this withdrawal?",
    description:
      "Balances and limits are checked again, then it is queued for signing and broadcast.",
    confirmLabel: "Approve",
    pendingLabel: "Approving…",
    success: "Withdrawal approved — it is queued for sending",
  },
  reject: {
    label: "Reject",
    icon: <X className="size-3.5" />,
    title: "Reject this withdrawal?",
    description: "It is cancelled and its reservation released. Nothing is sent.",
    confirmLabel: "Reject",
    pendingLabel: "Rejecting…",
    success: "Withdrawal rejected",
    destructive: true,
  },
  cancel: {
    label: "Cancel",
    icon: <Ban className="size-3.5" />,
    title: "Cancel this withdrawal?",
    description: "It is cancelled before anything is signed, and its reservation released.",
    confirmLabel: "Cancel withdrawal",
    pendingLabel: "Cancelling…",
    success: "Withdrawal cancelled",
    destructive: true,
  },
  bump: {
    label: "Bump fee",
    icon: <Rocket className="size-3.5" />,
    title: "Raise the fee cap?",
    description:
      "The fee cap doubles and the engine replaces the stuck transaction with a higher fee.",
    confirmLabel: "Bump fee",
    pendingLabel: "Bumping…",
    success: "Fee cap raised — a replacement transaction will be sent",
  },
  abandon: {
    label: "Abandon",
    icon: <Flame className="size-3.5" />,
    title: "Abandon this withdrawal?",
    description:
      "Only possible once the network proves none of its transactions can still land. It then fails for good.",
    confirmLabel: "Abandon",
    pendingLabel: "Abandoning…",
    success: "Withdrawal abandoned",
    destructive: true,
  },
}

/** Admin action buttons for one withdrawal, each confirmed with a fresh code. */
export function WithdrawalActions({
  withdrawal,
  size = "sm",
}: {
  withdrawal: Withdrawal
  size?: "sm" | "default"
}) {
  const { staff } = useAuth()
  const run = useWithdrawalAction()
  const [action, setAction] = useState<WithdrawalAction | null>(null)
  const [open, setOpen] = useState(false)
  const actions = availableActions(withdrawal.status)
  const now = useNow(60_000)
  const selfBlock = approvalBlock(withdrawal, staff?.id, now)

  if (staff?.role !== "admin" || actions.length === 0) return null

  async function confirm(code: string) {
    if (!action) return
    try {
      await run.mutateAsync({ id: withdrawal.id, action, totp_code: code })
      toast.success(ACTIONS[action].success)
      setOpen(false)
    } catch (error) {
      toastError(error, `${ACTIONS[action].label} failed`)
    }
  }

  const spec = action ? ACTIONS[action] : null

  return (
    <>
      <div className="flex flex-wrap justify-end gap-2">
        {actions.map((name) => (
          <Button
            key={name}
            size={size}
            variant={name === "approve" ? "default" : ACTIONS[name].destructive ? "destructive" : "outline"}
            disabled={run.isPending || (name === "approve" && selfBlock !== null)}
            title={name === "approve" && selfBlock ? selfBlock : undefined}
            onClick={() => {
              setAction(name)
              setOpen(true)
            }}
          >
            {ACTIONS[name].icon}
            {ACTIONS[name].label}
          </Button>
        ))}
      </div>
      <StepUpDialog
        open={open}
        onOpenChange={setOpen}
        title={spec?.title ?? ""}
        description={
          <>
            {spec?.description}{" "}
            <span className="font-medium text-foreground">
              {formatRaw(withdrawal.amount_raw, withdrawal.token)}
            </span>{" "}
            to <span className="break-all font-mono text-xs">{withdrawal.destination}</span>.
          </>
        }
        confirmLabel={spec?.confirmLabel ?? "Confirm"}
        pendingLabel={spec?.pendingLabel}
        destructive={spec?.destructive}
        pending={run.isPending}
        onConfirm={confirm}
      />
    </>
  )
}
