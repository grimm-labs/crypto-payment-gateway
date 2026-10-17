import { Link, useLocation } from "react-router-dom"

import { useIsAdmin } from "@/lib/auth"
import { cn } from "@/lib/utils"

const TABS = [
  { to: "/settings", label: "General", adminOnly: true },
  { to: "/settings/assets", label: "Assets", adminOnly: false },
  { to: "/projects", label: "Projects", adminOnly: false },
]

/** General (admin) | Assets | Projects (readable by every staff member). */
export function SettingsTabs() {
  const location = useLocation()
  const isAdmin = useIsAdmin()
  const tabs = TABS.filter((tab) => isAdmin || !tab.adminOnly)
  if (tabs.length < 2) return null

  return (
    <nav className="inline-flex h-8 items-center rounded-lg bg-muted p-[3px] text-muted-foreground">
      {tabs.map((tab) => {
        const active = location.pathname === tab.to
        return (
          <Link
            key={tab.to}
            to={tab.to}
            aria-current={active ? "page" : undefined}
            className={cn(
              "inline-flex h-full items-center rounded-md px-3 text-sm font-medium transition-colors",
              active ? "bg-background text-foreground shadow-sm" : "hover:text-foreground",
            )}
          >
            {tab.label}
          </Link>
        )
      })}
    </nav>
  )
}
