// ---
// relationships:
//   implements: ../../../docs/user/web-ui.md
// ---
import { useState, type ReactNode } from "react";
import { useQuery } from "@tanstack/react-query";
import { ExternalLink, X } from "lucide-react";
import { Link, useSearchParams } from "react-router";

import { ApiEquivalentCost } from "@/components/api-equivalent-cost";
import { LookBackInput } from "@/components/look-back-input";
import { Empty, PageHeader, QueryError } from "@/components/layout";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { get, type GroupBy, type Status, type UsageRow } from "@/lib/api";
import { DURATION_ERROR, durationSeconds, formatDuration } from "@/lib/duration";
import { num } from "@/lib/format";
import { SessionsCard, ToolsCard } from "@/pages/usage-sessions";
import { GROUPS, narrowTo, readView, withParams } from "@/lib/usage-view";

const EMPTY_KEY: Record<GroupBy, string> = { client: "admin token", account: "no account (gateway)", provider: "-", model: "no model" };
export function UsagePage() {
  const [params, setParams] = useSearchParams();
  const { group, period, filters } = readView(params);
  const errorsOnly = params.get("errors") === "true";
  const api = new URLSearchParams({ group_by: group, since_secs: String(period), ...Object.fromEntries(filters) });
  if (errorsOnly) api.set("errors", "true");
  const status = useQuery({ queryKey: ["status"], queryFn: () => get<Status>("/admin/status") });
  const usage = useQuery({
    queryKey: ["usage", group, period, filters, errorsOnly],
    queryFn: () => get<UsageRow[]>(`/admin/usage?${api}`),
    enabled: period !== null,
  });
  const rows = usage.data ?? [];
  const clientFilter = filters.find(([g]) => g === "client")?.[1];
  const dashboard = status.data?.dashboard_url;
  const setView = (change: Record<string, string | null>) => setParams(withParams(params, change), { replace: true });

  return (
    <>
      <PageHeader title="Usage" description="From the request log. Traces and dashboards are in OpenObserve.">
        {dashboard && (
          <Button asChild variant="outline">
            <a href={dashboard} target="_blank" rel="noreferrer noopener">
              Open OpenObserve <ExternalLink />
            </a>
          </Button>
        )}
      </PageHeader>
      <UsageFilters key={params.toString()} params={params} apply={(change) => setView(change)} />
      {(filters.length > 0 || errorsOnly) && (
        <div className="flex flex-wrap items-center gap-2" aria-label="Filters">
          {errorsOnly && (
            <Badge variant="outline" className="py-1 pr-1 text-sm font-normal">
              Errors only
              <Link to={withParams(params, { errors: null })} aria-label="Remove errors filter" className="text-muted-foreground hover:text-foreground rounded-full p-0.5">
                <X />
              </Link>
            </Badge>
          )}
          {filters.map(([g, value]) => (
            <Badge key={g} variant="outline" className="py-1 pr-1 text-sm font-normal">
              <span className="text-muted-foreground">{GROUPS.find(([d]) => d === g)![1]}:</span>
              <span className={value ? "font-mono" : "italic"}>{value || EMPTY_KEY[g]}</span>
              <Link
                to={withParams(params, { [g]: null })}
                aria-label={`Remove ${g} filter`}
                className="text-muted-foreground hover:text-foreground rounded-full p-0.5"
              >
                <X />
              </Link>
            </Badge>
          ))}
          <Link to={withParams(params, { ...Object.fromEntries(GROUPS.map(([g]) => [g, null])), errors: null })} className="text-primary text-sm">
            Clear all
          </Link>
        </div>
      )}
      {period === null && <p role="alert" className="text-destructive text-sm">{DURATION_ERROR} Apply a valid look-back period to load usage.</p>}
      <QueryError error={usage.error} />
      {usage.isError && <Button variant="outline" className="self-start" onClick={() => void usage.refetch()}>Retry query</Button>}
      {usage.isFetching && <p role="status" className="text-muted-foreground text-sm">Loading request usage…</p>}
      <p className="text-muted-foreground text-sm">API-equivalent costs use list prices recorded with each request. Subscription costs are estimates of API value, not spend. Unpriced requests are excluded.</p>
      <Card className="py-0">
        <Table className="min-w-[720px]">
          <TableHeader>
            <TableRow>
              <TableHead>{GROUPS.find(([g]) => g === group)![1]}</TableHead>
              <TableHead className="text-right">Requests</TableHead>
              <TableHead className="text-right">Errors</TableHead>
              <TableHead className="text-right">Input tokens</TableHead>
              <TableHead className="text-right">Output tokens</TableHead>
              <TableHead className="text-right">Cached</TableHead>
              <TableHead className="text-right">API-equivalent USD</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody className="font-mono text-xs">
            {rows.map((r) => (
              <TableRow key={r.key ?? ""}>
                <TableCell>
                  <NarrowLink to={narrowTo(params, r.key)} group={group}>
                    <Key row={r} group={group} />
                  </NarrowLink>
                </TableCell>
                <TableCell className="text-right">{num(r.requests)}</TableCell>
                <TableCell className="text-right">{num(r.errors)}</TableCell>
                <TableCell className="text-right">{num(r.input_tokens)}</TableCell>
                <TableCell className="text-right">{num(r.output_tokens)}</TableCell>
                <TableCell className="text-right">{num(r.cached_tokens)}</TableCell>
                <TableCell className="text-right"><ApiEquivalentCost rows={[r]} /></TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
        {usage.isSuccess && rows.length === 0 && (
          <Empty>{filters.length || errorsOnly ? "No matching requests in this period. Change a filter or the look-back period." : "No requests in this period. Change the look-back period."}</Empty>
        )}
      </Card>
      <SessionsCard client={clientFilter} />
      <ToolsCard client={clientFilter} />
    </>
  );
}

function Key({ row, group }: { row: UsageRow; group: GroupBy }) {
  return row.key ?? <span className="text-muted-foreground font-sans">{EMPTY_KEY[group]}</span>;
}

function NarrowLink({ to, group, children }: { to: string | null; group: GroupBy; children: ReactNode }) {
  if (!to) return children;
  return (
    <Link to={to} title={`Narrow to this ${group}`} className="hover:text-primary underline-offset-4 hover:underline">
      {children}
    </Link>
  );
}

function UsageFilters({ params, apply }: { params: URLSearchParams; apply: (change: Record<string, string | null>) => void }) {
  const view = readView(params);
  const [duration, setDuration] = useState(params.get("since") ?? formatDuration(view.period ?? 86_400));
  const [error, setError] = useState<string>();
  const [draft, setDraft] = useState(() => Object.fromEntries(GROUPS.map(([g]) => [g, {
    enabled: params.has(g), value: params.get(g) ?? "",
  }])) as Record<GroupBy, { enabled: boolean; value: string }>);
  return (
    <form className="flex flex-col gap-4" onSubmit={(event) => {
      event.preventDefault();
      const seconds = durationSeconds(duration);
      if (seconds === null) {
        setError(DURATION_ERROR);
        event.currentTarget.querySelector<HTMLInputElement>("#look-back")?.focus();
        return;
      }
      setError(undefined);
      apply({ since: String(seconds), ...Object.fromEntries(GROUPS.map(([g]) => [g, draft[g].enabled ? draft[g].value : null])) });
    }}>
      <div className="flex flex-wrap items-start gap-6">
        <div className="flex flex-col gap-2">
          <span id="group-by" className="text-sm font-medium">Group by</span>
          <div role="group" aria-labelledby="group-by" className="flex flex-wrap gap-2">
            {GROUPS.map(([g, label]) => (
              <Button key={g} type="button" variant={view.group === g ? "secondary" : "outline"}
                aria-pressed={view.group === g} className="rounded-full" onClick={() => apply({ group: g })}>
                {label}
              </Button>
            ))}
          </div>
        </div>
        <LookBackInput value={duration} onChange={setDuration} error={error} />
      </div>
      <div className="grid gap-4 sm:grid-cols-2 xl:grid-cols-4">
        {GROUPS.map(([g, label]) => (
          <div key={g} className="flex flex-col gap-2">
            <Label className="min-h-6">
              <input type="checkbox" checked={draft[g].enabled} onChange={(event) => setDraft({ ...draft, [g]: { ...draft[g], enabled: event.target.checked } })} />
              Filter by {g}
            </Label>
            <Label htmlFor={`filter-${g}`}>{label}</Label>
            <Input id={`filter-${g}`} value={draft[g].value} disabled={!draft[g].enabled}
              onChange={(event) => setDraft({ ...draft, [g]: { ...draft[g], value: event.target.value } })} />
          </div>
        ))}
      </div>
      <p className="text-muted-foreground text-sm">An unchecked filter matches any value. A checked filter with an empty entry matches an empty value, such as a request without a client key.</p>
      <Button type="submit" className="self-start">Apply filters</Button>
    </form>
  );
}
