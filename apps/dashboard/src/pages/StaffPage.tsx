import { useMemo, useState } from "react"
import { zodResolver } from "@hookform/resolvers/zod"
import {
  Ban,
  CircleCheck,
  MoreHorizontal,
  Plus,
  ShieldCheck,
  ShieldOff,
  UserCog,
} from "lucide-react"
import { Controller, useForm } from "react-hook-form"
import { toast } from "sonner"
import { z } from "zod"

import { QueryError } from "@/components/shared/QueryError"
import { StatusBadge } from "@/components/shared/StatusBadge"
import { StepUpCodeField, StepUpDialog } from "@/components/shared/StepUpCodeField"
import { Avatar, AvatarFallback } from "@/components/ui/avatar"
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
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu"
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
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table"
import {
  useCreateStaff,
  useResetStaffTotp,
  useStaffList,
  useUpdateStaff,
} from "@/hooks/queries"
import { useAuth } from "@/lib/auth"
import { toastError } from "@/lib/errors"
import { formatDate, initials } from "@/lib/format"
import type { Staff } from "@/lib/types"
import { isTotpCode, optionalPasswordSchema, passwordSchema } from "@/lib/validation"

/** Account changes confirmed with a step-up code. */
type MemberAction = { kind: "block" | "unblock" | "reset-totp"; member: Staff }

function fullName(member: Staff): string {
  return `${member.first_name} ${member.last_name}`
}

const ACTION_COPY: Record<
  MemberAction["kind"],
  { title: (member: Staff) => string; description: string; confirm: string; destructive?: boolean }
> = {
  block: {
    title: (member) => `Block ${fullName(member)}?`,
    description:
      "They will be signed out immediately and unable to log in until an admin unblocks them. Every API key they created is permanently revoked. Their account and history are kept.",
    confirm: "Block account",
    destructive: true,
  },
  unblock: {
    title: (member) => `Unblock ${fullName(member)}?`,
    description:
      "They can sign in again with their current password. API keys revoked when they were blocked stay revoked.",
    confirm: "Unblock account",
  },
  "reset-totp": {
    title: (member) => `Reset 2FA for ${fullName(member)}?`,
    description:
      "Their two-factor authentication will be removed and all their sessions revoked. They can sign in with their password only and re-enroll from the Account security page.",
    confirm: "Reset 2FA",
    destructive: true,
  },
}

