import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { ExternalLink } from "lucide-react";

import { Empty, PageHeader, QueryError } from "@/components/layout";
import { Button } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { Label } from "@/components/ui/label";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { get, type GroupBy, type Status, type UsageRow } from "@/lib/api";
import { num } from "@/lib/format";

const GROUPS: [GroupBy, string][] = [
  ["client", "Client"],
  ["account", "Account"],
  ["provider", "Provider"],
  ["model", "Model"],
];
const PERIODS: [string, number][] = [
  ["Last 24 hours", 86_400],
  ["Last 7 days", 7 * 86_400],
  ["Last 30 days", 30 * 86_400],
  ["Last 90 days", 90 * 86_400],
];
const EMPTY_KEY: Record<GroupBy, string> = { client: "admin token", account: "no account (gateway)", provider: "-", model: "no model" };

export function UsagePage() {
  const [group, setGroup] = useState<GroupBy>("client");
  const [period, setPeriod] = useState(String(PERIODS[0][1]));
  const status = useQuery({ queryKey: ["status"], queryFn: () => get<Status>("/admin/status") });
  const usage = useQuery({
    queryKey: ["usage", group, Number(period)],
    queryFn: () => get<UsageRow[]>(`/admin/usage?group_by=${group}&since_secs=${period}`),
  });
  const rows = usage.data ?? [];
  const dashboard = status.data?.dashboard_url;

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
          <Tabs value={group} onValueChange={(v) => setGroup(v as GroupBy)}>
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
          <Select value={period} onValueChange={setPeriod}>
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
                <TableCell>{r.key ?? <span className="text-muted-foreground font-sans">{EMPTY_KEY[group]}</span>}</TableCell>
                <TableCell className="text-right">{num(r.requests)}</TableCell>
                <TableCell className="text-right">{num(r.errors)}</TableCell>
                <TableCell className="text-right">{num(r.input_tokens)}</TableCell>
                <TableCell className="text-right">{num(r.output_tokens)}</TableCell>
                <TableCell className="text-right">{num(r.cached_tokens)}</TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
        {usage.isSuccess && rows.length === 0 && <Empty>No requests in this period.</Empty>}
      </Card>
    </>
  );
}
