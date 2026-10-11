import { KeyRound, UserRound } from "lucide-react"

import { useApiKeys, useStaffList } from "@/hooks/queries"
import { useIsAdmin } from "@/lib/auth"
import { shortHash } from "@/lib/format"
import type { Requester } from "@/lib/types"

/** Who requested (or approved) a withdrawal. Staff and API-key names are
 *  admin-only on the API: operators see short ids. */
export function RequesterLabel({ requester }: { requester: Requester | null }) {
  if (!requester) return <span className="text-xs text-muted-foreground">—</span>
  return requester.type === "staff" ? (
    <StaffName id={requester.id} />
  ) : (
    <ApiKeyName id={requester.id} />
  )
}

export function StaffName({ id }: { id: string | null }) {
  const isAdmin = useIsAdmin()
  const staff = useStaffList(isAdmin)
  if (!id) return <span className="text-xs text-muted-foreground">—</span>
  const member = isAdmin ? staff.data?.data.find((s) => s.id === id) : undefined
  return (
    <span className="inline-flex items-center gap-1 text-sm" title={id}>
      <UserRound className="size-3.5 text-muted-foreground" />
      {member ? `${member.first_name} ${member.last_name}` : `Staff ${shortHash(id, 4)}`}
    </span>
  )
}

function ApiKeyName({ id }: { id: string }) {
  const isAdmin = useIsAdmin()
  const keys = useApiKeys(isAdmin)
  const key = isAdmin ? keys.data?.data.find((k) => k.id === id) : undefined
  return (
    <span className="inline-flex items-center gap-1 text-sm" title={id}>
      <KeyRound className="size-3.5 text-muted-foreground" />
      {key ? key.name : `API key ${shortHash(id, 4)}`}
    </span>
  )
}
