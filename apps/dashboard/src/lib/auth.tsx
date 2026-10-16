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
import { useQueryClient } from "@tanstack/react-query"
import { RefreshCw } from "lucide-react"
import { Link, Navigate, Outlet, useLocation } from "react-router-dom"

import { Button } from "@/components/ui/button"
import {
  ApiError,
  api,
  clearLegacyRefreshToken,
  logoutAllSessions,
  logoutSession,
  refreshSession,
  setAccessToken,
  setOnSessionExpired,
} from "@/lib/api"
import type { AuthResponse, LoginResult, Staff } from "@/lib/types"

interface SetupPayload {
  email: string
  password: string
  first_name: string
  last_name: string
}

export interface ChangePasswordPayload {
  current_password: string
  new_password: string
  /** Required when the account has 2FA. */
  totp_code?: string
}

interface AuthContextValue {
  staff: Staff | null
  loading: boolean
  needsSetup: boolean
  /** Why the session could not be restored at startup (429, 5xx, network). */
  restoreError: string | null
  retryRestore: () => void
  /** Resolves to an MFA challenge when 2FA is required, null when signed in. */
  login: (email: string, password: string) => Promise<{ mfaToken: string } | null>
  loginTotp: (mfaToken: string, code: string) => Promise<void>
  completeSetup: (payload: SetupPayload) => Promise<void>
  /** Revokes every session and opens a fresh one for this tab. */
  changePassword: (payload: ChangePasswordPayload) => Promise<void>
  /** Refresh the signed-in profile after a self-update (e.g. 2FA on/off). */
  syncStaff: (staff: Staff) => void
  logout: () => Promise<void>
  /** Signs out every session of this account, this one included. */
  logoutAll: () => Promise<void>
}

const AuthContext = createContext<AuthContextValue | null>(null)

export function AuthProvider({ children }: { children: ReactNode }) {
  const queryClient = useQueryClient()
  const [staff, setStaff] = useState<Staff | null>(null)
  const [needsSetup, setNeedsSetup] = useState(false)
  const [loading, setLoading] = useState(true)
  const [restoreError, setRestoreError] = useState<string | null>(null)

  const applyAuth = useCallback((auth: AuthResponse) => {
    setAccessToken(auth.access_token)
    setStaff(auth.staff)
    setNeedsSetup(false)
    setRestoreError(null)
  }, [])

  const clearSession = useCallback(() => {
    setAccessToken(null)
    setStaff(null)
  }, [])

  // Drop cached data once signed out (after the pages have unmounted), so
  // the next account never sees the previous one's data.
  useEffect(() => {
    if (!staff) queryClient.clear()
  }, [staff, queryClient])

  useEffect(() => {
    setOnSessionExpired(clearSession)
    return () => setOnSessionExpired(null)
  }, [clearSession])

  // The session cookie is HttpOnly, so the only way to know whether one
  // exists is to try a refresh: 401 = signed out (login page, no error).
  const restore = useCallback(async (isCancelled: () => boolean = () => false) => {
    setLoading(true)
    setRestoreError(null)
    try {
      const setup = await api<{ needs_setup: boolean }>("/setup", { anonymous: true })
      if (isCancelled()) return
      setNeedsSetup(setup.needs_setup)
      if (!setup.needs_setup) {
        const session = await refreshSession()
        if (isCancelled()) return
        if (session) setStaff(session.staff)
      }
    } catch (error) {
      // A 429 / 5xx / network failure is not a logout: say so and offer a
      // retry instead of bouncing to the login page.
      if (!isCancelled()) {
        setRestoreError(
          error instanceof ApiError ? error.message : "Could not reach the API",
        )
      }
    } finally {
      if (!isCancelled()) setLoading(false)
    }
  }, [])

  useEffect(() => {
    clearLegacyRefreshToken()
    let cancelled = false
    void restore(() => cancelled)
    return () => {
      cancelled = true
    }
  }, [restore])

  const retryRestore = useCallback(() => {
    void restore()
  }, [restore])

  const login = useCallback(
    async (email: string, password: string) => {
      const result = await api<LoginResult>("/auth/login", {
        body: { email, password },
        anonymous: true,
      })
      if ("mfa_required" in result) {
        return { mfaToken: result.mfa_token }
      }
      applyAuth(result)
      return null
    },
    [applyAuth],
  )

  const loginTotp = useCallback(
    async (mfaToken: string, code: string) => {
      const auth = await api<AuthResponse>("/auth/login/totp", {
        body: { mfa_token: mfaToken, code },
        anonymous: true,
      })
      applyAuth(auth)
    },
    [applyAuth],
  )

  const completeSetup = useCallback(
    async (payload: SetupPayload) => {
      try {
        const auth = await api<AuthResponse>("/setup", { body: payload, anonymous: true })
        applyAuth(auth)
      } catch (error) {
        // Someone else finished the setup meanwhile — the login page applies.
        if (error instanceof ApiError && error.statusCode === 409) setNeedsSetup(false)
        throw error
      }
    },
    [applyAuth],
  )

  const changePassword = useCallback(
    async (payload: ChangePasswordPayload) => {
      const auth = await api<AuthResponse>("/auth/password", { body: payload })
      applyAuth(auth)
    },
    [applyAuth],
  )

  const syncStaff = useCallback((next: Staff) => {
    setStaff((current) => (current && current.id === next.id ? next : current))
  }, [])

  const logout = useCallback(async () => {
    await logoutSession()
    clearSession()
  }, [clearSession])

  const logoutAll = useCallback(async () => {
    await logoutAllSessions()
    clearSession()
  }, [clearSession])

  const value = useMemo(
    () => ({
      staff,
      loading,
      needsSetup,
      restoreError,
      retryRestore,
      login,
      loginTotp,
      completeSetup,
      changePassword,
      syncStaff,
      logout,
      logoutAll,
    }),
    [
      staff,
      loading,
      needsSetup,
      restoreError,
      retryRestore,
      login,
      loginTotp,
      completeSetup,
      changePassword,
      syncStaff,
      logout,
      logoutAll,
    ],
  )

  return <AuthContext.Provider value={value}>{children}</AuthContext.Provider>
}

