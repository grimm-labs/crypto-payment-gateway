import { useState } from "react"
import { FolderPlus, LogOut, RefreshCw } from "lucide-react"
import { useNavigate } from "react-router-dom"
import { toast } from "sonner"

import { ProjectColorPicker, ProjectFormFields, type ProjectFormValues } from "@/components/projects/ProjectForm"
import { Button } from "@/components/ui/button"
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card"
import { Checkbox } from "@/components/ui/checkbox"
import { Label } from "@/components/ui/label"
import { useCreateProject } from "@/hooks/projects"
import { useAuth } from "@/lib/auth"
import { toastError } from "@/lib/errors"
import type { UnassignedCounts } from "@/lib/types"

function plural(count: number, noun: string): string {
  return `${count} ${noun}${count === 1 ? "" : "s"}`
}

/** The blocking "Create your first project" step (spec §13), admins only. */
export function FirstProjectStep({
  unassigned,
  onCreated,
}: {
  unassigned: UnassignedCounts | null
  onCreated: (projectId: string) => void
}) {
  const navigate = useNavigate()
  const { logout } = useAuth()
  const create = useCreateProject()
  const [values, setValues] = useState<ProjectFormValues>({ name: "", color: "blue", key_tag: "" })
  const [attach, setAttach] = useState(true)

  const hasLegacy =
    unassigned !== null &&
    (unassigned.api_keys > 0 ||
      unassigned.deposit_addresses > 0 ||
      unassigned.withdrawals > 0 ||
      unassigned.webhook_endpoints > 0 ||
      unassigned.address_book_entries > 0)
  const ready = values.name.trim().length > 0 && values.name.trim().length <= 60

  async function submit(event: React.FormEvent) {
    event.preventDefault()
    if (!ready || create.isPending) return
    try {
      const project = await create.mutateAsync({
        name: values.name.trim(),
        color: values.color,
        key_tag: values.key_tag.trim() || undefined,
        attach_existing: hasLegacy ? attach : undefined,
      })
      onCreated(project.id)
      toast.success(`Project “${project.name}” created`)
      navigate("/", { replace: true })
    } catch (error) {
      toastError(error, "Could not create the project")
    }
  }

  return (
    <div className="flex min-h-svh items-center justify-center bg-muted/40 p-4">
      <Card className="w-full max-w-md">
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <FolderPlus className="size-5" /> Create your first project
          </CardTitle>
          <CardDescription>
            A project is one of your applications. It owns its API keys, payments,
            withdrawals, webhook endpoints and address-book entries. Nothing can be
            created before the first one exists.
          </CardDescription>
        </CardHeader>
        <CardContent>
          <form onSubmit={submit} className="space-y-4">
            <ProjectFormFields values={values} onChange={setValues} usedColors={[]} autoFocus />
            <ProjectColorPicker
              value={values.color}
              onChange={(color) => setValues({ ...values, color })}
              usedColors={[]}
            />
            {hasLegacy && unassigned && (
              <div className="flex items-start gap-3 rounded-lg border bg-muted/40 p-3">
                <Checkbox
                  id="attach-existing"
                  checked={attach}
                  onCheckedChange={(checked) => setAttach(checked === true)}
                />
                <div className="grid gap-1 leading-none">
                  <Label htmlFor="attach-existing">
                    Attach existing data to this project ({plural(unassigned.api_keys, "key")},{" "}
                    {plural(unassigned.deposit_addresses, "payment")},{" "}
                    {plural(unassigned.withdrawals, "withdrawal")},{" "}
                    {plural(unassigned.webhook_endpoints, "endpoint")})
                  </Label>
                  <p className="text-xs text-muted-foreground">
                    {attach
                      ? "Keys, payments, key withdrawals and address-book entries become this project's; endpoints subscribed to both payment and instance events are split in two."
                      : "Existing keys are revoked, endpoints keep their instance events only, and the rest stays the merchant's."}
                  </p>
                </div>
              </div>
            )}
            <div className="flex items-center justify-between gap-2">
              <Button type="button" variant="ghost" size="sm" onClick={() => void logout()}>
                <LogOut className="size-4" /> Sign out
              </Button>
              <Button type="submit" disabled={!ready || create.isPending}>
                {create.isPending ? "Creating…" : "Create project"}
              </Button>
            </div>
          </form>
        </CardContent>
      </Card>
    </div>
  )
}

/** Operators (and anyone while the list cannot load) wait here. */
export function OperatorWaitingScreen({
  title,
  message,
  onRefresh,
}: {
  title: string
  message: string
  onRefresh: () => void
}) {
  const { logout } = useAuth()
  return (
    <div className="flex min-h-svh flex-col items-center justify-center gap-3 bg-muted/40 p-6 text-center">
      <p className="text-lg font-semibold">{title}</p>
      <p className="max-w-md text-sm text-muted-foreground">{message}</p>
      <div className="flex gap-2">
        <Button onClick={onRefresh}>
          <RefreshCw className="size-4" /> Refresh
        </Button>
        <Button variant="outline" onClick={() => void logout()}>
          <LogOut className="size-4" /> Sign out
        </Button>
      </div>
    </div>
  )
}
