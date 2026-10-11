import { z } from "zod"

/** Mirrors the API password policy (setup, staff create/update, change). */
export const PASSWORD_MIN_LENGTH = 12
export const PASSWORD_MAX_LENGTH = 128
const PASSWORD_MESSAGE = `Password must be between ${PASSWORD_MIN_LENGTH} and ${PASSWORD_MAX_LENGTH} characters`

export const passwordSchema = z
  .string()
  .min(PASSWORD_MIN_LENGTH, PASSWORD_MESSAGE)
  .max(PASSWORD_MAX_LENGTH, PASSWORD_MESSAGE)

/** Empty = unchanged; otherwise the same policy as `passwordSchema`. */
export const optionalPasswordSchema = z
  .string()
  .optional()
  .refine(
    (value) =>
      !value || (value.length >= PASSWORD_MIN_LENGTH && value.length <= PASSWORD_MAX_LENGTH),
    { message: PASSWORD_MESSAGE },
  )

const TOTP_CODE = /^\d{6}$/

/** A 6-digit authenticator code. */
export function isTotpCode(value: string | undefined): boolean {
  return TOTP_CODE.test((value ?? "").trim())
}

export const totpCodeSchema = z.string().trim().regex(TOTP_CODE, "Enter the 6-digit code")
