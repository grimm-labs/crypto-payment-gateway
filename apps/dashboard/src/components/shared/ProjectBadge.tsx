import { Landmark } from "lucide-react"

import { useProjectScope } from "@/lib/project-scope"
import { PROJECT_COLOR_CLASS } from "@/lib/projects"
import type { ProjectColor } from "@/lib/types"
import { cn } from "@/lib/utils"

export function ProjectDot({ color, className }: { color: ProjectColor; className?: string }) {
  return (
    <span
      aria-hidden
      className={cn("inline-block size-2.5 shrink-0 rounded-full", PROJECT_COLOR_CLASS[color], className)}
    />
  )
}

/**
 * The owner of a row: the project's colour dot and name, or "Merchant" for
 * merchant-owned rows (endpoints, withdrawals, address-book entries). A
 * project id the list does not know (deleted meanwhile) shows its short id.
 */
export function ProjectBadge({
  projectId,
  merchantLabel = "Merchant",
  className,
}: {
  projectId: string | null | undefined
  /** Label of a null owner: "Merchant" (endpoints, entries), "Merchant funds" (withdrawals)… */
  merchantLabel?: string
  className?: string
}) {
  const { projects } = useProjectScope()
  if (!projectId) {
    return (
      <span className={cn("inline-flex items-center gap-1.5 text-sm text-muted-foreground", className)}>
        <Landmark className="size-3.5" /> {merchantLabel}
      </span>
    )
  }
  const project = projects.find((p) => p.id === projectId)
  if (!project) {
    return (
      <span className={cn("inline-flex items-center gap-1.5 text-sm text-muted-foreground", className)}>
        <span aria-hidden className="inline-block size-2.5 rounded-full bg-muted-foreground/40" />
        <code className="text-xs">{projectId.slice(0, 8)}</code>
      </span>
    )
  }
  return (
    <span className={cn("inline-flex items-center gap-1.5 text-sm", className)} title={project.name}>
      <ProjectDot color={project.color} />
      <span className="truncate">{project.name}</span>
    </span>
  )
}
