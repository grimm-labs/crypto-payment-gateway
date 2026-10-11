import { Navigate, Route, Routes } from "react-router-dom"

import { AppLayout } from "@/components/layout/AppLayout"
import { RequireAdmin, RequireAuth } from "@/lib/auth"
import { AccountSecurityPage } from "@/pages/AccountSecurityPage"
import { ApiKeysPage } from "@/pages/ApiKeysPage"
import { AuditLogPage } from "@/pages/AuditLogPage"
import { DeliveriesPage } from "@/pages/DeliveriesPage"
import { LightningPage } from "@/pages/LightningPage"
import { LoginPage } from "@/pages/LoginPage"
import { OverviewPage } from "@/pages/OverviewPage"
import { PaymentDetailPage } from "@/pages/PaymentDetailPage"
import { PaymentsPage } from "@/pages/PaymentsPage"
import { SettingsPage } from "@/pages/SettingsPage"
import { SetupPage } from "@/pages/SetupPage"
import { StaffPage } from "@/pages/StaffPage"
import { TrafficPage } from "@/pages/TrafficPage"
import { WalletsPage } from "@/pages/WalletsPage"
import { WebhooksPage } from "@/pages/WebhooksPage"

export default function App() {
  return (
    <Routes>
      <Route path="/setup" element={<SetupPage />} />
      <Route path="/login" element={<LoginPage />} />
      <Route element={<RequireAuth />}>
        <Route element={<AppLayout />}>
          <Route path="/" element={<OverviewPage />} />
          <Route path="/payments" element={<PaymentsPage />} />
          <Route path="/payments/:id" element={<PaymentDetailPage />} />
          <Route path="/deliveries" element={<DeliveriesPage />} />
          <Route path="/traffic" element={<TrafficPage />} />
          <Route path="/wallets" element={<WalletsPage />} />
          <Route path="/account/security" element={<AccountSecurityPage />} />
          <Route path="/account/2fa" element={<Navigate to="/account/security" replace />} />
          <Route element={<RequireAdmin />}>
            {/* The wallet history is admin-only on the API too. */}
            <Route path="/lightning" element={<LightningPage />} />
            <Route path="/api-keys" element={<ApiKeysPage />} />
            <Route path="/webhooks" element={<WebhooksPage />} />
            <Route path="/staff" element={<StaffPage />} />
            <Route path="/settings" element={<SettingsPage />} />
            <Route path="/audit-log" element={<AuditLogPage />} />
          </Route>
        </Route>
      </Route>
      <Route path="*" element={<Navigate to="/" replace />} />
    </Routes>
  )
}
