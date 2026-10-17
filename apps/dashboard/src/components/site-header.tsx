import { useLocation } from "react-router-dom"

import { HeaderFreezeControl } from "@/components/outflows/OutflowControls"
import { ProjectSelector } from "@/components/project-selector"
import { Separator } from "@/components/ui/separator"
import { SidebarTrigger } from "@/components/ui/sidebar"

const TITLES: Array<[string, string]> = [
  ["/payments", "Payments"],
  ["/projects", "Projects"],
  ["/deliveries", "Deliveries"],
  ["/traffic", "Traffic"],
  ["/webhooks", "Webhooks"],
  ["/wallets", "Wallets"],
  ["/withdrawals", "Withdrawals"],
  ["/treasury", "Treasury"],
  ["/address-book", "Address book"],
  ["/lightning", "Lightning"],
  ["/api-keys", "API Keys"],
  ["/staff", "Staff"],
  ["/settings/assets", "Settings · Assets"],
  ["/settings", "Settings"],
  ["/audit-log", "Audit log"],
  ["/account", "Account security"],
]

export function SiteHeader() {
  const location = useLocation()
  const title =
    TITLES.find(([prefix]) => location.pathname.startsWith(prefix))?.[1] ?? "Overview"

  return (
    <header className="flex h-(--header-height) shrink-0 items-center gap-2 border-b transition-[width,height] ease-linear group-has-data-[collapsible=icon]/sidebar-wrapper:h-(--header-height)">
      <div className="flex w-full items-center gap-1 px-4 lg:gap-2 lg:px-6">
        <SidebarTrigger className="-ml-1" />
        <Separator
          orientation="vertical"
          className="mx-2 data-[orientation=vertical]:h-4"
        />
        {/* Global project scope (or "Instance" on instance-wide pages). */}
        <ProjectSelector />
        <Separator
          orientation="vertical"
          className="mx-2 hidden data-[orientation=vertical]:h-4 sm:block"
        />
        <h1 className="hidden text-base font-medium sm:block">{title}</h1>
        {/* Emergency stop, one click away on every page (any staff member). */}
        <div className="ml-auto">
          <HeaderFreezeControl />
        </div>
      </div>
    </header>
  )
}
