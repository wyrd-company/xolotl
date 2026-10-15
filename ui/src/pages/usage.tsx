import type { ReactNode } from "react";
import { useQuery } from "@tanstack/react-query";
import { ExternalLink, X } from "lucide-react";
import { Link, useSearchParams } from "react-router";

import { Empty, PageHeader, QueryError } from "@/components/layout";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { Label } from "@/components/ui/label";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { get, type GroupBy, type Status, type UsageRow } from "@/lib/api";
import { num } from "@/lib/format";
import { GROUPS, narrowTo, PERIODS, readView, withParams } from "@/lib/usage-view";

const EMPTY_KEY: Record<GroupBy, string> = { client: "admin token", account: "no account (gateway)", provider: "-", model: "no model" };
export function UsagePage() {
  const [params, setParams] = useSearchParams();
  const { group, period, filters } = readView(params);
  const api = new URLSearchParams({ group_by: group, since_secs: String(period), ...Object.fromEntries(filters) });
  const status = useQuery({ queryKey: ["status"], queryFn: () => get<Status>("/admin/status") });
  const usage = useQuery({
    queryKey: ["usage", group, period, filters],
    queryFn: () => get<UsageRow[]>(`/admin/usage?${api}`),
  });
  const rows = usage.data ?? [];
  const dashboard = status.data?.dashboard_url;
  const setView = (change: Record<string, string>) => setParams(withParams(params, change), { replace: true });

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
      <div className="flex flex-wrap items-end gap-6">
        <div className="flex flex-col gap-2">
          <span id="group-by" className="text-sm font-medium">
            Group by
          </span>
          <Tabs value={group} onValueChange={(v) => setView({ group: v })}>
            <TabsList aria-labelledby="group-by">
              {GROUPS.map(([g, label]) => (
                <TabsTrigger key={g} value={g}>
                  {label}
                </TabsTrigger>
              ))}
            </TabsList>
          </Tabs>
        </div>
        <div className="flex flex-col gap-2">
          <Label htmlFor="period">Period</Label>
          <Select value={String(period)} onValueChange={(v) => setView({ since: v })}>
            <SelectTrigger id="period" className="w-44">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {PERIODS.map(([label, secs]) => (
                <SelectItem key={secs} value={String(secs)}>
                  {label}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div>
      </div>
      {filters.length > 0 && (
        <div className="flex flex-wrap items-center gap-2" aria-label="Filters">
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
          <Link to={withParams(params, Object.fromEntries(GROUPS.map(([g]) => [g, null])))} className="text-primary text-sm">
            Clear all
          </Link>
        </div>
      )}
      <QueryError error={usage.error} />
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
              </TableRow>
            ))}
          </TableBody>
        </Table>
        {usage.isSuccess && rows.length === 0 && (
          <Empty>{filters.length ? "No matching requests in this period." : "No requests in this period."}</Empty>
        )}
      </Card>
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
