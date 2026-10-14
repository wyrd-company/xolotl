import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { ChevronDown, ChevronRight, RefreshCw } from "lucide-react";

import { Empty, Mono, PageHeader, QueryError } from "@/components/layout";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardAction, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { get, post, put, type McpCall, type McpGroupBy, type McpServer, type McpTool, type McpUsageRow } from "@/lib/api";
import { clock, num, relative } from "@/lib/format";
import { useAction } from "@/lib/hooks";

const GROUPS: [McpGroupBy, string][] = [
  ["server", "Server"],
  ["tool", "Tool"],
  ["client", "Client"],
];

export function McpPage() {
  const servers = useQuery({ queryKey: ["mcp-servers"], queryFn: () => get<McpServer[]>("/admin/mcp/servers"), refetchInterval: 30_000 });
  const refresh = useAction(() => post<McpServer[]>("/admin/mcp/refresh"), {
    invalidate: [["mcp-servers"], ["mcp-tools"], ["policy-view"]],
    success: "Tool lists refreshed",
  });
  const list = servers.data ?? [];

  return (
    <>
      <PageHeader
        title="MCP servers"
        description={
          <>
            Upstream servers come from the config file; header secrets live in OpenBao. Clients connect to <Mono>{window.location.origin}/mcp</Mono>{" "}
            with a key whose policy has the <Mono>mcp</Mono> scope, and see them in code mode.
          </>
        }
      >
        <Button variant="outline" disabled={refresh.isPending || list.length === 0} onClick={() => refresh.mutate(undefined)}>
          <RefreshCw className={refresh.isPending ? "animate-spin" : undefined} />
          Refresh tools
        </Button>
      </PageHeader>
      <QueryError error={servers.error} />

      <section aria-label="MCP servers" className="flex flex-col gap-4">
        {list.map((s) => (
          <ServerCard key={s.name} s={s} />
        ))}
        {servers.isSuccess && list.length === 0 && (
          <Card>
            <Empty>
              No MCP servers are configured. Add them under <Mono>[mcp.servers.&lt;name&gt;]</Mono> in the config file.
            </Empty>
          </Card>
        )}
      </section>

      {list.length > 0 && <Calls />}
    </>
  );
}

function ServerState({ s }: { s: McpServer }) {
  if (s.last_error) return <Badge variant="destructive">error</Badge>;
  if (s.connected) return <Badge variant="success">connected</Badge>;
  return <Badge variant="secondary">idle</Badge>;
}

function ServerCard({ s }: { s: McpServer }) {
  const [open, setOpen] = useState(false);
  const refresh = useAction(() => post(`/admin/mcp/servers/${s.name}/refresh`), {
    invalidate: [["mcp-servers"], ["mcp-tools", s.name], ["policy-view"]],
    success: (r) => `${s.name}: ${(r as { tools: number }).tools} tools`,
  });
  const fields = Object.entries(s.secret_fields);

  return (
    <Card className="gap-4">
      <CardHeader>
        <CardTitle className="flex flex-wrap items-center gap-2">
          <span className="font-mono">{s.name}</span>
          <ServerState s={s} />
        </CardTitle>
        <CardDescription>{s.description || <Mono className="break-all">{s.url}</Mono>}</CardDescription>
        <CardAction>
          <Button variant="outline" size="sm" disabled={refresh.isPending} onClick={() => refresh.mutate(undefined)}>
            <RefreshCw className={refresh.isPending ? "animate-spin" : undefined} />
            Refresh
          </Button>
        </CardAction>
      </CardHeader>
      <CardContent className="flex flex-col gap-4 text-sm">
        <dl className="grid gap-x-6 gap-y-2 sm:grid-cols-[auto_1fr]">
          <dt className="text-muted-foreground">In code</dt>
          <dd>
            <Mono>{s.namespace}</Mono>
          </dd>
          {s.description && (
            <>
              <dt className="text-muted-foreground">URL</dt>
              <dd>
                <Mono className="break-all">{s.url}</Mono>
              </dd>
            </>
          )}
          <dt className="text-muted-foreground">Server</dt>
          <dd>
            {s.server_info ?? "-"}
            {s.protocol_version && <span className="text-muted-foreground"> · MCP {s.protocol_version}</span>}
          </dd>
          <dt className="text-muted-foreground">Tools</dt>
          <dd>
            {s.tools} · listed {relative(s.tools_refreshed_at)}
          </dd>
        </dl>
        {s.last_error && <p className="bg-danger-muted text-destructive rounded-md px-3 py-2 break-words">{s.last_error}</p>}
        {fields.length > 0 && (
          <div className="flex flex-col gap-3">
            <h3 className="font-medium">
              Secret <Mono className="text-muted-foreground font-normal">{s.secret}</Mono>
            </h3>
            {fields.map(([field, set]) => (
              <SecretField key={field} server={s.name} field={field} set={set} />
            ))}
          </div>
        )}
        <div>
          <Button variant="ghost" size="sm" className="-ml-2" aria-expanded={open} onClick={() => setOpen(!open)}>
            {open ? <ChevronDown /> : <ChevronRight />}
            {open ? "Hide tools" : "Show tools"}
          </Button>
        </div>
        {open && <Tools server={s.name} />}
      </CardContent>
    </Card>
  );
}

