import type { ReactNode } from "react";
import { useQuery } from "@tanstack/react-query";
import { AlertTriangle } from "lucide-react";
import { Link } from "react-router";

import { CatalogRefresh } from "@/components/catalog-refresh";
import { accountName, peakUsage, WindowBars } from "@/components/account-state";
import { Empty, Mono, PageHeader, QueryError } from "@/components/layout";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Card, CardAction, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
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

const DAY = 86_400;

export function OverviewPage() {
  const status = useQuery({ queryKey: ["status"], queryFn: () => get<Status>("/admin/status"), refetchInterval: 30_000 });
  const accounts = useQuery({ queryKey: ["accounts"], queryFn: () => get<Account[]>("/admin/accounts"), refetchInterval: 30_000 });
  const providers = useQuery({ queryKey: ["providers"], queryFn: () => get<GatewayProvider[]>("/admin/providers") });
  const routes = useQuery({ queryKey: ["catalog-routes"], queryFn: () => get<CatalogRoute[]>("/admin/catalog/routes") });
  const usage = useQuery({
    queryKey: ["usage", "provider", DAY],
    queryFn: () => get<UsageRow[]>(`/admin/usage?group_by=provider&since_secs=${DAY}`),
    refetchInterval: 60_000,
  });

  const s = status.data;
  const accts = accounts.data ?? [];
  const now = Date.now() / 1000;
  const totals = (usage.data ?? []).reduce(
    (t, r) => ({
      requests: t.requests + r.requests,
      errors: t.errors + r.errors,
      input: t.input + r.input_tokens,
      output: t.output + r.output_tokens,
    }),
    { requests: 0, errors: 0, input: 0, output: 0 },
  );
  const byProvider = new Map((usage.data ?? []).map((r) => [r.key, r.requests]));
  const modelCount = new Map<string, number>();
  for (const r of routes.data ?? []) modelCount.set(r.provider, (modelCount.get(r.provider) ?? 0) + 1);

  const attention: ReactNode[] = [];
  if (s && !s.vault.ok) attention.push(<>OpenBao is unreachable: {s.vault.error}</>);
  if (s && !s.database.ok) attention.push(<>SQLite is failing: {s.database.error}</>);
  if (s && s.unsaved_credentials > 0)
    attention.push(<>{s.unsaved_credentials} refreshed credential(s) are not saved to OpenBao yet; xolotl keeps retrying.</>);
  for (const a of accts.filter((a) => a.enabled)) {
    if (a.auth_failed)
      attention.push(
        <>
          <strong>{accountName(a)}</strong> ({a.provider}) needs to log in again{a.last_error ? ` (${a.last_error})` : ""} ·{" "}
          <Link to="/accounts/add">Log in</Link>
        </>,
      );
    else if (a.cooldown_until && a.cooldown_until > now)
      attention.push(
        <>
          <strong>{accountName(a)}</strong> ({a.provider}) is cooling down until {clock(a.cooldown_until)}
          {a.cooldown_reason ? ` (${a.cooldown_reason})` : ""}
        </>,
      );
  }

  return (
    <>
      <PageHeader
        title="Overview"
        description={
          s && (
            <>Strategy: {s.subscription.strategy}</>
          )
        }
      />
      <QueryError error={status.error ?? accounts.error} />

      {attention.length > 0 && (
        <Alert variant="warning">
          <AlertTriangle />
          <AlertTitle>Needs attention</AlertTitle>
          <AlertDescription>
            <ul className="flex flex-col gap-1">
              {attention.map((a, i) => (
                <li key={i}>{a}</li>
              ))}
            </ul>
          </AlertDescription>
        </Alert>
      )}

      <section aria-label="Subscription pools" className="grid gap-4 md:grid-cols-3">
        {SUB_PROVIDERS.map((p) => {
          const pool = accts.filter((a) => a.provider === p);
          const available = pool.filter((a) => a.available);
          const best = [...available].sort((a, b) => peakUsage(a) - peakUsage(b))[0];
          return (
            <Card key={p}>
              <CardHeader>
                <CardTitle>{PROVIDER_NAMES[p]}</CardTitle>
                <CardAction className="text-muted-foreground text-sm">
                  {available.length} of {pool.length} available
                </CardAction>
              </CardHeader>
              <CardContent className="flex flex-col gap-3">
                {best?.usage?.windows.length ? (
                  <>
                    <div className="text-muted-foreground text-xs">Least used available account</div>
                    <WindowBars windows={best.usage.windows} softLimit={s?.subscription.soft_limit_percent} />
                  </>
                ) : (
                  <div className="text-muted-foreground text-sm">{pool.length ? "No usage reported yet." : "No accounts."}</div>
                )}
                <Link to="/accounts" className="text-primary text-sm">
                  View accounts
                </Link>
              </CardContent>
            </Card>
          );
        })}
      </section>

      <section aria-label="Last 24 hours" className="grid grid-cols-2 gap-4 md:grid-cols-4">
        <Stat label="Requests (24h)" value={num(totals.requests)} />
        <Stat label="Error rate (24h)" value={totals.requests ? `${((100 * totals.errors) / totals.requests).toFixed(1)}%` : "-"} />
        <Stat label="Input tokens (24h)" value={num(totals.input)} />
        <Stat label="Output tokens (24h)" value={num(totals.output)} />
      </section>

      <Card className="gap-3 pb-0">
        <CardHeader>
          <CardTitle>Gateway providers</CardTitle>
          <CardAction>
            <Link to="/providers" className="text-primary text-sm">
              Manage
            </Link>
          </CardAction>
        </CardHeader>
        <CardContent>
          <CatalogRefresh providers={(providers.data ?? []).filter((p) => p.enabled).map((p) => p.name)} />
        </CardContent>
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Provider</TableHead>
              <TableHead>Kind</TableHead>
              <TableHead>Key</TableHead>
              <TableHead className="text-right">Models</TableHead>
              <TableHead className="text-right">Requests (24h)</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {(providers.data ?? []).map((p) => (
              <TableRow key={p.name}>
                <TableCell>
                  <Mono>{p.name}</Mono>
                  {!p.enabled && (
                    <Badge variant="secondary" className="ml-2">
                      disabled
                    </Badge>
                  )}
                </TableCell>
                <TableCell>{p.kind}</TableCell>
                <TableCell>
                  <KeyState provider={p} />
                </TableCell>
                <TableCell className="text-right font-mono">{num(modelCount.get(p.name) ?? 0)}</TableCell>
                <TableCell className="text-right font-mono">{num(byProvider.get(p.name) ?? 0)}</TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
        {providers.data?.length === 0 && <Empty>No gateway providers are configured.</Empty>}
      </Card>

      <Card className="gap-3 pb-0">
        <CardHeader>
          <CardTitle>Recent errors</CardTitle>
          <CardAction>
            <Link to="/usage" className="text-primary text-sm">
              Usage
            </Link>
          </CardAction>
        </CardHeader>
        {s?.recent_errors.length ? (
          <Table>
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
                  <TableCell className="whitespace-nowrap">{relative(e.ts)}</TableCell>
                  <TableCell>
                    <Badge variant={e.status >= 500 ? "destructive" : "warning"}>{e.status}</Badge>
                  </TableCell>
                  <TableCell className="font-mono text-xs">{e.client_id ?? "admin token"}</TableCell>
                  <TableCell className="font-mono text-xs">
                    {e.provider}
                    {e.model ? `/${e.model}` : ""}
                  </TableCell>
                  <TableCell className="font-mono text-xs">{e.endpoint}</TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        ) : (
          <Empty>No failed requests in the log.</Empty>
        )}
      </Card>
    </>
  );
}

function Stat({ label, value }: { label: string; value: string }) {
  return (
    <div className="bg-card rounded-lg border px-5 py-4 shadow-xs">
      <div className="text-muted-foreground text-sm">{label}</div>
      <div className="font-mono text-2xl font-semibold">{value}</div>
    </div>
  );
}

export function KeyState({ provider }: { provider: GatewayProvider }) {
  if (provider.no_auth) return <span className="text-muted-foreground">none needed</span>;
  return provider.has_key ? <span className="text-success">in vault</span> : <span className="text-destructive">missing</span>;
}
