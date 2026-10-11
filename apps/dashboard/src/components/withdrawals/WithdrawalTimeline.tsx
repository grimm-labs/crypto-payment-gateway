import type { ReactNode } from "react"
import { Circle, CircleCheck, CircleDot, CircleX, TriangleAlert } from "lucide-react"

import { StaffName } from "@/components/withdrawals/RequesterLabel"
import { formatDate, humanize } from "@/lib/format"
import type { Withdrawal } from "@/lib/types"
import { cn } from "@/lib/utils"

type StepState = "done" | "current" | "pending" | "skipped" | "error" | "warn"

interface Step {
  label: string
  at: string | null
  state: StepState
  detail?: ReactNode
}

const ICONS: Record<StepState, ReactNode> = {
  done: <CircleCheck className="size-4 text-emerald-600" />,
  current: <CircleDot className="size-4 animate-pulse text-sky-600" />,
  pending: <Circle className="size-4 text-muted-foreground/50" />,
  skipped: <Circle className="size-4 text-muted-foreground/30" />,
  error: <CircleX className="size-4 text-destructive" />,
  warn: <TriangleAlert className="size-4 text-orange-600" />,
}

function steps(w: Withdrawal): Step[] {
  const ended = w.status === "cancelled" || w.status === "failed"
  const later = (done: boolean, current: boolean): StepState =>
    done ? "done" : current ? "current" : ended ? "skipped" : "pending"
  const list: Step[] = [{ label: "Requested", at: w.created_at, state: "done" }]

  if (w.approval_required) {
    list.push({
      label: "Approved",
      at: w.approved_at,
      state: later(w.approved_at !== null, w.status === "pending_approval"),
      detail: w.approved_at ? (
        <>by <StaffName id={w.approved_by} /></>
      ) : w.status === "pending_approval" ? (
        `Awaiting another admin — cancelled automatically after ${formatDate(w.approval_expires_at)}`
      ) : undefined,
    })
  }
  list.push({
    label: "Signed and broadcast",
    at: w.broadcast_at,
    state: later(
      w.broadcast_at !== null,
      w.status === "queued" || w.status === "signed",
    ),
    detail:
      w.status === "queued"
        ? "Queued — waits for the engine (and for the outflow freeze to be lifted, if any)"
        : undefined,
  })
  list.push({
    label: "Confirmed",
    at: w.confirmed_at,
    state: later(w.confirmed_at !== null, w.status === "broadcast"),
    detail:
      w.confirmations !== null && w.network !== "lightning"
        ? `${w.confirmations} confirmation${w.confirmations === 1 ? "" : "s"}`
        : undefined,
  })

  if (w.status === "cancelled") {
    list.push({
      label: "Cancelled",
      at: w.updated_at,
      state: "error",
      detail: w.cancel_reason ? humanize(w.cancel_reason) : undefined,
    })
  } else if (w.status === "failed") {
    list.push({
      label: "Failed",
      at: w.updated_at,
      state: "error",
      detail: w.failure_reason ? humanize(w.failure_reason) : undefined,
    })
  } else if (w.status === "stuck") {
    list.push({
      label: "Stuck",
      at: w.updated_at,
      state: "warn",
      detail:
        "The fee cap is reached (or the transaction cannot be proven expired yet): bump the fee or abandon it.",
    })
  }
  return list
}

/** State timeline of a withdrawal, from its timestamps and status. */
export function WithdrawalTimeline({ withdrawal }: { withdrawal: Withdrawal }) {
  return (
    <ol className="space-y-4">
      {steps(withdrawal).map((step) => (
        <li key={step.label} className="flex gap-3">
          <span className="mt-0.5">{ICONS[step.state]}</span>
          <div className="min-w-0">
            <p
              className={cn(
                "text-sm font-medium",
                (step.state === "pending" || step.state === "skipped") && "text-muted-foreground",
                step.state === "skipped" && "line-through",
              )}
            >
              {step.label}
            </p>
            {step.at && step.state !== "pending" && (
              <p className="text-xs text-muted-foreground">{formatDate(step.at)}</p>
            )}
            {step.detail && <p className="text-xs text-muted-foreground">{step.detail}</p>}
          </div>
        </li>
      ))}
    </ol>
  )
}