function SecretField({ server, field, set }: { server: string; field: string; set: boolean }) {
  const [value, setValue] = useState("");
  const save = useAction(() => put(`/admin/mcp/servers/${server}/secret`, { fields: { [field]: value.trim() } }), {
    invalidate: [["mcp-servers"]],
    success: `Saved ${field} for ${server}`,
    onSuccess: () => setValue(""),
  });
  const id = `secret-${server}-${field}`;
  return (
    <form
      className="flex flex-col gap-2"
      onSubmit={(e) => {
        e.preventDefault();
        save.mutate(undefined);
      }}
    >
      <Label htmlFor={id} className="flex items-center gap-2">
        <Mono>{`{${field}}`}</Mono>
        {set ? <Badge variant="success">set</Badge> : <Badge variant="warning">not set</Badge>}
      </Label>
      <div className="flex max-w-xl gap-2">
        <Input
          id={id}
          type="password"
          autoComplete="off"
          className="font-mono"
          placeholder={set ? "Replace value" : "Value"}
          value={value}
          onChange={(e) => setValue(e.target.value)}
        />
        <Button type="submit" variant="outline" disabled={!value.trim() || save.isPending}>
          Save
        </Button>
      </div>
    </form>
  );
}

function Tools({ server }: { server: string }) {
  const tools = useQuery({ queryKey: ["mcp-tools", server], queryFn: () => get<McpTool[]>(`/admin/mcp/servers/${server}/tools`) });
  const list = tools.data ?? [];
  return (
    <div className="overflow-hidden rounded-md border">
      <QueryError error={tools.error} />
      <Table className="min-w-[600px]">
        <TableHeader>
          <TableRow>
            <TableHead>Tool</TableHead>
            <TableHead>Description</TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          {list.map((t) => (
            <TableRow key={t.name}>
              <TableCell className="align-top font-mono text-xs">
                {t.name}
                {t.annotations?.readOnlyHint && (
                  <Badge variant="outline" className="ml-2 font-sans">
                    read-only
                  </Badge>
                )}
                {t.annotations?.destructiveHint && (
                  <Badge variant="warning" className="ml-2 font-sans">
                    destructive
                  </Badge>
                )}
              </TableCell>
              <TableCell className="text-muted-foreground text-xs whitespace-normal">{t.description?.split("\n")[0] ?? ""}</TableCell>
            </TableRow>
          ))}
        </TableBody>
      </Table>
      {tools.isSuccess && list.length === 0 && <Empty>No tools listed yet. Refresh to ask the server.</Empty>}
    </div>
  );
}

function Calls() {
  const [group, setGroup] = useState<McpGroupBy>("server");
  const usage = useQuery({ queryKey: ["mcp-usage", group], queryFn: () => get<McpUsageRow[]>(`/admin/mcp/usage?group_by=${group}`) });
  const calls = useQuery({ queryKey: ["mcp-calls"], queryFn: () => get<McpCall[]>("/admin/mcp/calls?limit=25"), refetchInterval: 30_000 });

  return (
    <div className="grid gap-4 xl:grid-cols-2">
      <Card className="gap-4 pb-0">
        <CardHeader>
          <CardTitle>Tool calls, last 24 hours</CardTitle>
          <CardAction>
            <Tabs value={group} onValueChange={(v) => setGroup(v as McpGroupBy)}>
              <TabsList aria-label="Group by">
                {GROUPS.map(([g, label]) => (
                  <TabsTrigger key={g} value={g}>
                    {label}
                  </TabsTrigger>
                ))}
              </TabsList>
            </Tabs>
          </CardAction>
        </CardHeader>
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>{GROUPS.find(([g]) => g === group)![1]}</TableHead>
              <TableHead className="text-right">Calls</TableHead>
              <TableHead className="text-right">Errors</TableHead>
              <TableHead className="text-right">Avg ms</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody className="font-mono text-xs">
            {(usage.data ?? []).map((r) => (
              <TableRow key={r.key ?? ""}>
                <TableCell>{r.key ?? <span className="text-muted-foreground font-sans">admin token</span>}</TableCell>
                <TableCell className="text-right">{num(r.calls)}</TableCell>
                <TableCell className="text-right">{num(r.errors)}</TableCell>
                <TableCell className="text-right">{num(Math.round(r.avg_latency_ms))}</TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
        {usage.isSuccess && usage.data.length === 0 && <Empty>No tool calls in the last 24 hours.</Empty>}
      </Card>

      <Card className="gap-4 pb-0">
        <CardHeader>
          <CardTitle>Recent calls</CardTitle>
        </CardHeader>
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Time</TableHead>
              <TableHead>Tool</TableHead>
              <TableHead>Client</TableHead>
              <TableHead>Result</TableHead>
              <TableHead className="text-right">ms</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody className="text-xs">
            {(calls.data ?? []).map((c, i) => (
              <TableRow key={`${c.ts}-${i}`}>
                <TableCell>{clock(c.ts)}</TableCell>
                <TableCell className="font-mono">
                  {c.server}/{c.tool}
                </TableCell>
                <TableCell className="font-mono">{c.client_id ?? "-"}</TableCell>
                <TableCell title={c.error ?? undefined}>
                  {c.status === "ok" ? (
                    <Badge variant="success">ok</Badge>
                  ) : (
                    <Badge variant="destructive">{c.status === "tool_error" ? "tool error" : "failed"}</Badge>
                  )}
                </TableCell>
                <TableCell className="text-right font-mono">{num(c.latency_ms)}</TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
        {calls.isSuccess && calls.data.length === 0 && <Empty>No tool calls yet.</Empty>}
      </Card>
    </div>
  );
}
