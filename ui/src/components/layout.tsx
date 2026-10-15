import type { ComponentProps, ReactNode } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { NavLink, Outlet } from "react-router";
import { toast } from "sonner";

import { Emblem } from "@/components/emblem";
import { del, get, setCsrfToken, type Session, type Status } from "@/lib/api";
import { cn } from "@/lib/utils";

const NAV = [
  { to: "/", label: "Overview", end: true },
  { to: "/accounts", label: "Accounts" },
  { to: "/policies", label: "Policies" },
  { to: "/keys", label: "Client keys" },
  { to: "/providers", label: "Providers & models" },
  { to: "/mcp", label: "MCP servers" },
  { to: "/usage", label: "Usage" },
];

export function Layout() {
  const qc = useQueryClient();
  const status = useQuery({ queryKey: ["status"], queryFn: () => get<Status>("/admin/status"), refetchInterval: 30_000 });
  const vault = status.data?.vault;
  const user = qc.getQueryData<Session>(["session"])?.user;

  async function signOut() {
    try {
      await del("/admin/session");
    } catch (e) {
      toast.error((e as Error).message);
    }
    setCsrfToken(null);
    qc.clear();
    window.location.hash = "#/";
    window.location.reload();
  }

  return (
    <div className="flex min-h-screen flex-col md:flex-row">
      <nav aria-label="Main" className="bg-sidebar text-sidebar-foreground flex shrink-0 flex-col gap-1 p-4 md:w-60 md:py-6">
        <div className="flex items-center gap-2.5 px-3 pb-3 text-white md:pb-5">
          <Emblem className="h-8 w-auto" />
          <span className="font-mono text-lg font-medium">xolotl</span>
        </div>
        <div className="flex flex-wrap gap-1 md:flex-col">
          {NAV.map((n) => (
            <NavLink
              key={n.to}
              to={n.to}
              end={n.end}
              className={({ isActive }) =>
                cn(
                  "rounded-md px-3 py-2.5 text-sm no-underline transition-colors hover:bg-sidebar-accent hover:text-white",
                  isActive && "bg-sidebar-accent font-medium text-white",
                )
              }
            >
              {n.label}
            </NavLink>
          ))}
        </div>
        <div className="hidden min-h-6 grow md:block" />
        <div className="px-3 pt-3 text-xs text-[#a3a9b3]">
          OpenBao:{" "}
          {vault ? (
            <span className={vault.ok ? "" : "text-red-300"}>{vault.ok ? "connected" : "unreachable"}</span>
          ) : (
            "…"
          )}
          {status.data && <span className="block pt-1">v{status.data.version}</span>}
          {user && <span className="block pt-1">Signed in as {user}</span>}
        </div>
        <button type="button" onClick={signOut} className="rounded-md px-3 py-2.5 text-left text-sm hover:bg-sidebar-accent hover:text-white">
          Sign out
        </button>
      </nav>
      <main className="flex min-w-0 flex-1 flex-col gap-6 px-4 py-6 md:px-10 md:py-8">
        <Outlet />
      </main>
    </div>
  );
}

export function PageHeader({ title, description, children }: { title: string; description?: ReactNode; children?: ReactNode }) {
  return (
    <header className="flex flex-wrap items-center justify-between gap-3">
      <div className="flex flex-col gap-1">
        <h1 className="text-2xl font-semibold md:text-[28px]">{title}</h1>
        {description && <div className="text-muted-foreground text-sm">{description}</div>}
      </div>
      {children && <div className="flex flex-wrap items-center gap-2">{children}</div>}
    </header>
  );
}

export function Mono({ className, ...props }: ComponentProps<"span">) {
  return <span className={cn("font-mono", className)} {...props} />;
}

export function Empty({ children }: { children: ReactNode }) {
  return <div className="text-muted-foreground px-4 py-8 text-center text-sm">{children}</div>;
}

export function QueryError({ error }: { error: Error | null }) {
  if (!error) return null;
  return <div className="bg-danger-muted text-destructive rounded-md px-4 py-3 text-sm">{error.message}</div>;
}
