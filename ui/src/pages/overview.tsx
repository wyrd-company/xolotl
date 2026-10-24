// ---
// relationships:
//   implements: ../../../docs/user/web-ui.md
// ---
import type { ReactNode } from "react";
import { useQuery } from "@tanstack/react-query";
import { AlertTriangle } from "lucide-react";
import { Link } from "react-router";

import { ApiEquivalentCost } from "@/components/api-equivalent-cost";
import { CatalogRefresh } from "@/components/catalog-refresh";
import {
  accountName,
  accountPath,
  accountState,
  peakUsage,
  WindowBars,
} from "@/components/account-state";
import { Empty, Mono, PageHeader, QueryError } from "@/components/layout";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardAction,
  CardContent,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import {
  get,
  PROVIDER_NAMES,
  SUB_PROVIDERS,
  type Account,
  type CatalogRoute,
  type GatewayProvider,
  type Status,
  type UsageRow,
} from "@/lib/api";
import { clock, num, relative } from "@/lib/format";
import { publicUsageWindows } from "@/lib/usage-windows";
import { resetCreditsSummary } from "@/lib/reset-credits";

const DAY = 86_400;
const STATE_COLORS = {
  success: "bg-success",
  destructive: "bg-destructive",
  info: "bg-info",
  secondary: "bg-muted-foreground",
  warning: "bg-warning",
};

