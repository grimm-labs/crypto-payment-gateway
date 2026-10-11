import type { AuthResponse } from "@/lib/types"

const ADMIN_BASE = "/api/admin/v1"

/** Where the refresh token used to live before it moved to an HttpOnly
 *  cookie — only ever read to delete it. */
const LEGACY_REFRESH_TOKEN_KEY = "spg.refresh_token"

/** Required by the cookie-authenticated endpoints (refresh, logout); sent on
 *  every request since it is harmless elsewhere. A cross-site form cannot set
 *  a custom header, so this blocks CSRF on top of `SameSite=Strict`. */
const CSRF_HEADERS = { "X-CSRF-Protection": "1" }

/** Two tabs can race on a refresh-token rotation: the loser gets a 401 but
 *  the browser receives the winner's cookie shortly after. */
const REFRESH_RETRY_DELAY_MS = 400

// The access token only lives in memory. The session itself is the
// `cpg_refresh` HttpOnly cookie, which JavaScript never sees.
let accessToken: string | null = null

// Bumped on logout: a refresh that was already in flight must not resurrect
// the access token once the user has signed out.
let sessionEpoch = 0

export function setAccessToken(token: string | null) {
  accessToken = token
}

/** One-time cleanup of the refresh token older dashboards kept in storage. */
export function clearLegacyRefreshToken() {
  try {
    localStorage.removeItem(LEGACY_REFRESH_TOKEN_KEY)
  } catch {
    // Storage blocked (privacy mode) — nothing to clean up.
  }
}

export class ApiError extends Error {
  statusCode: number
  /** Seconds, from the `Retry-After` header of a 429. */
  retryAfter: number | null

  constructor(statusCode: number, message: string, retryAfter: number | null = null) {
    super(message)
    this.statusCode = statusCode
    this.retryAfter = retryAfter
  }
}

// Called when a 401 survives a refresh attempt — the auth provider registers
// its logout handler here.
let onSessionExpired: (() => void) | null = null
export function setOnSessionExpired(handler: (() => void) | null) {
  onSessionExpired = handler
}

interface RequestOptions {
  method?: string
  body?: unknown
  /** Skip the Authorization header and the 401-refresh dance (login, setup). */
  anonymous?: boolean
  /** Extra request headers (e.g. Idempotency-Key). */
  headers?: Record<string, string>
}

export async function api<T>(path: string, options: RequestOptions = {}): Promise<T> {
  return (await apiResponse<T>(path, options)).data
}

/** Like `api`, but also exposes the HTTP status (e.g. 200 replay vs 202). */
export async function apiResponse<T>(
  path: string,
  options: RequestOptions = {},
): Promise<{ status: number; data: T }> {
  const response = await send(path, options)

  // Only a 401 means "session gone". A 429 (rate limit) or 403 (role, 2FA)
  // is surfaced as-is and never signs the user out.
  if (response.status === 401 && !options.anonymous) {
    // Throws on a 429 / network error during the refresh: the caller sees
    // that error and the session is kept.
    const refreshed = await refreshSession()
    if (refreshed) {
      const retried = await send(path, options)
      if (retried.status === 401) onSessionExpired?.()
      return handleResponse<T>(retried)
    }
    onSessionExpired?.()
  }

  return handleResponse<T>(response)
}

function send(path: string, options: RequestOptions): Promise<Response> {
  const headers: Record<string, string> = { ...CSRF_HEADERS, ...options.headers }
  if (options.body !== undefined) {
    headers["Content-Type"] = "application/json"
  }
  if (!options.anonymous && accessToken) {
    headers["Authorization"] = `Bearer ${accessToken}`
  }
  return fetch(`${ADMIN_BASE}${path}`, {
    method: options.method ?? (options.body !== undefined ? "POST" : "GET"),
    headers,
    body: options.body !== undefined ? JSON.stringify(options.body) : undefined,
    // Same-origin is the default; explicit because the session cookie
    // must ride along on login / setup / refresh / logout.
    credentials: "same-origin",
  })
}

async function handleResponse<T>(response: Response): Promise<{ status: number; data: T }> {
  if (response.ok) {
    return { status: response.status, data: (await response.json()) as T }
  }
  throw await toApiError(response)
}

async function toApiError(response: Response): Promise<ApiError> {
  let message = `HTTP ${response.status}`
  try {
    const body = (await response.json()) as { message?: string }
    if (body.message) message = body.message
  } catch {
    // non-JSON error body — keep the generic message
  }
  const retryAfter = Number(response.headers.get("Retry-After"))
  return new ApiError(
    response.status,
    message,
    Number.isFinite(retryAfter) && retryAfter > 0 ? retryAfter : null,
  )
}

// Single-flight refresh: concurrent 401s (and the startup bootstrap) share
// one refresh call — each call rotates the cookie, so parallel ones would
// invalidate each other.
let refreshPromise: Promise<AuthResponse | null> | null = null

/**
 * Exchange the session cookie for a fresh access token.
 * Resolves to the new session, or `null` when there is no valid session
 * (401 twice). Throws an `ApiError` on anything else (429, 5xx) and a
 * `TypeError` on network failure — those must not end the session.
 */
export function refreshSession(): Promise<AuthResponse | null> {
  refreshPromise ??= doRefresh().finally(() => {
    refreshPromise = null
  })
  return refreshPromise
}

async function doRefresh(): Promise<AuthResponse | null> {
  const epoch = sessionEpoch
  let response = await postRefresh()
  if (response.status === 401) {
    await new Promise((resolve) => setTimeout(resolve, REFRESH_RETRY_DELAY_MS))
    if (epoch !== sessionEpoch) return null
    response = await postRefresh()
  }
  if (epoch !== sessionEpoch) return null

  if (response.status === 401) {
    setAccessToken(null)
    return null
  }
  if (!response.ok) throw await toApiError(response)

  const auth = (await response.json()) as AuthResponse
  if (epoch !== sessionEpoch) return null
  setAccessToken(auth.access_token)
  return auth
}

function postRefresh(): Promise<Response> {
  return fetch(`${ADMIN_BASE}/auth/refresh`, {
    method: "POST",
    headers: CSRF_HEADERS,
    credentials: "same-origin",
  })
}

/**
 * End the current session server-side. Public endpoint authenticated by the
 * cookie: called directly — never through the 401 → refresh → retry dance,
 * which used to rotate the session and then log out a stale one (#30).
 * Best effort: the local state is cleared by the caller regardless.
 */
export async function logoutSession(): Promise<void> {
  sessionEpoch += 1
  // Let a refresh already in flight land first, so the cookie this call
  // carries (and the server kills) is the latest one.
  await refreshPromise?.catch(() => null)
  setAccessToken(null)
  try {
    await fetch(`${ADMIN_BASE}/auth/logout`, {
      method: "POST",
      headers: CSRF_HEADERS,
      credentials: "same-origin",
    })
  } catch {
    // Network error — nothing more the client can do.
  }
}

/** Revoke every session of the current staff member, this one included. */
export async function logoutAllSessions(): Promise<void> {
  await api<{ status: string }>("/auth/logout-all", { method: "POST" })
  sessionEpoch += 1
  setAccessToken(null)
}
