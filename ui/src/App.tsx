import { useEffect, useState } from "react";
import { QueryClient, QueryClientProvider, useQuery, useQueryClient } from "@tanstack/react-query";
import { createHashRouter, Navigate, RouterProvider } from "react-router";

import { Layout } from "@/components/layout";
import { Toaster } from "@/components/ui/sonner";
import { get, setCsrfToken, setUnauthorizedHandler, type Session } from "@/lib/api";
import { AccountsPage } from "@/pages/accounts";
import { AddAccountPage } from "@/pages/add-account";
import { KeysPage } from "@/pages/keys";
import { LoginPage } from "@/pages/login";
import { McpPage } from "@/pages/mcp";
import { OverviewPage } from "@/pages/overview";
import { PoliciesPage } from "@/pages/policies";
import { ProvidersPage } from "@/pages/providers";
import { UsagePage } from "@/pages/usage";

const queryClient = new QueryClient({
  defaultOptions: { queries: { retry: (n, e) => n < 2 && (e as { status?: number }).status !== 401, refetchOnWindowFocus: true } },
});

// Hash routes: xolotl serves only `/`, so every screen lives in the fragment.
const router = createHashRouter([
  {
    element: <Layout />,
    children: [
      { index: true, element: <OverviewPage /> },
      { path: "accounts", element: <AccountsPage /> },
      { path: "accounts/add", element: <AddAccountPage /> },
      { path: "policies", element: <PoliciesPage /> },
      { path: "policies/:name", element: <PoliciesPage /> },
      { path: "keys", element: <KeysPage /> },
      { path: "providers", element: <ProvidersPage /> },
      { path: "mcp", element: <McpPage /> },
      { path: "usage", element: <UsagePage /> },
      { path: "*", element: <Navigate to="/" replace /> },
    ],
  },
]);

function Gate() {
  const qc = useQueryClient();
  const session = useQuery({
    queryKey: ["session"],
    queryFn: async () => {
      const s = await get<Session>("/admin/session");
      if (s.authenticated && s.csrf_token) setCsrfToken(s.csrf_token);
      return s;
    },
    staleTime: Infinity,
  });
  // Set by a 401 (expired session); otherwise the session check decides.
  const [expired, setExpired] = useState(false);
  const signedIn = !expired && !!session.data?.authenticated;

  useEffect(() => {
    setUnauthorizedHandler(() => {
      setCsrfToken(null);
      setExpired(true);
      qc.clear();
    });
    return () => setUnauthorizedHandler(null);
  }, [qc]);

  if (session.isPending) return null;
  if (!signedIn) {
    return <LoginPage configured={session.data?.configured ?? true} error={session.error?.message} />;
  }
  return <RouterProvider router={router} />;
}

export default function App() {
  return (
    <QueryClientProvider client={queryClient}>
      <Gate />
      <Toaster position="bottom-right" richColors />
    </QueryClientProvider>
  );
}
