import { useMemo, useState } from "react"
import { zodResolver } from "@hookform/resolvers/zod"
import { LogOut, ShieldCheck, ShieldOff } from "lucide-react"
import { QRCodeSVG } from "qrcode.react"
import { Controller, useForm } from "react-hook-form"
import { useNavigate } from "react-router-dom"
import { toast } from "sonner"
import { z } from "zod"

import { CopyButton } from "@/components/shared/CopyButton"
import { QueryError } from "@/components/shared/QueryError"
import { TotpCodeInput } from "@/components/shared/StepUpCodeField"
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
import { Badge } from "@/components/ui/badge"
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
} from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { Skeleton } from "@/components/ui/skeleton"
import {
  useDisableTotp,
  useEnableTotp,
  useMe,
  useTotpSetup,
} from "@/hooks/queries"
import { useAuth } from "@/lib/auth"
import { toastError } from "@/lib/errors"
import type { TotpSetup } from "@/lib/types"
import { isTotpCode, passwordSchema } from "@/lib/validation"

export function AccountSecurityPage() {
  const me = useMe()

  return (
    <div className="space-y-6 p-4 lg:p-6">
      <div>
        <h1 className="text-2xl font-semibold tracking-tight">Account security</h1>
        <p className="text-sm text-muted-foreground">
          Your password, two-factor authentication and active sessions.
        </p>
      </div>
      <ChangePasswordCard totpEnabled={me.data?.totp_enabled ?? false} loading={me.isLoading} />
      <TwoFactorCard />
      <SessionsCard />
    </div>
  )
}

const changePasswordFields = z.object({
  current_password: z.string().min(1, "Current password is required"),
  new_password: passwordSchema,
  confirm_password: z.string(),
  /** Required iff the account has 2FA. */
  totp_code: z.string().optional(),
})

type ChangePasswordValues = z.infer<typeof changePasswordFields>

function changePasswordSchema(totpEnabled: boolean) {
  return changePasswordFields.superRefine((data, ctx) => {
    if (data.new_password !== data.confirm_password) {
      ctx.addIssue({
        code: z.ZodIssueCode.custom,
        path: ["confirm_password"],
        message: "Passwords do not match",
      })
    }
    if (totpEnabled && !isTotpCode(data.totp_code)) {
      ctx.addIssue({ code: z.ZodIssueCode.custom, path: ["totp_code"], message: "Enter the 6-digit code" })
    }
  })
}

const EMPTY_PASSWORD_FORM: ChangePasswordValues = {
  current_password: "",
  new_password: "",
  confirm_password: "",
  totp_code: "",
}

function ChangePasswordCard({
  totpEnabled,
  loading,
}: {
  totpEnabled: boolean
  loading: boolean
}) {
  const { changePassword } = useAuth()
  const schema = useMemo(() => changePasswordSchema(totpEnabled), [totpEnabled])
  const form = useForm<ChangePasswordValues>({
    resolver: zodResolver(schema),
    defaultValues: EMPTY_PASSWORD_FORM,
  })

  async function onSubmit(values: ChangePasswordValues) {
    try {
      await changePassword({
        current_password: values.current_password,
        new_password: values.new_password,
        ...(totpEnabled ? { totp_code: values.totp_code?.trim() } : {}),
      })
      toast.success("Password changed — every other session was signed out")
      form.reset(EMPTY_PASSWORD_FORM)
    } catch (error) {
      toastError(error, "Password change failed")
      // Codes are single-use: ask for a fresh one.
      form.setValue("totp_code", "")
    }
  }

  const { errors, isSubmitting } = form.formState

  return (
    <Card>
      <CardHeader>
        <CardTitle>Change password</CardTitle>
        <CardDescription>
          Signs out all your other sessions. This browser stays signed in.
        </CardDescription>
      </CardHeader>
      <CardContent>
        {loading ? (
          <Skeleton className="h-40 w-full max-w-md" />
        ) : (
          <form onSubmit={form.handleSubmit(onSubmit)} className="max-w-md space-y-4">
            <div className="space-y-2">
              <Label htmlFor="current-password">Current password</Label>
              <Input
                id="current-password"
                type="password"
                autoComplete="current-password"
                {...form.register("current_password")}
              />
              {errors.current_password && (
                <p className="text-xs text-destructive">{errors.current_password.message}</p>
              )}
            </div>
            <div className="space-y-2">
              <Label htmlFor="new-password">New password</Label>
              <Input
                id="new-password"
                type="password"
                autoComplete="new-password"
                {...form.register("new_password")}
              />
              {errors.new_password ? (
                <p className="text-xs text-destructive">{errors.new_password.message}</p>
              ) : (
                <p className="text-xs text-muted-foreground">12 to 128 characters.</p>
              )}
            </div>
            <div className="space-y-2">
              <Label htmlFor="confirm-new-password">Confirm new password</Label>
              <Input
                id="confirm-new-password"
                type="password"
                autoComplete="new-password"
                {...form.register("confirm_password")}
              />
              {errors.confirm_password && (
                <p className="text-xs text-destructive">{errors.confirm_password.message}</p>
              )}
            </div>
            {totpEnabled && (
              <Controller
                control={form.control}
                name="totp_code"
                render={({ field, fieldState }) => (
                  <TotpCodeInput
                    id="change-password-code"
                    value={field.value ?? ""}
                    onChange={field.onChange}
                    error={fieldState.error?.message}
                  />
                )}
              />
            )}
            <Button type="submit" disabled={isSubmitting}>
              {isSubmitting ? "Changing…" : "Change password"}
            </Button>
          </form>
        )}
      </CardContent>
    </Card>
  )
}

