import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query"

import { api } from "@/lib/api"
import { useAuth } from "@/lib/auth"
import type { Project, ProjectColor, ProjectsResponse } from "@/lib/types"

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
    onSuccess: () => {
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