export function useAuth(): AuthContextValue {
  const context = useContext(AuthContext)
  if (!context) throw new Error("useAuth must be used within AuthProvider")
  return context
}

export function useIsAdmin(): boolean {
  return useAuth().staff?.role === "admin"
}

export function RequireAuth() {
  const { staff, loading, needsSetup, restoreError, retryRestore } = useAuth()
  const location = useLocation()

  if (loading) return <FullPageSpinner />
  if (restoreError && !staff) {
    return <RestoreErrorPanel message={restoreError} onRetry={retryRestore} />
  }
  if (needsSetup) return <Navigate to="/setup" replace />
  if (!staff) return <Navigate to="/login" replace state={{ from: location }} />
  return <Outlet />
}

export function RequireAdmin() {
  const { staff } = useAuth()
  if (staff?.role !== "admin") {
    return (
      <div className="flex h-full min-h-64 flex-col items-center justify-center gap-2 p-8 text-center">
        <p className="text-lg font-semibold">Access denied</p>
        <p className="text-sm text-muted-foreground">
          This section requires the admin role.
        </p>
      </div>
    )
  }
  return <Outlet />
}

function FullPageSpinner() {
  return (
    <div className="flex min-h-svh items-center justify-center">
      <div className="size-6 animate-spin rounded-full border-2 border-muted-foreground border-t-transparent" />
    </div>
  )
}

function RestoreErrorPanel({ message, onRetry }: { message: string; onRetry: () => void }) {
  return (
    <div className="flex min-h-svh flex-col items-center justify-center gap-3 bg-muted/40 p-6 text-center">
      <p className="text-lg font-semibold">Could not restore your session</p>
      <p className="max-w-md text-sm text-muted-foreground">{message}</p>
      <div className="flex gap-2">
        <Button onClick={onRetry}>
          <RefreshCw className="size-4" /> Try again
        </Button>
        <Button asChild variant="outline">
          <Link to="/login">Sign in</Link>
        </Button>
      </div>
    </div>
  )
}