function TwoFactorCard() {
  const me = useMe()
  const enabled = me.data?.totp_enabled ?? false

  return (
    <Card>
      <CardHeader>
        <div className="flex items-center justify-between gap-4">
          <div className="space-y-1.5">
            <CardTitle>Two-factor authentication</CardTitle>
            <CardDescription>
              When enabled, signing in requires your password and a 6-digit code
              from an app such as Google Authenticator, Authy or 1Password.
              Admins need it for payouts and other sensitive actions.
            </CardDescription>
          </div>
          {!me.isLoading && !me.isError && (
            <Badge variant={enabled ? "default" : "outline"} className="shrink-0 gap-1">
              {enabled ? (
                <>
                  <ShieldCheck className="size-3.5" /> Enabled
                </>
              ) : (
                <>
                  <ShieldOff className="size-3.5" /> Disabled
                </>
              )}
            </Badge>
          )}
        </div>
      </CardHeader>
      <CardContent>
        {me.isError ? (
          <QueryError error={me.error} />
        ) : me.isLoading ? (
          <Skeleton className="h-9 w-40" />
        ) : enabled ? (
          <DisableSection />
        ) : (
          <EnableSection />
        )}
      </CardContent>
    </Card>
  )
}

function EnableSection() {
  const [open, setOpen] = useState(false)

  return (
    <>
      <Button onClick={() => setOpen(true)}>Enable 2FA</Button>
      <Dialog open={open} onOpenChange={setOpen}>
        <DialogContent className="sm:max-w-md">
          {/* Mounted per opening: password, secret and code never linger. */}
          <EnableTotpFlow onDone={() => setOpen(false)} />
        </DialogContent>
      </Dialog>
    </>
  )
}

function EnableTotpFlow({ onDone }: { onDone: () => void }) {
  const totpSetup = useTotpSetup()
  const enableTotp = useEnableTotp()
  const [password, setPassword] = useState("")
  const [setup, setSetup] = useState<TotpSetup | null>(null)
  const [code, setCode] = useState("")

  async function onStart(event: React.FormEvent) {
    event.preventDefault()
    if (!password) return
    try {
      // Each start generates a fresh pending secret (10-minute lifetime).
      setSetup(await totpSetup.mutateAsync(password))
      setPassword("")
    } catch (error) {
      toastError(error, "Failed to start 2FA setup")
    }
  }

  async function onEnable(event: React.FormEvent) {
    event.preventDefault()
    if (!isTotpCode(code)) {
      toast.error("Enter the 6-digit code from your app")
      return
    }
    try {
      await enableTotp.mutateAsync(code.trim())
      toast.success("Two-factor authentication enabled")
      onDone()
    } catch (error) {
      setCode("")
      toastError(error, "Verification failed")
    }
  }

  if (!setup) {
    return (
      <form onSubmit={onStart} className="space-y-4">
        <DialogHeader>
          <DialogTitle>Set up two-factor authentication</DialogTitle>
          <DialogDescription>Confirm your password to continue.</DialogDescription>
        </DialogHeader>
        <div className="space-y-2">
          <Label htmlFor="totp-setup-password">Password</Label>
          <Input
            id="totp-setup-password"
            type="password"
            autoComplete="current-password"
            autoFocus
            value={password}
            onChange={(event) => setPassword(event.target.value)}
          />
        </div>
        <DialogFooter>
          <Button type="button" variant="outline" onClick={onDone}>
            Cancel
          </Button>
          <Button type="submit" disabled={!password || totpSetup.isPending}>
            {totpSetup.isPending ? "Checking…" : "Continue"}
          </Button>
        </DialogFooter>
      </form>
    )
  }

  return (
    <form onSubmit={onEnable} className="space-y-4">
      <DialogHeader>
        <DialogTitle>Set up two-factor authentication</DialogTitle>
        <DialogDescription>
          Scan the QR code with your authenticator app, then enter the 6-digit
          code it shows to confirm.
        </DialogDescription>
      </DialogHeader>
      <div className="space-y-4">
        <div className="flex justify-center">
          {/* White backing so the QR scans in dark mode too. */}
          <div className="rounded-lg bg-white p-3">
            <QRCodeSVG value={setup.otpauth_url} size={192} />
          </div>
        </div>
        <div className="space-y-1">
          <p className="text-xs text-muted-foreground">Can't scan? Enter this key manually:</p>
          <div className="flex items-center justify-between gap-2 rounded-md border bg-muted/40 px-3 py-2">
            <code className="break-all font-mono text-xs">{setup.secret}</code>
            <CopyButton value={setup.secret} />
          </div>
        </div>
        <TotpCodeInput id="enable-code" value={code} onChange={setCode} autoFocus />
      </div>
      <DialogFooter>
        <Button type="button" variant="outline" onClick={onDone}>
          Cancel
        </Button>
        <Button type="submit" disabled={!isTotpCode(code) || enableTotp.isPending}>
          {enableTotp.isPending ? "Verifying…" : "Verify & enable"}
        </Button>
      </DialogFooter>
    </form>
  )
}

