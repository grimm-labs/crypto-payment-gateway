/* eslint-disable react-refresh/only-export-components */
import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useState,
  type ReactNode,
} from "react"
import { Outlet, useLocation } from "react-router-dom"

import { useProjects } from "@/hooks/projects"
import { useAuth } from "@/lib/auth"
import type { Project, UnassignedCounts } from "@/lib/types"
import { FirstProjectStep, OperatorWaitingScreen } from "@/pages/FirstProjectPage"

/** Where the selection survives reloads (per browser). */
const STORAGE_KEY = "cpg.project"
export const ALL_PROJECTS = "all"

export type ProjectSelection = typeof ALL_PROJECTS | string

interface ProjectScopeValue {
  projects: Project[]
  /** Legacy rows while no project exists; null afterwards. */
  unassigned: UnassignedCounts | null
  /** True once the list has loaded at least once. */
  loaded: boolean
  error: unknown
  refetch: () => void
  /** `"all"` or a project id. */
  selected: ProjectSelection
  setSelected: (selection: ProjectSelection) => void
  /** The selected project, or undefined for "All projects". */
  selectedProject: Project | undefined
  /** `project_id` to send on project-scoped queries (undefined = all). */
  scopeParam: string | undefined
}

const ProjectScopeContext = createContext<ProjectScopeValue | null>(null)

function readStoredSelection(): ProjectSelection {
  try {
    return localStorage.getItem(STORAGE_KEY) || ALL_PROJECTS
  } catch {
    return ALL_PROJECTS
  }
}

function storeSelection(selection: ProjectSelection) {
  try {
    if (selection === ALL_PROJECTS) localStorage.removeItem(STORAGE_KEY)
    else localStorage.setItem(STORAGE_KEY, selection)
  } catch {
    // Storage blocked (privacy mode): the selection only lasts the session.
  }
}

/**
 * Holds the project list and the global selection ("All projects" or one
 * project) every project-scoped page filters on. An unknown stored id (a
 * deleted project) falls back to "all".
 */
export function ProjectScopeProvider({ children }: { children: ReactNode }) {
  const { staff } = useAuth()
  const projectsQuery = useProjects()
  const [stored, setStored] = useState<ProjectSelection>(readStoredSelection)

  const projects = useMemo(
    () => projectsQuery.data?.projects ?? [],
    [projectsQuery.data?.projects],
  )
  const loaded = projectsQuery.data !== undefined
  const selected: ProjectSelection =
    stored !== ALL_PROJECTS && loaded && !projects.some((p) => p.id === stored)
      ? ALL_PROJECTS
      : stored

  const setSelected = useCallback((selection: ProjectSelection) => {
    setStored(selection)
    storeSelection(selection)
  }, [])

  // Forget a stale id for good once the list says it is gone.
  useEffect(() => {
    if (loaded && stored !== ALL_PROJECTS && !projects.some((p) => p.id === stored)) {
      storeSelection(ALL_PROJECTS)
    }
  }, [loaded, projects, stored])

  const value = useMemo<ProjectScopeValue>(
    () => ({
      projects,
      unassigned: projectsQuery.data?.unassigned ?? null,
      loaded,
      error: projectsQuery.error,
      refetch: () => void projectsQuery.refetch(),
      selected,
      setSelected,
      selectedProject: projects.find((p) => p.id === selected),
      scopeParam: selected === ALL_PROJECTS ? undefined : selected,
    }),
    [projects, projectsQuery, loaded, selected, setSelected],
  )

  // Signed out: nothing to scope (the list query is disabled anyway).
  if (!staff) return <>{children}</>
  return <ProjectScopeContext.Provider value={value}>{children}</ProjectScopeContext.Provider>
}

const EMPTY_SCOPE: ProjectScopeValue = {
  projects: [],
  unassigned: null,
  loaded: false,
  error: null,
  refetch: () => {},
  selected: ALL_PROJECTS,
  setSelected: () => {},
  selectedProject: undefined,
  scopeParam: undefined,
}

export function useProjectScope(): ProjectScopeValue {
  return useContext(ProjectScopeContext) ?? EMPTY_SCOPE
}

/** Pages that show the whole instance: the header shows "Instance" instead
 *  of the project selector. Every other page is project-scoped. */
const INSTANCE_PATHS = [
  "/wallets",
  "/treasury",
  "/lightning",
  "/settings",
  "/staff",
  "/audit-log",
  "/account",
  "/projects",
]
// `/transfers` is project-scoped: the selected project on either side.

export type PageScope = "project" | "instance"

export function pageScope(pathname: string): PageScope {
  return INSTANCE_PATHS.some((prefix) => pathname === prefix || pathname.startsWith(`${prefix}/`))
    ? "instance"
    : "project"
}

export function usePageScope(): PageScope {
  return pageScope(useLocation().pathname)
}

/**
 * Blocks the app until a project exists (spec §13): admins get the
 * "Create your first project" step, operators a waiting screen.
 */
export function RequireProject() {
  const { staff } = useAuth()
  const { projects, unassigned, loaded, error, refetch, setSelected } = useProjectScope()

  if (!loaded) {
    if (error) {
      return (
        <OperatorWaitingScreen
          title="Could not load the projects"
          message={error instanceof Error ? error.message : "The API did not answer"}
          onRefresh={refetch}
        />
      )
    }
    return (
      <div className="flex min-h-svh items-center justify-center">
        <div className="size-6 animate-spin rounded-full border-2 border-muted-foreground border-t-transparent" />
      </div>
    )
  }
  if (projects.length === 0) {
    if (staff?.role === "admin") {
      return <FirstProjectStep unassigned={unassigned} onCreated={(id) => setSelected(id)} />
    }
    return (
      <OperatorWaitingScreen
        title="No project yet"
        message="An admin must create the first project before the dashboard can be used."
        onRefresh={refetch}
      />
    )
  }
  return <Outlet />
}
