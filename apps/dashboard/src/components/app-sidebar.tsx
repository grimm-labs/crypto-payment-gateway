import * as React from "react"
import {
  ActivityIcon,
  CoinsIcon,
  KeyRoundIcon,
  LayoutDashboardIcon,
  ReceiptTextIcon,
  ScrollTextIcon,
  SendIcon,
  SettingsIcon,
  ShieldCheckIcon,
  UsersIcon,
  WalletIcon,
  WebhookIcon,
  ZapIcon,
} from "lucide-react"
import { Link, useLocation } from "react-router-dom"

import { NavMain, type NavItem } from "@/components/nav-main"
import { NavUser } from "@/components/nav-user"
import {
  Sidebar,
  SidebarContent,
  SidebarFooter,
  SidebarGroup,
  SidebarGroupContent,
  SidebarGroupLabel,
  SidebarHeader,
  SidebarMenu,
  SidebarMenuButton,
  SidebarMenuItem,
} from "@/components/ui/sidebar"
import { useAuth } from "@/lib/auth"
import { ACCOUNT_SECURITY_PATH } from "@/lib/errors"

const NAV_MAIN: Array<NavItem & { adminOnly?: boolean }> = [
  { title: "Overview", url: "/", icon: <LayoutDashboardIcon /> },
  { title: "Payments", url: "/payments", icon: <ReceiptTextIcon /> },
  { title: "Deliveries", url: "/deliveries", icon: <SendIcon /> },
  { title: "Traffic", url: "/traffic", icon: <ActivityIcon /> },
  { title: "Wallets", url: "/wallets", icon: <WalletIcon /> },
  // The wallet history is admin-only on the API.
  { title: "Lightning", url: "/lightning", icon: <ZapIcon />, adminOnly: true },
]

const NAV_ADMIN: NavItem[] = [
  { title: "API Keys", url: "/api-keys", icon: <KeyRoundIcon /> },
  { title: "Webhooks", url: "/webhooks", icon: <WebhookIcon /> },
  { title: "Staff", url: "/staff", icon: <UsersIcon /> },
  { title: "Settings", url: "/settings", icon: <SettingsIcon /> },
  { title: "Audit log", url: "/audit-log", icon: <ScrollTextIcon /> },
]

// Visible to every role — each staff member manages their own account here.
const NAV_ACCOUNT: NavItem[] = [
  { title: "Security", url: ACCOUNT_SECURITY_PATH, icon: <ShieldCheckIcon /> },
]

export function AppSidebar({ ...props }: React.ComponentProps<typeof Sidebar>) {
  const { staff } = useAuth()
  const location = useLocation()
  const isAdmin = staff?.role === "admin"
  const mainItems = NAV_MAIN.filter((item) => isAdmin || !item.adminOnly)

  return (
    <Sidebar collapsible="offcanvas" {...props}>
      <SidebarHeader>
        <SidebarMenu>
          <SidebarMenuItem>
            <SidebarMenuButton
              asChild
              className="data-[slot=sidebar-menu-button]:p-1.5!"
            >
              <Link to="/">
                <CoinsIcon className="size-5!" />
                <span className="text-base font-semibold">Payment Gateway</span>
              </Link>
            </SidebarMenuButton>
          </SidebarMenuItem>
        </SidebarMenu>
      </SidebarHeader>
      <SidebarContent>
        <NavMain items={mainItems} />
        {isAdmin && (
          <SidebarGroup>
            <SidebarGroupLabel>Administration</SidebarGroupLabel>
            <SidebarGroupContent>
              <SidebarMenu>
                {NAV_ADMIN.map((item) => (
                  <SidebarMenuItem key={item.title}>
                    <SidebarMenuButton
                      asChild
                      tooltip={item.title}
                      isActive={location.pathname.startsWith(item.url)}
                    >
                      <Link to={item.url}>
                        {item.icon}
                        <span>{item.title}</span>
                      </Link>
                    </SidebarMenuButton>
                  </SidebarMenuItem>
                ))}
              </SidebarMenu>
            </SidebarGroupContent>
          </SidebarGroup>
        )}
        <SidebarGroup>
          <SidebarGroupLabel>Account</SidebarGroupLabel>
          <SidebarGroupContent>
            <SidebarMenu>
              {NAV_ACCOUNT.map((item) => (
                <SidebarMenuItem key={item.title}>
                  <SidebarMenuButton
                    asChild
                    tooltip={item.title}
                    isActive={location.pathname.startsWith(item.url)}
                  >
                    <Link to={item.url}>
                      {item.icon}
                      <span>{item.title}</span>
                    </Link>
                  </SidebarMenuButton>
                </SidebarMenuItem>
              ))}
            </SidebarMenu>
          </SidebarGroupContent>
        </SidebarGroup>
      </SidebarContent>
      <SidebarFooter>
        <NavUser />
      </SidebarFooter>
    </Sidebar>
  )
}
