import type { KeyWithdrawalToken, Network, Token, ChainMode } from "@/lib/types"

/** Mirrors `domain::Network` — the order every selector uses. */
export const NETWORKS: Network[] = ["tron", "ethereum", "solana", "bitcoin", "lightning"]

/** Tokens a network accepts as payments (deposit addresses). */
export const PAYMENT_TOKENS: Record<Network, Token[]> = {
  tron: ["USDT"],
  ethereum: ["USDT", "ETH"],
  solana: ["USDT"],
  bitcoin: ["BTC"],
  lightning: ["BTC"],
}

/** Tokens that can leave the gateway on a network: payment tokens + gas asset. */
export const WITHDRAWABLE_TOKENS: Record<Network, Token[]> = {
  tron: ["USDT", "TRX"],
  ethereum: ["USDT", "ETH"],
  solana: ["USDT", "SOL"],
  bitcoin: ["BTC"],
  lightning: ["BTC"],
}

/** Tokens a merchant key may be granted (TRX and SOL are admin-only gas assets). */
export const KEY_WITHDRAWAL_TOKENS: KeyWithdrawalToken[] = ["USDT", "BTC", "ETH"]

/** Gas assets: never payments, withdrawable by admins only. */
export function isGasOnlyToken(token: string): boolean {
  return token === "TRX" || token === "SOL"
}

/** Bitcoin and Lightning take BTC amounts expressed in sats. */
export function isBtcNetwork(network: string): boolean {
  return network === "bitcoin" || network === "lightning"
}

export function isNetwork(value: string): value is Network {
  return (NETWORKS as string[]).includes(value)
}

/** Networks a set of tokens can be withdrawn on. */
export function networksForTokens(tokens: readonly string[]): Network[] {
  return NETWORKS.filter((network) =>
    WITHDRAWABLE_TOKENS[network].some((token) => tokens.includes(token)),
  )
}

const EXPLORER_TX: Record<ChainMode, Partial<Record<Network, (hash: string) => string>>> = {
  mainnet: {
    ethereum: (hash) => `https://etherscan.io/tx/${hash}`,
    tron: (hash) => `https://tronscan.org/#/transaction/${hash}`,
    solana: (hash) => `https://solscan.io/tx/${hash}`,
    bitcoin: (hash) => `https://mempool.space/tx/${hash}`,
  },
  // CHAIN_MODE=testnet: Sepolia, Nile, devnet, testnet4 (config `ChainParams`).
  testnet: {
    ethereum: (hash) => `https://sepolia.etherscan.io/tx/${hash}`,
    tron: (hash) => `https://nile.tronscan.org/#/transaction/${hash}`,
    solana: (hash) => `https://solscan.io/tx/${hash}?cluster=devnet`,
    bitcoin: (hash) => `https://mempool.space/testnet4/tx/${hash}`,
  },
}

/** Block-explorer page of a transaction on the instance's chains; null on
 *  lightning (a payment id). */
export function explorerTxUrl(network: string, hash: string, mode: ChainMode = "mainnet"): string | null {
  const build = EXPLORER_TX[mode][network as Network]
  return build ? build(encodeURIComponent(hash)) : null
}

/** Stable key of a (network, token) pair — Ethereum appears once per token. */
export function assetKey(item: { network: string; token: string }): string {
  return `${item.network}-${item.token}`
}
