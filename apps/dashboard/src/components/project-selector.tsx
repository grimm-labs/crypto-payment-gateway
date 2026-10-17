import { ChevronsUpDown, Layers, Server } from "lucide-react"

import { ProjectDot } from "@/components/shared/ProjectBadge"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu"
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip"
import { ALL_PROJECTS, usePageScope, useProjectScope } from "@/lib/project-scope"

/**
 * The global project scope, in the header: "All projects" or one project
 * on project-scoped pages; an "Instance" badge on the pages that show the
 * whole instance (spec §14.1).
 */
export function ProjectSelector() {
  const scope = usePageScope()
  const { projects, selected, selectedProject, setSelected } = useProjectScope()

  if (scope === "instance") {
    return (
      <Tooltip>
        <TooltipTrigger asChild>
          <Badge variant="outline" className="gap-1 font-normal">
            <Server className="size-3" /> Instance
          </Badge>
        </TooltipTrigger>
        <TooltipContent>This page shows the whole instance</TooltipContent>
      </Tooltip>
    )
  }

  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <Button variant="outline" size="sm" className="max-w-48 gap-1.5 sm:max-w-64">
          {selectedProject ? (
            <ProjectDot color={selectedProject.color} />
          ) : (
            <Layers className="size-3.5 text-muted-foreground" />
          )}
          <span className="truncate">{selectedProject?.name ?? "All projects"}</span>
          <ChevronsUpDown className="size-3.5 shrink-0 text-muted-foreground" />
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="start" className="w-64">
        <DropdownMenuLabel>Project</DropdownMenuLabel>
        <DropdownMenuItem onSelect={() => setSelected(ALL_PROJECTS)}>
          <Layers className="size-4" />
          All projects
          {selected === ALL_PROJECTS && <span className="ml-auto text-xs text-muted-foreground">current</span>}
        </DropdownMenuItem>
        <DropdownMenuSeparator />
        {projects.map((project) => (
          <DropdownMenuItem key={project.id} onSelect={() => setSelected(project.id)}>
            <ProjectDot color={project.color} />
            <span className="truncate">{project.name}</span>
            {selected === project.id && <span className="ml-auto text-xs text-muted-foreground">current</span>}
          </DropdownMenuItem>
        ))}
      </DropdownMenuContent>
    </DropdownMenu>
  )
}
