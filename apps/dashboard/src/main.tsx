import { StrictMode } from "react"
import { createRoot } from "react-dom/client"
import { QueryClient, QueryClientProvider } from "@tanstack/react-query"
import { BrowserRouter } from "react-router-dom"

import { Toaster } from "@/components/ui/sonner"
import { TooltipProvider } from "@/components/ui/tooltip"
import { ApiError } from "@/lib/api"
import { AuthProvider } from "@/lib/auth"
import { ProjectScopeProvider } from "@/lib/project-scope"
import App from "./App"
import "./index.css"

const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      // One retry for network / 5xx errors; a 4xx (403 role, 429 rate
      // limit…) would only fail again — and a 429 retry burns the budget.
      retry: (failureCount, error) =>
        failureCount < 1 && !(error instanceof ApiError && error.statusCode < 500),
      refetchOnWindowFocus: false,
    },
  },
})

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <QueryClientProvider client={queryClient}>
      <BrowserRouter>
        <AuthProvider>
          <ProjectScopeProvider>
            <TooltipProvider>
              <App />
            </TooltipProvider>
          </ProjectScopeProvider>
          <Toaster richColors position="top-right" />
        </AuthProvider>
      </BrowserRouter>
    </QueryClientProvider>
  </StrictMode>,
)
