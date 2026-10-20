import { useMutation, useQueries, useQuery, useQueryClient } from "@tanstack/react-query"

import { api } from "@/lib/api"
import { useAuth } from "@/lib/auth"
import type { LedgerPair, Project, ProjectBalances, ProjectColor, ProjectsResponse } from "@/lib/types"

export const PROJECTS_QUERY_KEY = ["projects"] as const

/** Every project (any staff member). Only fetched once signed in. */
export function useProjects() {
  const { staff } = useAuth()
  return useQuery({
    queryKey: PROJECTS_QUERY_KEY,
    queryFn: () => api<ProjectsResponse>("/projects"),
    enabled: staff !== null,
    staleTime: 30_000,
  })
}

export interface CreateProjectPayload {
  name: string
  /** Absent = the first palette colour no project uses yet. */
  color?: ProjectColor
  key_tag?: string
  /** First project only: attach the legacy rows (default) or release them. */
  attach_existing?: boolean
}

export function useCreateProject() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (payload: CreateProjectPayload) => api<Project>("/projects", { body: payload }),
    onSuccess: (created) => {
      // The list knows the new project at once: selecting it right after
      // creation must not read as a stale id (which the scope forgets).
      queryClient.setQueryData<ProjectsResponse>(PROJECTS_QUERY_KEY, (current) =>
        current && !current.projects.some((p) => p.id === created.id)
          ? { ...current, projects: [...current.projects, created], unassigned: null }
          : current,
      )
      queryClient.invalidateQueries({ queryKey: PROJECTS_QUERY_KEY })
      // The first project may have attached keys, payments, endpoints…
      queryClient.invalidateQueries({ queryKey: ["api-keys"] })
      queryClient.invalidateQueries({ queryKey: ["payments"] })
      queryClient.invalidateQueries({ queryKey: ["webhook-endpoints"] })
      queryClient.invalidateQueries({ queryKey: ["address-book"] })
      queryClient.invalidateQueries({ queryKey: ["withdrawals"] })
    },
  })
}

export interface UpdateProjectPayload {
  id: string
  name?: string
  color?: ProjectColor
  /** null clears the tag. */
  key_tag?: string | null
}

export function useUpdateProject() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ id, ...body }: UpdateProjectPayload) =>
      api<Project>(`/projects/${id}`, { method: "PATCH", body }),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: PROJECTS_QUERY_KEY }),
  })
}

export function useDeleteProject() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (id: string) =>
      api<{ status: string; id: string }>(`/projects/${id}`, { method: "DELETE" }),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: PROJECTS_QUERY_KEY }),
  })
}

/** One project's balances, one entry per payment (network, token). */
export function useProjectBalance(projectId: string | undefined) {
  return useQuery({
    queryKey: ["project-balance", projectId],
    queryFn: () => api<ProjectBalances>(`/projects/${projectId}/balance`),
    enabled: Boolean(projectId),
    refetchInterval: 30_000,
  })
}

/** The balances of several projects at once (the "All projects" overview). */
export function useProjectBalancesOf(projectIds: string[]) {
  return useQueries({
    queries: projectIds.map((id) => ({
      queryKey: ["project-balance", id],
      queryFn: () => api<ProjectBalances>(`/projects/${id}/balance`),
      refetchInterval: 30_000,
    })),
  })
}

/** The reconciliation of every payment pair (any staff member). */
export function useLedger() {
  return useQuery({
    queryKey: ["ledger"],
    queryFn: () => api<{ data: LedgerPair[] }>("/ledger"),
    refetchInterval: 60_000,
  })
}
