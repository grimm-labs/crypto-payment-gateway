import { useState } from "react"
import { FolderPlus, Pencil, Trash2 } from "lucide-react"
import { toast } from "sonner"

import {
  ProjectColorPicker,
  ProjectFormFields,
  type ProjectFormValues,
} from "@/components/projects/ProjectForm"
import { ProjectDot } from "@/components/shared/ProjectBadge"
import { QueryError } from "@/components/shared/QueryError"
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "@/components/ui/alert-dialog"
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
import { Skeleton } from "@/components/ui/skeleton"
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table"
import { useCreateProject, useDeleteProject, useUpdateProject } from "@/hooks/projects"
import { useIsAdmin } from "@/lib/auth"
import { toastError } from "@/lib/errors"
import { formatDate } from "@/lib/format"
import { useProjectScope } from "@/lib/project-scope"
import { nextProjectColor } from "@/lib/projects"
import type { Project } from "@/lib/types"

function isEmpty(project: Project): boolean {
  const c = project.counts
  return (
    c.api_keys === 0 &&
    c.deposit_addresses === 0 &&
    c.withdrawals === 0 &&
    c.webhook_endpoints === 0 &&
    c.address_book_entries === 0
  )
}

export function ProjectsPage() {
  const { projects, loaded, error, refetch, setSelected, selected } = useProjectScope()
  const isAdmin = useIsAdmin()
  const [editing, setEditing] = useState<Project | null>(null)
  const [deleting, setDeleting] = useState<Project | null>(null)
  const remove = useDeleteProject()

  async function confirmDelete() {
    if (!deleting) return
    try {
      await remove.mutateAsync(deleting.id)
      if (selected === deleting.id) setSelected("all")
      toast.success(`Project “${deleting.name}” deleted`)
    } catch (err) {
      toastError(err, "Deletion failed")
    } finally {
      setDeleting(null)
    }
  }

  return (
    <div className="space-y-6 p-4 lg:p-6">
      <div className="flex flex-wrap items-end justify-between gap-4">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">Projects</h1>
          <p className="max-w-3xl text-sm text-muted-foreground">
            Your applications on this gateway. Each project owns its API keys,
            payments, withdrawals, webhook endpoints and address-book entries; funds
            stay pooled in the treasuries and are tracked per project as a ledger.
          </p>
        </div>
        {isAdmin && <CreateProjectDialog usedColors={projects.map((p) => p.color)} />}
      </div>

      {error ? (
        <QueryError error={error} />
      ) : !loaded ? (
        <Skeleton className="h-64 w-full" />
      ) : (
        <div className="rounded-lg border">
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>Project</TableHead>
                <TableHead>Key tag</TableHead>
                <TableHead className="text-right">Keys</TableHead>
                <TableHead className="text-right">Payments</TableHead>
                <TableHead className="text-right">Withdrawals</TableHead>
                <TableHead className="text-right">Endpoints</TableHead>
                <TableHead className="text-right">Address book</TableHead>
                <TableHead>Created</TableHead>
                {isAdmin && <TableHead className="text-right">Actions</TableHead>}
              </TableRow>
            </TableHeader>
            <TableBody>
              {projects.map((project) => (
                <TableRow key={project.id}>
                  <TableCell className="font-medium">
                    <button
                      type="button"
                      className="flex items-center gap-2 underline-offset-4 hover:underline"
                      onClick={() => setSelected(project.id)}
                      title="Select this project in the header"
                    >
                      <ProjectDot color={project.color} />
                      {project.name}
                    </button>
                  </TableCell>
                  <TableCell>
                    {project.key_tag ? (
                      <code className="text-xs">{project.key_tag}_…</code>
                    ) : (
                      <span className="text-xs text-muted-foreground">global</span>
                    )}
                  </TableCell>
                  <TableCell className="text-right tabular-nums">{project.counts.api_keys}</TableCell>
                  <TableCell className="text-right tabular-nums">{project.counts.deposit_addresses}</TableCell>
                  <TableCell className="text-right tabular-nums">{project.counts.withdrawals}</TableCell>
                  <TableCell className="text-right tabular-nums">{project.counts.webhook_endpoints}</TableCell>
                  <TableCell className="text-right tabular-nums">{project.counts.address_book_entries}</TableCell>
                  <TableCell className="text-muted-foreground">{formatDate(project.created_at)}</TableCell>
                  {isAdmin && (
                    <TableCell className="text-right">
                      <div className="flex justify-end gap-1">
                        <Button variant="outline" size="sm" onClick={() => setEditing(project)}>
                          <Pencil className="size-3.5" /> Edit
                        </Button>
                        <Button
                          variant="outline"
                          size="sm"
                          disabled={!isEmpty(project) || projects.length <= 1}
                          title={
                            projects.length <= 1
                              ? "The last project cannot be deleted"
                              : !isEmpty(project)
                                ? "Only an empty project can be deleted"
                                : undefined
                          }
                          onClick={() => setDeleting(project)}
                        >
                          <Trash2 className="size-3.5" /> Delete
                        </Button>
                      </div>
                    </TableCell>
                  )}
                </TableRow>
              ))}
            </TableBody>
          </Table>
        </div>
      )}

      {editing && (
        <EditProjectDialog
          project={editing}
          usedColors={projects.filter((p) => p.id !== editing.id).map((p) => p.color)}
          onClose={() => {
            setEditing(null)
            refetch()
          }}
        />
      )}

      <AlertDialog open={deleting !== null} onOpenChange={(open) => !open && setDeleting(null)}>
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>Delete “{deleting?.name}”?</AlertDialogTitle>
            <AlertDialogDescription>
              The project owns nothing, so nothing else changes. This cannot be undone.
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel>Cancel</AlertDialogCancel>
            <AlertDialogAction onClick={confirmDelete}>Delete project</AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </div>
  )
}

