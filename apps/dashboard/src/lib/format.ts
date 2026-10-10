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

/** Base-unit exponent per token (1 BTC = 100 000 000 sats). */
export const TOKEN_DECIMALS: Record<string, number> = { USDT: 6, BTC: 8 }

/**
 * Amounts arrive as decimal strings — never parsed to float. BTC is shown at
 * its full 8 decimals; other tokens as-is.
 */
export function formatAmount(amount: string, token = "USDT"): string {
  const value = token === "BTC" ? padDecimals(amount, TOKEN_DECIMALS.BTC) : amount
  return `${value} ${token}`
}

/** `"0.0015"` → `"0.00150000"` (string math; longer fractions are kept). */
export function padDecimals(amount: string, decimals: number): string {
  const [whole, fraction = ""] = amount.split(".")
  if (fraction.length >= decimals) return amount
  return `${whole}.${fraction.padEnd(decimals, "0")}`
}

/**
 * Integer sats string → trimmed BTC decimal string, without floats:
 * `"150000"` → `"0.0015"`, `"100000000"` → `"1"`. `null` if not digits.
 */
export function satsToBtc(sats: string): string | null {
  const digits = sats.trim()
  if (!/^\d+$/.test(digits)) return null
  const padded = digits.padStart(TOKEN_DECIMALS.BTC + 1, "0")
  const whole = padded.slice(0, -TOKEN_DECIMALS.BTC).replace(/^0+(?=\d)/, "")
  const fraction = padded.slice(-TOKEN_DECIMALS.BTC).replace(/0+$/, "")
  return fraction ? `${whole}.${fraction}` : whole
}

export function shortHash(value: string, keep = 8): string {
  if (value.length <= keep * 2 + 1) return value
  return `${value.slice(0, keep)}…${value.slice(-keep)}`
}

export function initials(firstName: string, lastName: string): string {
  return `${firstName.charAt(0)}${lastName.charAt(0)}`.toUpperCase()
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

/** Bitcoin and Lightning take BTC amounts expressed in sats. */
export function isBtcNetwork(network: string): boolean {
  return network === "bitcoin" || network === "lightning"
}

/** The token a network receives. */
export function networkToken(network: string): "USDT" | "BTC" {
  return isBtcNetwork(network) ? "BTC" : "USDT"
}
