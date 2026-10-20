import { ExternalLink } from "lucide-react"

import { CopyButton } from "@/components/shared/CopyButton"
import { useOutflows } from "@/hooks/withdrawals"
import { shortHash } from "@/lib/format"
import { explorerTxUrl } from "@/lib/networks"

/** Short transaction hash with copy and block-explorer link (none on
 *  lightning, where the hash is a wallet payment id). */
export function TxHash({
  network,
  hash,
  keep = 8,
}: {
  network: string
  hash: string | null | undefined
  keep?: number
}) {
  // The explorers follow the instance's chains (mainnet or the testnets).
  const outflows = useOutflows()
  if (!hash) return <span className="text-xs text-muted-foreground">—</span>
  const url = explorerTxUrl(network, hash, outflows.data?.chain_mode ?? "mainnet")
  return (
    <div className="flex items-center gap-1">
      {url ? (
        <a
          href={url}
          target="_blank"
          rel="noopener noreferrer"
          className="inline-flex items-center gap-1 font-mono text-xs underline-offset-4 hover:underline"
          title={hash}
        >
          {shortHash(hash, keep)}
          <ExternalLink className="size-3" />
        </a>
      ) : (
        <code className="text-xs" title={hash}>
          {shortHash(hash, keep)}
        </code>
      )}
      <CopyButton value={hash} />
    </div>
  )
}
