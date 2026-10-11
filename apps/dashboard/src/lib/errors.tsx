import { Link } from "react-router-dom"
import { toast } from "sonner"

import { ApiError } from "@/lib/api"

/** Route of the page where staff manage their password, 2FA and sessions. */
export const ACCOUNT_SECURITY_PATH = "/account/security"

export function errorMessage(error: unknown, fallback: string): string {
  return error instanceof Error && error.message ? error.message : fallback
}

/** 403 answered to an admin without 2FA on a step-up protected action. */
export function isTwoFactorRequired(error: unknown): boolean {
  return (
    error instanceof ApiError &&
    error.statusCode === 403 &&
    error.message.startsWith("Two-factor authentication must be enabled")
  )
}

/** Error toast; the "enable 2FA first" refusal links to Account security. */
export function toastError(error: unknown, fallback: string) {
  const message = errorMessage(error, fallback)
  if (isTwoFactorRequired(error)) {
    toast.error(message, {
      description: (
        <Link to={ACCOUNT_SECURITY_PATH} className="underline underline-offset-4">
          Set up two-factor authentication →
        </Link>
      ),
    })
    return
  }
  toast.error(message)
}