function DisableSection() {
  const [open, setOpen] = useState(false)

  return (
    <div className="space-y-3">
      <p className="text-sm text-muted-foreground">
        Your account is protected by an authenticator app. If you lose access to
        it, an administrator can reset your 2FA from the Staff page.
      </p>
      <Button
        variant="outline"
        className="text-destructive hover:text-destructive"
        onClick={() => setOpen(true)}
      >
        Disable 2FA
      </Button>
      <Dialog open={open} onOpenChange={setOpen}>
        <DialogContent className="sm:max-w-md">
          <DisableTotpForm onDone={() => setOpen(false)} />
        </DialogContent>
      </Dialog>
    </div>
  )
}

function DisableTotpForm({ onDone }: { onDone: () => void }) {
  const disableTotp = useDisableTotp()
  const [password, setPassword] = useState("")
  const [code, setCode] = useState("")

  async function onDisable(event: React.FormEvent) {
    event.preventDefault()
    if (!password || !isTotpCode(code)) return
    try {
      await disableTotp.mutateAsync({ password, code: code.trim() })
      toast.success("Two-factor authentication disabled")
      onDone()
    } catch (error) {
      setCode("")
      toastError(error, "Verification failed")
    }
  }

  return (
    <form onSubmit={onDisable} className="space-y-4">
      <DialogHeader>
        <DialogTitle>Disable two-factor authentication</DialogTitle>
        <DialogDescription>
          Your account will no longer require a code at sign-in, and sensitive
          admin actions will be blocked until you enable it again. Confirm with
          your password and a current code.
        </DialogDescription>
      </DialogHeader>
      <div className="space-y-2">
        <Label htmlFor="disable-password">Password</Label>
        <Input
          id="disable-password"
          type="password"
          autoComplete="current-password"
          autoFocus
          value={password}
          onChange={(event) => setPassword(event.target.value)}
        />
      </div>
      <TotpCodeInput id="disable-code" value={code} onChange={setCode} />
      <DialogFooter>
        <Button type="button" variant="outline" onClick={onDone}>
          Cancel
        </Button>
        <Button
          type="submit"
          variant="destructive"
          disabled={!password || !isTotpCode(code) || disableTotp.isPending}
        >
          {disableTotp.isPending ? "Verifying…" : "Disable 2FA"}
        </Button>
      </DialogFooter>
    </form>
  )
}

function SessionsCard() {
  const { logoutAll } = useAuth()
  const navigate = useNavigate()
  const [pending, setPending] = useState(false)

  async function signOutEverywhere() {
    setPending(true)
    try {
      await logoutAll()
      toast.success("Signed out of all sessions")
      navigate("/login", { replace: true })
    } catch (error) {
      toastError(error, "Could not sign out of all sessions")
    } finally {
      setPending(false)
    }
  }

  return (
    <Card>
      <CardHeader>
        <CardTitle>Sessions</CardTitle>
        <CardDescription>
          Signs you out everywhere — every browser and device, this one
          included. Use it if you suspect someone else accessed your account.
        </CardDescription>
      </CardHeader>
      <CardContent>
        <AlertDialog>
          <AlertDialogTrigger asChild>
            <Button variant="outline" disabled={pending}>
              <LogOut className="size-4" /> Sign out of all sessions
            </Button>
          </AlertDialogTrigger>
          <AlertDialogContent>
            <AlertDialogHeader>
              <AlertDialogTitle>Sign out of all sessions?</AlertDialogTitle>
              <AlertDialogDescription>
                Every session of your account is revoked immediately, including
                this one — you will need to sign in again.
              </AlertDialogDescription>
            </AlertDialogHeader>
            <AlertDialogFooter>
              <AlertDialogCancel>Cancel</AlertDialogCancel>
              <AlertDialogAction onClick={signOutEverywhere}>Sign out everywhere</AlertDialogAction>
            </AlertDialogFooter>
          </AlertDialogContent>
        </AlertDialog>
      </CardContent>
    </Card>
  )
}
