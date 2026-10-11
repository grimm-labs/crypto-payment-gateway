export function formatDate(iso: string | null | undefined): string {
  if (!iso) return "—"
  const date = new Date(iso)
  if (Number.isNaN(date.getTime())) return "—"
  return date.toLocaleString(undefined, {
    year: "numeric",
    month: "short",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  })
}

/** Base-unit exponent per token (1 BTC = 10^8 sats, 1 ETH = 10^18 wei,
 *  1 TRX = 10^6 sun, 1 SOL = 10^9 lamports). */
export const TOKEN_DECIMALS: Record<string, number> = {
  USDT: 6,
  BTC: 8,
  ETH: 18,
  TRX: 6,
  SOL: 9,
}

/** Decimals of `token`; unknown tokens are treated like USDT. */
export function tokenDecimals(token: string): number {
  return TOKEN_DECIMALS[token] ?? TOKEN_DECIMALS.USDT
}

/**
 * Amounts arrive as decimal strings — never parsed to float. BTC is shown at
 * its full 8 decimals; other tokens as-is.
 */
export function formatAmount(amount: string, token: string): string {
  const value = token === "BTC" ? padDecimals(amount, TOKEN_DECIMALS.BTC) : amount
  return `${value} ${token}`
}

/** `"0.0015"` → `"0.00150000"` (string math; longer fractions are kept). */
export function padDecimals(amount: string, decimals: number): string {
  const [whole, fraction = ""] = amount.split(".")
  if (fraction.length >= decimals) return amount
  return `${whole}.${fraction.padEnd(decimals, "0")}`
}

/** Integer string (or bigint) → bigint, `null` if it is not a whole number. */
export function toBigInt(raw: string | bigint | null | undefined): bigint | null {
  if (typeof raw === "bigint") return raw
  if (raw === null || raw === undefined) return null
  const digits = raw.trim()
  return /^-?\d+$/.test(digits) ? BigInt(digits) : null
}

/**
 * Base units → trimmed decimal string, with BigInt only:
 * `rawToDecimal("150000", "BTC")` → `"0.0015"`, `("10", "USDT")` → `"0.00001"`.
 */
export function rawToDecimal(raw: string | bigint, token: string): string {
  const value = toBigInt(raw)
  if (value === null) return String(raw)
  const negative = value < 0n
  const digits = (negative ? -value : value).toString()
  const decimals = tokenDecimals(token)
  const padded = digits.padStart(decimals + 1, "0")
  const whole = padded.slice(0, -decimals)
  const fraction = padded.slice(-decimals).replace(/0+$/, "")
  return `${negative ? "-" : ""}${whole}${fraction ? `.${fraction}` : ""}`
}

/** Base units → `"1.5 ETH"` (BTC padded to 8 decimals). `"—"` when null. */
export function formatRaw(raw: string | bigint | null | undefined, token: string): string {
  if (raw === null || raw === undefined) return "—"
  return formatAmount(rawToDecimal(raw, token), token)
}

/**
 * Strict decimal → base units: plain digits with at most the token's
 * decimals, never rounded. `null` when malformed (or too precise).
 */
export function decimalToRaw(value: string, token: string): bigint | null {
  const trimmed = value.trim()
  const decimals = tokenDecimals(token)
  const match = /^(\d+)(?:\.(\d*))?$/.exec(trimmed)
  if (!match) return null
  const [, whole, fraction = ""] = match
  if (fraction.length > decimals) return null
  return BigInt(whole + fraction.padEnd(decimals, "0"))
}

/** A decimal token amount the API accepts: > 0 (or ≥ 0 with `allowZero`). */
export function isValidDecimalAmount(
  value: string,
  token: string,
  { allowZero = false }: { allowZero?: boolean } = {},
): boolean {
  const raw = decimalToRaw(value, token)
  return raw !== null && (allowZero ? raw >= 0n : raw > 0n)
}

/**
 * Integer sats string → trimmed BTC decimal string, without floats:
 * `"150000"` → `"0.0015"`, `"100000000"` → `"1"`. `null` if not digits.
 */
export function satsToBtc(sats: string): string | null {
  const digits = sats.trim()
  if (!/^\d+$/.test(digits)) return null
  return rawToDecimal(digits, "BTC")
}

/** Smaller of two base-unit amounts. */
export function minBigInt(a: bigint, b: bigint): bigint {
  return a < b ? a : b
}

export function shortHash(value: string, keep = 8): string {
  if (value.length <= keep * 2 + 1) return value
  return `${value.slice(0, keep)}…${value.slice(-keep)}`
}

export function initials(firstName: string, lastName: string): string {
  return `${firstName.charAt(0)}${lastName.charAt(0)}`.toUpperCase()
}

/** Remaining time until `target`, e.g. `"23 h 12 min"`, `"45 s"`; `null` once passed. */
export function formatCountdown(target: string, now: number): string | null {
  const remaining = new Date(target).getTime() - now
  if (Number.isNaN(remaining) || remaining <= 0) return null
  const totalSeconds = Math.ceil(remaining / 1000)
  const hours = Math.floor(totalSeconds / 3600)
  const minutes = Math.floor((totalSeconds % 3600) / 60)
  if (hours > 0) return `${hours} h ${minutes} min`
  if (minutes > 0) return `${minutes} min`
  return `${totalSeconds} s`
}

/** `"withdrawal_pending_approval"` → `"withdrawal pending approval"`. */
export function humanize(value: string): string {
  return value.replace(/_/g, " ")
}

export const NETWORK_LABELS: Record<string, string> = {
  tron: "Tron",
  ethereum: "Ethereum",
  solana: "Solana",
  bitcoin: "Bitcoin",
  lightning: "Lightning",
}

export function networkLabel(network: string): string {
  return NETWORK_LABELS[network] ?? network
}