export function OverviewPage() {
  const status = useQuery({
    queryKey: ["status"],
    queryFn: () => get<Status>("/admin/status"),
    refetchInterval: 30_000,
  });
  const accounts = useQuery({
    queryKey: ["accounts"],
    queryFn: () => get<Account[]>("/admin/accounts"),
    refetchInterval: 30_000,
  });
  const providers = useQuery({
    queryKey: ["providers"],
    queryFn: () => get<GatewayProvider[]>("/admin/providers"),
  });
  const routes = useQuery({
    queryKey: ["catalog-routes"],
    queryFn: () => get<CatalogRoute[]>("/admin/catalog/routes"),
  });
  const usage = useQuery({
    queryKey: ["usage", "provider", DAY],
    queryFn: () =>
      get<UsageRow[]>(`/admin/usage?group_by=provider&since_secs=${DAY}`),
    refetchInterval: 60_000,
  });
  const s = status.data;
  const accts = accounts.data ?? [];
  const configured = (providers.data ?? []).filter(
    (p) => p.has_key || p.no_auth,
  );
  const pools = SUB_PROVIDERS.map((p) => ({
    provider: p,
    accounts: accts.filter((a) => a.provider === p),
  }));
  const unconfigured = [
    ...pools
      .filter((p) => !p.accounts.length)
      .map((p) => PROVIDER_NAMES[p.provider]),
    ...(providers.data ?? [])
      .filter((p) => !p.has_key && !p.no_auth)
      .map((p) => `${p.name} (no key)`),
  ];
  const failed = accts.filter((a) => a.enabled && a.auth_failed);
  const actionCount =
    failed.length + (s && !s.vault.ok ? 1 : 0) + (s && !s.database.ok ? 1 : 0);
  const modelCount = new Map<string, number>();
  for (const r of routes.data ?? [])
    modelCount.set(r.provider, (modelCount.get(r.provider) ?? 0) + 1);
  const healthReady = status.isSuccess && accounts.isSuccess;

  return (
    <>
      <PageHeader
        title="Overview"
        description={s && <>Strategy: {s.subscription.strategy}</>}
      />
      <QueryError
        error={
          status.error ?? accounts.error ?? providers.error ?? routes.error
        }
      />
      {actionCount > 0 ? (
        <Card
          className="border-warning-border gap-0 overflow-hidden py-0"
          aria-label="Needs action"
        >
          <div className="bg-warning-muted text-warning flex items-center gap-2 border-b px-5 py-4 font-medium">
            <AlertTriangle className="size-4" /> <h2>Needs action</h2>{" "}
            <span className="font-mono text-sm">{actionCount}</span>
          </div>
          <div className="divide-y">
            {s && !s.vault.ok && (
              <ActionRow
                badge="Unreachable"
                name="OpenBao"
                cause={
                  s.vault.error || "Cannot connect to the credential vault."
                }
              >
                <Button asChild size="sm">
                  <a
                    href="https://wyrd.foo/docs/xolotl/web-ui/#restore-openbao"
                    target="_blank"
                    rel="noreferrer"
                  >
                    Check OpenBao
                  </a>
                </Button>
              </ActionRow>
            )}
            {s && !s.database.ok && (
              <ActionRow
                badge="Storage failure"
                name="SQLite"
                cause={s.database.error || "Cannot read the database."}
              >
                <Button asChild size="sm">
                  <a
                    href="https://wyrd.foo/docs/xolotl/web-ui/#restore-sqlite"
                    target="_blank"
                    rel="noreferrer"
                  >
                    Check storage
                  </a>
                </Button>
              </ActionRow>
            )}
            {failed.map((a) => (
              <ActionRow
                key={a.id}
                badge={accountState(a).label}
                name={`${PROVIDER_NAMES[a.provider]} · ${accountName(a)}`}
                cause={
                  a.last_error ||
                  "The provider refused this account's credential."
                }
              >
                <Button asChild size="sm">
                  <Link to={accountPath(a.id)}>
                    {a.provider === "opencode-go"
                      ? "Replace key"
                      : "Log in again"}
                  </Link>
                </Button>
              </ActionRow>
            ))}
          </div>
        </Card>
      ) : healthReady && s ? (
        <p className="text-muted-foreground rounded-lg border px-5 py-3 text-sm">
          <span className="text-success font-medium">Nothing needs action</span>{" "}
          · OpenBao connected · SQLite ok ·{" "}
          {s.unsaved_credentials
            ? `${s.unsaved_credentials} credential(s) waiting to save`
            : "all credentials saved"}
        </p>
      ) : (
        <p className="text-muted-foreground text-sm">
          Health{" "}
          {status.isError || accounts.isError ? "unavailable" : "loading…"}
        </p>
      )}
      <div className="grid min-w-0 items-start gap-6 xl:grid-cols-[minmax(0,1.9fr)_minmax(300px,1fr)]">
        <Card
          className="min-w-0 gap-0 overflow-hidden pb-0"
          aria-label="Health"
        >
          <CardHeader className="pb-4">
            <CardTitle>Health</CardTitle>
            <CardAction>
              <Link to="/accounts" className="text-primary text-sm">
                Accounts
              </Link>
            </CardAction>
          </CardHeader>
          <div className="text-muted-foreground px-6 pb-2 text-xs font-medium uppercase tracking-wide">
            Subscriptions
          </div>
          {accounts.isPending && <Empty>Loading accounts…</Empty>}
          {pools
            .filter((p) => p.accounts.length)
            .map((p) => (
              <SubscriptionRow
                key={p.provider}
                pool={p.accounts}
                softLimit={s?.subscription.soft_limit_percent}
              />
            ))}
          {accounts.isSuccess && accts.length === 0 && (
            <p className="text-muted-foreground px-6 pb-4 text-sm">
              No subscription accounts configured.
            </p>
          )}
          {s && (
            <div className="text-muted-foreground border-t px-6 py-3 text-sm">
              OpenBao {s.vault.ok ? "connected" : "unreachable"} · SQLite{" "}
              {s.database.ok ? "ok" : "failing"} ·{" "}
              {s.unsaved_credentials
                ? `${s.unsaved_credentials} refreshed credential(s) not saved yet; xolotl keeps retrying.`
                : "all credentials saved"}
            </div>
          )}
          <div className="flex items-center justify-between border-t px-6 py-4">
            <h3 className="text-muted-foreground text-xs font-medium uppercase tracking-wide">
              Gateway
            </h3>
            <Link to="/providers" className="text-primary text-sm">
              Manage
            </Link>
          </div>
          {configured.length > 0 && (
            <>
              <Table>
                <TableHeader>
                  <TableRow>
                    <TableHead>Provider</TableHead>
                    <TableHead>Key</TableHead>
                    <TableHead className="text-right">Models</TableHead>
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {configured.map((p) => (
                    <TableRow key={p.name}>
                      <TableCell>
                        <Mono>{p.name}</Mono>
                        {!p.enabled && (
                          <Badge variant="secondary" className="ml-2">
                            disabled
                          </Badge>
                        )}
                      </TableCell>
                      <TableCell>
                        <KeyState provider={p} />
                      </TableCell>
                      <TableCell className="text-right font-mono">
                        {routes.isError
                          ? "unavailable"
                          : routes.isPending
                            ? "…"
                            : num(modelCount.get(p.name) ?? 0)}
                      </TableCell>
                    </TableRow>
                  ))}
                </TableBody>
              </Table>
              <CardContent className="py-4">
                <CatalogRefresh
                  providers={configured
                    .filter((p) => p.enabled)
                    .map((p) => p.name)}
                />
              </CardContent>
            </>
          )}
          {providers.isPending && <Empty>Loading gateway providers…</Empty>}
          {providers.isSuccess && configured.length === 0 && (
            <p className="text-muted-foreground px-6 pb-4 text-sm">
              No gateway providers set up.
            </p>
          )}
          {accounts.isSuccess &&
            providers.isSuccess &&
            unconfigured.length > 0 && (
              <div className="bg-muted text-muted-foreground flex flex-wrap items-center justify-between gap-2 border-t px-6 py-4 text-sm">
                <span>Not set up: {unconfigured.join(" · ")}</span>
                <span className="flex flex-wrap gap-x-3">
                  <Link to="/providers" className="text-primary">
                    Set up more providers
                  </Link>
                  {pools.some((p) => !p.accounts.length) && (
                    <Link to="/accounts/add" className="text-primary">
                      Add subscription account
                    </Link>
                  )}
                </span>
              </div>
            )}
        </Card>

        <Card className="min-w-0" aria-label="Traffic, last 24h">
          <CardHeader>
            <CardTitle>Traffic, last 24h</CardTitle>
            <CardAction>
              <Link to="/usage" className="text-primary text-sm">
                Usage
              </Link>
            </CardAction>
          </CardHeader>
          <CardContent className="flex flex-col gap-5">
            <QueryError error={usage.error} />
            {usage.isSuccess ? (
              <Traffic rows={usage.data} />
            ) : (
              <p className="text-muted-foreground text-sm">
                Traffic {usage.isError ? "unavailable" : "loading…"}
              </p>
            )}
          </CardContent>
        </Card>
      </div>
      <Card className="gap-3 pb-0">
        <CardHeader>
          <CardTitle>Recent errors</CardTitle>
          <CardAction>
            <Link to="/usage?errors=true" className="text-primary text-sm">
              All errors in Usage
            </Link>
          </CardAction>
        </CardHeader>
        {s?.recent_errors.length ? (
          <Table className="min-w-[680px]">
            <TableHeader>
              <TableRow>
                <TableHead>When</TableHead>
                <TableHead>Status</TableHead>
                <TableHead>Client</TableHead>
                <TableHead>Target</TableHead>
                <TableHead>Endpoint</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {s.recent_errors.map((e, i) => (
                <TableRow key={i}>
                  <TableCell className="whitespace-nowrap">
                    {relative(e.ts)}
                  </TableCell>
                  <TableCell>
                    <Badge
                      variant={e.status >= 500 ? "destructive" : "warning"}
                    >
                      {e.status}
                    </Badge>
                  </TableCell>
                  <TableCell className="font-mono text-xs">
                    {e.client_id ?? "admin token"}
                  </TableCell>
                  <TableCell className="font-mono text-xs">
                    {e.provider}
                    {e.model ? `/${e.model}` : ""}
                  </TableCell>
                  <TableCell className="font-mono text-xs">
                    {e.endpoint}
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        ) : (
          <Empty>No failed requests in the log.</Empty>
        )}
      </Card>{" "}
    </>
  );
}

function ActionRow({
  badge,
  name,
  cause,
  children,
}: {
  badge: string;
  name: string;
  cause: string;
  children: ReactNode;
}) {
  return (
    <div className="flex flex-wrap items-center gap-3 px-5 py-4 sm:flex-nowrap sm:gap-5">
      <Badge variant="destructive">{badge}</Badge>
      <div className="min-w-0 grow">
        <div className="font-medium break-words">{name}</div>
        <p className="text-muted-foreground text-sm break-words">{cause}</p>
      </div>
      <div className="shrink-0">{children}</div>
    </div>
  );
}

function SubscriptionRow({
  pool,
  softLimit,
}: {
  pool: Account[];
  softLimit?: number;
}) {
  const available = pool.filter((a) => a.available);
  // Unknown usage must not displace an account with reported quota windows.
  const best = available
    .filter((a) => publicUsageWindows(a.usage?.windows).length)
    .sort((a, b) => peakUsage(a) - peakUsage(b))[0];
  const provider = pool[0].provider;
  return (
    <div className="grid gap-4 border-b px-6 py-4 lg:grid-cols-[minmax(160px,1fr)_minmax(0,1.5fr)]">
      <div className="flex min-w-0 flex-col gap-2">
        <div className="flex flex-wrap items-baseline gap-2">
          <h3 className="font-medium">{PROVIDER_NAMES[provider]}</h3>
          <span
            className={`font-mono text-sm ${available.length ? "text-muted-foreground" : "text-destructive"}`}
          >
            {available.length} of {pool.length} available
          </span>
        </div>
        <div
          className="flex flex-wrap gap-1"
          aria-label={`${PROVIDER_NAMES[provider]} account states`}
        >
          {pool.map((a) => {
            const state = accountState(a);
            const label = `${accountName(a)}: ${state.label}${a.unsaved ? " · credentials not saved" : ""}`;
            return (
              <Link
                key={a.id}
                to={accountPath(a.id)}
                aria-label={label}
                title={label}
                className={`h-2 w-8 rounded-sm ${STATE_COLORS[state.variant]}`}
              />
            );
          })}
        </div>
        {pool
          .filter(
            (a) =>
              a.enabled &&
              !a.auth_failed &&
              (a.cooldown_until ?? 0) > Date.now() / 1000,
          )
          .map((a) => (
            <p key={a.id} className="text-muted-foreground text-xs break-words">
              {accountName(a)} cooling down until {clock(a.cooldown_until!)}
              {a.cooldown_reason ? ` · ${a.cooldown_reason}` : ""}
            </p>
          ))}
        {(provider === "codex" || provider === "claude") && (
          <p className="text-muted-foreground text-xs">
            {resetCreditsSummary(pool)}
          </p>
        )}
        {pool.some((a) => a.unsaved) && (
          <p className="text-warning text-xs">
            Refreshed credentials waiting to save; xolotl keeps retrying.
          </p>
        )}
      </div>
      <div className="min-w-0">
        {best ? (
          <>
            <div className="text-muted-foreground mb-2 text-xs break-words">
              Least-used available account ({accountName(best)})
            </div>
            <WindowBars
              windows={publicUsageWindows(best.usage?.windows)}
              softLimit={softLimit}
            />
          </>
        ) : (
          <p className="text-muted-foreground text-sm">
            {available.length
              ? "No usage reported yet."
              : "No available account, so no quota to show."}
          </p>
        )}
      </div>
    </div>
  );
}

function Traffic({ rows }: { rows: UsageRow[] }) {
  const totals = rows.reduce(
    (t, r) => ({
      requests: t.requests + r.requests,
      errors: t.errors + r.errors,
      input: t.input + r.input_tokens,
      output: t.output + r.output_tokens,
    }),
    { requests: 0, errors: 0, input: 0, output: 0 },
  );
  return (
    <>
      <div className="grid grid-cols-2 gap-x-4 gap-y-6">
        <Stat label="Requests" value={num(totals.requests)} />
        <Stat
          label="Error rate"
          value={
            totals.requests
              ? `${((100 * totals.errors) / totals.requests).toFixed(1)}%`
              : "—"
          }
        >
          <span className="text-muted-foreground text-xs">
            {num(totals.errors)} failed
          </span>
        </Stat>
        <Stat label="Input tokens" value={num(totals.input)} />
        <Stat label="Output tokens" value={num(totals.output)} />
        <div className="col-span-2 text-sm"><span className="text-muted-foreground">API-equivalent USD: </span><ApiEquivalentCost rows={rows} /><p className="text-muted-foreground text-xs">List-price estimate; subscription value, not spend. Unpriced requests excluded.</p></div>
      </div>
      <div className="border-t pt-4">
        <h3 className="text-muted-foreground mb-3 text-sm">
          Requests by provider
        </h3>
        <div className="flex flex-col gap-3">
          {rows.map((r) => (
            <div key={r.key ?? ""} className="text-sm">
              <div className="mb-1 flex justify-between gap-3">
                <span className="min-w-0 break-all">
                  {r.key ?? "Unknown provider"}
                </span>
                <Mono>{num(r.requests)}</Mono>
              </div>
              <div className="bg-muted h-1.5 overflow-hidden rounded-full">
                <div
                  className="bg-primary h-full"
                  style={{
                    width: `${totals.requests ? (100 * r.requests) / totals.requests : 0}%`,
                  }}
                />
              </div>
            </div>
          ))}
        </div>
        {!rows.length && (
          <p className="text-muted-foreground text-sm">
            No requests in the last 24 hours.
          </p>
        )}
      </div>
    </>
  );
}

function Stat({
  label,
  value,
  children,
}: {
  label: string;
  value: string;
  children?: ReactNode;
}) {
  return (
    <div>
      <div className="text-muted-foreground text-sm">{label}</div>
      <div className="font-mono text-2xl font-semibold">{value}</div>
      {children}
    </div>
  );
}

export function KeyState({ provider }: { provider: GatewayProvider }) {
  if (provider.no_auth)
    return <span className="text-muted-foreground">none needed</span>;
  return provider.has_key ? (
    <span className="text-success">in vault</span>
  ) : (
    <span className="text-destructive">missing</span>
  );
}