function CreateProjectDialog({ usedColors }: { usedColors: Project["color"][] }) {
  const [open, setOpen] = useState(false)
  const [values, setValues] = useState<ProjectFormValues>(() => ({
    name: "",
    color: nextProjectColor(usedColors),
    key_tag: "",
  }))
  const create = useCreateProject()
  const { setSelected } = useProjectScope()
  const ready = values.name.trim().length > 0

  function handleOpenChange(next: boolean) {
    setOpen(next)
    if (next) setValues({ name: "", color: nextProjectColor(usedColors), key_tag: "" })
  }

  async function submit(event: React.FormEvent) {
    event.preventDefault()
    if (!ready || create.isPending) return
    try {
      const project = await create.mutateAsync({
        name: values.name.trim(),
        color: values.color,
        key_tag: values.key_tag.trim() || undefined,
      })
      toast.success(`Project “${project.name}” created`)
      setSelected(project.id)
      setOpen(false)
    } catch (error) {
      toastError(error, "Could not create the project")
    }
  }

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      <DialogTrigger asChild>
        <Button>
          <FolderPlus className="size-4" /> New project
        </Button>
      </DialogTrigger>
      <DialogContent className="sm:max-w-md">
        <form onSubmit={submit} className="space-y-4">
          <DialogHeader>
            <DialogTitle>New project</DialogTitle>
            <DialogDescription>
              Keys, payments, endpoints and address-book entries created for it
              belong to it for good.
            </DialogDescription>
          </DialogHeader>
          <ProjectFormFields values={values} onChange={setValues} usedColors={usedColors} autoFocus idPrefix="new-project" />
          <ProjectColorPicker
            value={values.color}
            onChange={(color) => setValues({ ...values, color })}
            usedColors={usedColors}
          />
          <DialogFooter>
            <Button type="submit" disabled={!ready || create.isPending}>
              {create.isPending ? "Creating…" : "Create project"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}

function EditProjectDialog({
  project,
  usedColors,
  onClose,
}: {
  project: Project
  usedColors: Project["color"][]
  onClose: () => void
}) {
  const [values, setValues] = useState<ProjectFormValues>({
    name: project.name,
    color: project.color,
    key_tag: project.key_tag ?? "",
  })
  const update = useUpdateProject()
  const dirty =
    values.name.trim() !== project.name ||
    values.color !== project.color ||
    values.key_tag.trim() !== (project.key_tag ?? "")
  const ready = dirty && values.name.trim().length > 0

  async function submit(event: React.FormEvent) {
    event.preventDefault()
    if (!ready || update.isPending) return
    try {
      await update.mutateAsync({
        id: project.id,
        ...(values.name.trim() !== project.name ? { name: values.name.trim() } : {}),
        ...(values.color !== project.color ? { color: values.color } : {}),
        ...(values.key_tag.trim() !== (project.key_tag ?? "")
          ? { key_tag: values.key_tag.trim() || null }
          : {}),
      })
      toast.success("Project updated")
      onClose()
    } catch (error) {
      toastError(error, "Update failed")
    }
  }

  return (
    <Dialog open onOpenChange={(open) => !open && onClose()}>
      <DialogContent className="sm:max-w-md">
        <form onSubmit={submit} className="space-y-4">
          <DialogHeader>
            <DialogTitle>Edit project</DialogTitle>
            <DialogDescription>
              Renaming never moves anything; a new tag only affects keys created from now on.
            </DialogDescription>
          </DialogHeader>
          <ProjectFormFields values={values} onChange={setValues} usedColors={usedColors} idPrefix="edit-project" />
          <ProjectColorPicker
            value={values.color}
            onChange={(color) => setValues({ ...values, color })}
            usedColors={usedColors}
          />
          <DialogFooter>
            <Button type="submit" disabled={!ready || update.isPending}>
              {update.isPending ? "Saving…" : "Save"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}