export function StaffPage() {
  const { staff: me } = useAuth()
  const staffList = useStaffList()
  const updateStaff = useUpdateStaff()
  const resetStaffTotp = useResetStaffTotp()
  const [editing, setEditing] = useState<Staff | null>(null)
  const [action, setAction] = useState<MemberAction | null>(null)
  // Kept while the dialog animates out, so its copy doesn't flash empty.
  const [lastAction, setLastAction] = useState<MemberAction | null>(null)

  function openAction(next: MemberAction) {
    setAction(next)
    setLastAction(next)
  }

  async function runAction({ kind, member }: MemberAction, code: string) {
    try {
      if (kind === "reset-totp") {
        await resetStaffTotp.mutateAsync({ id: member.id, totp_code: code })
        toast.success(`2FA removed for ${fullName(member)} — their sessions were revoked`)
      } else if (kind === "block") {
        await updateStaff.mutateAsync({ id: member.id, is_active: false, totp_code: code })
        toast.success(`${fullName(member)} is blocked — their sessions were revoked`)
      } else {
        await updateStaff.mutateAsync({ id: member.id, is_active: true, totp_code: code })
        toast.success(`${fullName(member)} can sign in again`)
      }
      setAction(null)
    } catch (error) {
      toastError(
        error,
        kind === "reset-totp" ? "Reset failed" : kind === "block" ? "Blocking failed" : "Unblocking failed",
      )
    }
  }

  const copy = lastAction ? ACTION_COPY[lastAction.kind] : null

  return (
    <div className="space-y-6 p-4 lg:p-6">
      <div className="flex flex-wrap items-end justify-between gap-4">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">Staff</h1>
          <p className="text-sm text-muted-foreground">
            Dashboard accounts. Admins manage everything; operators handle daily
            operations only.
          </p>
        </div>
        <CreateStaffDialog />
      </div>

      {staffList.isError ? (
        <QueryError error={staffList.error} />
      ) : staffList.isLoading ? (
        <Skeleton className="h-64 w-full" />
      ) : (
        <div className="rounded-lg border">
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>Member</TableHead>
                <TableHead>Role</TableHead>
                <TableHead>Status</TableHead>
                <TableHead>2FA</TableHead>
                <TableHead>Created</TableHead>
                <TableHead className="text-right">Actions</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {staffList.data?.data.length === 0 && (
                <TableRow>
                  <TableCell colSpan={6} className="h-24 text-center text-muted-foreground">
                    No staff accounts found.
                  </TableCell>
                </TableRow>
              )}
              {staffList.data?.data.map((member) => (
                <TableRow key={member.id}>
                  <TableCell>
                    <div className="flex items-center gap-3">
                      <Avatar className="size-8">
                        <AvatarFallback className="text-xs">
                          {initials(member.first_name, member.last_name)}
                        </AvatarFallback>
                      </Avatar>
                      <div className="leading-tight">
                        <p className="font-medium">
                          {member.first_name} {member.last_name}
                          {member.id === me?.id && (
                            <span className="ml-2 text-xs text-muted-foreground">(you)</span>
                          )}
                        </p>
                        <p className="text-xs text-muted-foreground">{member.email}</p>
                      </div>
                    </div>
                  </TableCell>
                  <TableCell>
                    <StatusBadge status={member.role} />
                  </TableCell>
                  <TableCell>
                    <StatusBadge status={member.is_active ? "active" : "blocked"} />
                  </TableCell>
                  <TableCell>
                    {member.totp_enabled ? (
                      <span className="inline-flex items-center gap-1 text-sm">
                        <ShieldCheck className="size-3.5 text-emerald-600" /> Enabled
                      </span>
                    ) : (
                      <span className="text-muted-foreground">—</span>
                    )}
                  </TableCell>
                  <TableCell className="text-muted-foreground">
                    {formatDate(member.created_at)}
                  </TableCell>
                  <TableCell className="text-right">
                    <DropdownMenu>
                      <DropdownMenuTrigger asChild>
                        <Button variant="ghost" size="icon-sm" aria-label="Actions">
                          <MoreHorizontal className="size-4" />
                        </Button>
                      </DropdownMenuTrigger>
                      <DropdownMenuContent align="end">
                        <DropdownMenuItem onClick={() => setEditing(member)}>
                          <UserCog /> Edit
                        </DropdownMenuItem>
                        {member.id !== me?.id && member.totp_enabled && (
                          <DropdownMenuItem
                            onClick={() => openAction({ kind: "reset-totp", member })}
                          >
                            <ShieldOff /> Reset 2FA
                          </DropdownMenuItem>
                        )}
                        {member.id !== me?.id &&
                          (member.is_active ? (
                            member.role !== "admin" && (
                              <DropdownMenuItem
                                variant="destructive"
                                onClick={() => openAction({ kind: "block", member })}
                              >
                                <Ban /> Block
                              </DropdownMenuItem>
                            )
                          ) : (
                            <DropdownMenuItem
                              onClick={() => openAction({ kind: "unblock", member })}
                            >
                              <CircleCheck /> Unblock
                            </DropdownMenuItem>
                          ))}
                      </DropdownMenuContent>
                    </DropdownMenu>
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        </div>
      )}

      {editing && (
        <EditStaffDialog member={editing} onClose={() => setEditing(null)} />
      )}

      <StepUpDialog
        open={action !== null}
        onOpenChange={(open) => !open && setAction(null)}
        title={copy && lastAction ? copy.title(lastAction.member) : ""}
        description={copy?.description ?? ""}
        confirmLabel={copy?.confirm ?? "Confirm"}
        destructive={copy?.destructive}
        pending={updateStaff.isPending || resetStaffTotp.isPending}
        onConfirm={(code) => (action ? runAction(action, code) : Promise.resolve())}
      />
    </div>
  )
}

const createSchema = z.object({
  first_name: z.string().trim().min(1, "Required"),
  last_name: z.string().trim().min(1, "Required"),
  email: z.string().trim().email("Invalid email"),
  password: passwordSchema,
  role: z.enum(["admin", "operator"]),
  totp_code: z.string().trim().refine(isTotpCode, "Enter the 6-digit code"),
})

type CreateValues = z.infer<typeof createSchema>

const CREATE_DEFAULTS: Partial<CreateValues> = { role: "operator", totp_code: "" }

function CreateStaffDialog() {
  const { staff: me } = useAuth()
  const [open, setOpen] = useState(false)
  const createStaff = useCreateStaff()
  const form = useForm<CreateValues>({
    resolver: zodResolver(createSchema),
    defaultValues: CREATE_DEFAULTS,
  })

  async function onSubmit(values: CreateValues) {
    try {
      await createStaff.mutateAsync(values)
      toast.success(`${values.first_name} ${values.last_name} added`)
      setOpen(false)
      form.reset(CREATE_DEFAULTS)
    } catch (error) {
      toastError(error, "Creation failed")
      // Single-use codes: a retry needs a fresh one.
      form.setValue("totp_code", "")
    }
  }

  const { errors, isSubmitting } = form.formState

  function handleOpenChange(next: boolean) {
    setOpen(next)
    // Codes are single-use and short-lived: never carry one over.
    if (!next) form.setValue("totp_code", "")
  }

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      <DialogTrigger asChild>
        <Button>
          <Plus className="size-4" /> Add staff
        </Button>
      </DialogTrigger>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>Add staff member</DialogTitle>
          <DialogDescription>
            They can sign in immediately with the password you set.
          </DialogDescription>
        </DialogHeader>
        <form onSubmit={form.handleSubmit(onSubmit)} className="space-y-4">
          <div className="grid grid-cols-2 gap-4">
            <div className="space-y-2">
              <Label htmlFor="create-first-name">First name</Label>
              <Input id="create-first-name" {...form.register("first_name")} />
              {errors.first_name && (
                <p className="text-xs text-destructive">{errors.first_name.message}</p>
              )}
            </div>
            <div className="space-y-2">
              <Label htmlFor="create-last-name">Last name</Label>
              <Input id="create-last-name" {...form.register("last_name")} />
              {errors.last_name && (
                <p className="text-xs text-destructive">{errors.last_name.message}</p>
              )}
            </div>
          </div>
          <div className="space-y-2">
            <Label htmlFor="create-email">Email</Label>
            <Input id="create-email" type="email" {...form.register("email")} />
            {errors.email && <p className="text-xs text-destructive">{errors.email.message}</p>}
          </div>
          <div className="space-y-2">
            <Label htmlFor="create-password">Password</Label>
            <Input
              id="create-password"
              type="password"
              autoComplete="new-password"
              {...form.register("password")}
            />
            {errors.password ? (
              <p className="text-xs text-destructive">{errors.password.message}</p>
            ) : (
              <p className="text-xs text-muted-foreground">12 to 128 characters.</p>
            )}
          </div>
          <div className="space-y-2">
            <Label>Role</Label>
            <Select
              value={form.watch("role")}
              onValueChange={(value) => form.setValue("role", value as CreateValues["role"])}
            >
              <SelectTrigger className="w-full">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="operator">Operator — daily operations</SelectItem>
                <SelectItem value="admin">Admin — full access</SelectItem>
              </SelectContent>
            </Select>
          </div>
          <Controller
            control={form.control}
            name="totp_code"
            render={({ field, fieldState }) => (
              <StepUpCodeField
                id="create-staff-code"
                value={field.value ?? ""}
                onChange={field.onChange}
                error={fieldState.error?.message}
              />
            )}
          />
          <DialogFooter>
            <Button type="submit" disabled={isSubmitting || !me?.totp_enabled}>
              {isSubmitting ? "Adding…" : "Add member"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}

const editFields = z.object({
  first_name: z.string().trim().min(1, "Required"),
  last_name: z.string().trim().min(1, "Required"),
  role: z.enum(["admin", "operator"]),
  password: optionalPasswordSchema,
  totp_code: z.string().optional(),
})

type EditValues = z.infer<typeof editFields>

/** A role or password change needs a step-up code; names alone don't. */
function needsStepUp(values: Pick<EditValues, "role" | "password">, member: Staff): boolean {
  return values.role !== member.role || Boolean(values.password)
}

function editSchema(member: Staff) {
  return editFields.superRefine((values, ctx) => {
    if (needsStepUp(values, member) && !isTotpCode(values.totp_code)) {
      ctx.addIssue({
        code: z.ZodIssueCode.custom,
        path: ["totp_code"],
        message: "Enter the 6-digit code",
      })
    }
  })
}

function EditStaffDialog({ member, onClose }: { member: Staff; onClose: () => void }) {
  const updateStaff = useUpdateStaff()
  const schema = useMemo(() => editSchema(member), [member])
  const form = useForm<EditValues>({
    resolver: zodResolver(schema),
    defaultValues: {
      first_name: member.first_name,
      last_name: member.last_name,
      role: member.role,
      password: "",
      totp_code: "",
    },
  })
  const stepUp = needsStepUp(
    { role: form.watch("role"), password: form.watch("password") },
    member,
  )

  async function onSubmit(values: EditValues) {
    const sensitive = needsStepUp(values, member)
    try {
      await updateStaff.mutateAsync({
        id: member.id,
        first_name: values.first_name,
        last_name: values.last_name,
        // Only send what changed: an unchanged role must not require a code.
        ...(values.role !== member.role ? { role: values.role } : {}),
        ...(values.password ? { password: values.password } : {}),
        ...(sensitive ? { totp_code: values.totp_code?.trim() } : {}),
      })
      toast.success("Staff member updated")
      onClose()
    } catch (error) {
      toastError(error, "Update failed")
      form.setValue("totp_code", "")
    }
  }

  const { errors, isSubmitting } = form.formState

  return (
    <Dialog open onOpenChange={(open) => !open && onClose()}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>
            Edit {member.first_name} {member.last_name}
          </DialogTitle>
          <DialogDescription>{member.email}</DialogDescription>
        </DialogHeader>
        <form onSubmit={form.handleSubmit(onSubmit)} className="space-y-4">
          <div className="grid grid-cols-2 gap-4">
            <div className="space-y-2">
              <Label htmlFor="edit-first-name">First name</Label>
              <Input id="edit-first-name" {...form.register("first_name")} />
              {errors.first_name && (
                <p className="text-xs text-destructive">{errors.first_name.message}</p>
              )}
            </div>
            <div className="space-y-2">
              <Label htmlFor="edit-last-name">Last name</Label>
              <Input id="edit-last-name" {...form.register("last_name")} />
              {errors.last_name && (
                <p className="text-xs text-destructive">{errors.last_name.message}</p>
              )}
            </div>
          </div>
          <div className="space-y-2">
            <Label>Role</Label>
            <Select
              value={form.watch("role")}
              onValueChange={(value) => form.setValue("role", value as EditValues["role"])}
            >
              <SelectTrigger className="w-full">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="operator">Operator — daily operations</SelectItem>
                <SelectItem value="admin">Admin — full access</SelectItem>
              </SelectContent>
            </Select>
          </div>
          <div className="space-y-2">
            <Label htmlFor="edit-password">New password (optional)</Label>
            <Input
              id="edit-password"
              type="password"
              autoComplete="new-password"
              placeholder="Leave empty to keep current"
              {...form.register("password")}
            />
            {errors.password ? (
              <p className="text-xs text-destructive">{errors.password.message}</p>
            ) : (
              <p className="text-xs text-muted-foreground">12 to 128 characters.</p>
            )}
          </div>
          {stepUp && (
            <Controller
              control={form.control}
              name="totp_code"
              render={({ field, fieldState }) => (
                <StepUpCodeField
                  id="edit-staff-code"
                  value={field.value ?? ""}
                  onChange={field.onChange}
                  error={fieldState.error?.message}
                />
              )}
            />
          )}
          <DialogFooter>
            <Button type="submit" disabled={isSubmitting}>
              {isSubmitting ? "Saving…" : "Save changes"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}
