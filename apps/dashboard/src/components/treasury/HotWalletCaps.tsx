import { useState } from "react"
import { ArchiveRestore } from "lucide-react"

import { WithdrawalDialog, type WithdrawalPreset } from "@/components/withdrawals/WithdrawalDialog"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card"
import { useAddressBook } from "@/hooks/withdrawals"
import { useIsAdmin } from "@/lib/auth"
import { formatRaw, networkLabel } from "@/lib/format"
import type { TreasuryRow } from "@/lib/types"
import { capExcessRaw } from "@/lib/withdrawals"

/** Hot-wallet cap per asset, with a guided withdrawal of the excess to the
 *  first active cold-storage address-book entry of the network. */
export function HotWalletCaps({ rows }: { rows: TreasuryRow[] }) {
  const isAdmin = useIsAdmin()
  const book = useAddressBook()
  const [preset, setPreset] = useState<WithdrawalPreset | undefined>()
  const [open, setOpen] = useState(false)
  const capped = rows.filter((row) => row.hot_wallet_cap_raw !== null)

  function coldStorageEntry(network: string) {
    return book.data?.data.find(
      (entry) => entry.network === network && entry.is_cold_storage && entry.active,
    )
  }

  return (
    <Card>
      <CardHeader>
        <CardTitle>Hot-wallet caps</CardTitle>
        <CardDescription>
          Above its cap, a treasury raises an alert. “Send excess to cold storage”
          prepares a normal withdrawal of the excess (TOTP, approval above the
          threshold) — nothing moves automatically.
        </CardDescription>
      </CardHeader>
      <CardContent>
        {capped.length === 0 ? (
          <p className="text-sm text-muted-foreground">
            No hot-wallet cap configured — set them in Settings → Assets.
          </p>
        ) : (
          <div className="divide-y rounded-lg border">
            {capped.map((row) => {
              const excess = capExcessRaw(row)
              const entry = coldStorageEntry(row.network)
              return (
                <div
                  key={`${row.network}-${row.token}`}
                  className="flex flex-wrap items-center justify-between gap-3 p-3"
                >
                  <div className="space-y-0.5">
                    <p className="flex items-center gap-2 font-medium">
                      {networkLabel(row.network)} · {row.token}
                      {row.cap_exceeded ? (
                        <Badge variant="destructive">Cap exceeded</Badge>
                      ) : (
                        row.available && <Badge variant="outline">Within cap</Badge>
                      )}
                    </p>
                    <p className="text-sm tabular-nums text-muted-foreground">
                      {formatRaw(row.confirmed_raw, row.token)} held · cap{" "}
                      {formatRaw(row.hot_wallet_cap_raw, row.token)}
                      {excess !== null && (
                        <>
                          {" "}
                          · excess{" "}
                          <span className="font-medium text-foreground">
                            {formatRaw(excess, row.token)}
                          </span>
                        </>
                      )}
                    </p>
                    {excess !== null && !entry && (
                      <p className="text-xs text-amber-700 dark:text-amber-400">
                        No active cold-storage entry on {networkLabel(row.network)} — add one
                        in the Address book.
                      </p>
                    )}
                  </div>
                  {isAdmin && excess !== null && (
                    <Button
                      size="sm"
                      variant="outline"
                      disabled={!entry}
                      onClick={() => {
                        setPreset({
                          network: row.network,
                          token: row.token,
                          amountRaw: excess.toString(),
                          addressBookEntryId: entry?.id,
                        })
                        setOpen(true)
                      }}
                    >
                      <ArchiveRestore className="size-3.5" /> Send excess to cold storage
                    </Button>
                  )}
                </div>
              )
            })}
          </div>
        )}
      </CardContent>
      {isAdmin && (
        <WithdrawalDialog
          open={open}
          onOpenChange={setOpen}
          preset={preset}
          lockNetwork
          title="Send excess to cold storage"
        />
      )}
    </Card>
  )
}
