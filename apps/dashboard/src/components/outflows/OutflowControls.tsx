import { useState } from "react"
import { OctagonX, ShieldOff, Snowflake } from "lucide-react"
import { toast } from "sonner"

import { StepUpDialog } from "@/components/shared/StepUpCodeField"
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
import { useFreezeOutflows, useOutflows, useUnfreezeOutflows } from "@/hooks/withdrawals"
import { useIsAdmin } from "@/lib/auth"
import { toastError } from "@/lib/errors"

/** Emergency stop — any staff member, no code (speed matters more). */
export function FreezeButton({ compact = false }: { compact?: boolean }) {
  const freeze = useFreezeOutflows()

  async function handleFreeze() {
    try {
      await freeze.mutateAsync()
      toast.success("Outflows frozen — no withdrawal or sweep leaves the gateway")
    } catch (error) {
      toastError(error, "Could not freeze outflows")
    }
  }

  return (
    <AlertDialog>
      <AlertDialogTrigger asChild>
        <Button
          size={compact ? "sm" : "default"}
          // Solid red on purpose: it must be findable in an emergency.
          className="bg-red-600 text-white hover:bg-red-700 dark:bg-red-600 dark:hover:bg-red-700"
          disabled={freeze.isPending}
        >
          <Snowflake className="size-4" />
          {compact ? "Freeze" : "Freeze outflows"}
        </Button>
      </AlertDialogTrigger>
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>Freeze all outflows?</AlertDialogTitle>
          <AlertDialogDescription>
            No withdrawal, sweep or gas top-up will be signed or broadcast
            until an admin lifts the outflow freeze (with a fresh
            authentication code). New withdrawals are refused; queued ones
            wait. Deposits keep being detected.
          </AlertDialogDescription>
        </AlertDialogHeader>
        <AlertDialogFooter>
          <AlertDialogCancel>Cancel</AlertDialogCancel>
          <AlertDialogAction variant="destructive" onClick={handleFreeze}>
            Freeze outflows
          </AlertDialogAction>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  )
}

/** Lifting the freeze: ADMIN + fresh code; impossible while OUTFLOWS_FROZEN=true. */
export function UnfreezeButton({ forcedByEnvironment }: { forcedByEnvironment: boolean }) {
  const isAdmin = useIsAdmin()
  const unfreeze = useUnfreezeOutflows()
  const [open, setOpen] = useState(false)

  if (!isAdmin) return null

  async function handleUnfreeze(code: string) {
    try {
      await unfreeze.mutateAsync(code)
      toast.success("Outflows resumed")
      setOpen(false)
    } catch (error) {
      toastError(error, "Could not lift the freeze")
    }
  }

  return (
    <>
      <Button
        variant="outline"
        size="sm"
        className="bg-background text-foreground"
        disabled={forcedByEnvironment || unfreeze.isPending}
        title={
          forcedByEnvironment
            ? "OUTFLOWS_FROZEN=true forces the freeze — change the environment to lift it"
            : undefined
        }
        onClick={() => setOpen(true)}
      >
        <ShieldOff className="size-3.5" /> Unfreeze
      </Button>
      <StepUpDialog
        open={open}
        onOpenChange={setOpen}
        title="Lift the outflow freeze?"
        description="Queued withdrawals, sweeps and gas top-ups resume immediately."
        confirmLabel="Unfreeze outflows"
        pendingLabel="Unfreezing…"
        pending={unfreeze.isPending}
        onConfirm={handleUnfreeze}
      />
    </>
  )
}

/** Red banner across the whole app while outflows are frozen. */
export function FrozenBanner() {
  const outflows = useOutflows()
  if (!outflows.data?.frozen) return null
  const forced = outflows.data.forced_by_environment

  return (
    <div
      role="alert"
      className="flex flex-wrap items-center justify-between gap-3 border-b border-red-700 bg-red-600 px-4 py-2 text-sm text-white lg:px-6"
    >
      <div className="flex items-center gap-2">
        <OctagonX className="size-4 shrink-0" />
        <p>
          <span className="font-semibold">Outflows are frozen.</span> No withdrawal,
          sweep or gas top-up leaves the gateway
          {forced ? " — forced by OUTFLOWS_FROZEN in the environment." : "."}
        </p>
      </div>
      <UnfreezeButton forcedByEnvironment={forced} />
    </div>
  )
}

/** Header control: the freeze button, hidden while already frozen. */
export function HeaderFreezeControl() {
  const outflows = useOutflows()
  if (!outflows.data || outflows.data.frozen) return null
  return <FreezeButton compact />
}
