import { useEffect, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { ChevronDown, ChevronRight, ExternalLink, LogIn, Pencil, Plus, RefreshCw, Trash2 } from "lucide-react";
import { toast } from "sonner";

import { Confirm } from "@/components/confirm";
import { Empty, Mono, PageHeader, QueryError } from "@/components/layout";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardAction, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import {
  del,
  get,
  post,
  put,
  type McpCall,
  type McpGroupBy,
  type McpLoginStart,
  type McpLoginStatus,
  type McpOAuthStatus,
  type McpServer,
  type McpServerDefinition,
  type McpTool,
  type McpUsageRow,
} from "@/lib/api";
import { clock, dateTime, num, relative } from "@/lib/format";
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
  const [adding, setAdding] = useState(false);

  return (
    <>
      <PageHeader
        title="MCP servers"
        description={
          <>
            Servers added here are stored in OpenBao, next to the config file's; so are their secrets and logins. Clients connect to{" "}
            <Mono>{window.location.origin}/mcp</Mono> with a key whose policy has the <Mono>mcp</Mono> scope, and see them in code mode.
          </>
        }
      >
        <Button disabled={adding} onClick={() => setAdding(true)}>
          <Plus /> Add server
        </Button>
        <Button variant="outline" disabled={refresh.isPending || list.length === 0} onClick={() => refresh.mutate(undefined)}>
          <RefreshCw className={refresh.isPending ? "animate-spin" : undefined} />
          Refresh tools
        </Button>
      </PageHeader>
      <QueryError error={servers.error} />

      <section aria-label="MCP servers" className="flex flex-col gap-4">
        {adding && <ServerForm initial={null} onDone={() => setAdding(false)} />}
        {list.map((s) => (
          <ServerCard key={s.name} s={s} />
        ))}
        {servers.isSuccess && list.length === 0 && !adding && (
          <Card>
            <Empty>
              No MCP servers yet. Add one here, or under <Mono>[mcp.servers.&lt;name&gt;]</Mono> in the config file.
            </Empty>
          </Card>
        )}
      </section>

      {list.length > 0 && <Calls />}
    </>
  );
}

function ServerState({ s }: { s: McpServer }) {
  if (s.oauth?.needs_login) return <Badge variant="warning">sign-in needed</Badge>;
  if (s.last_error) return <Badge variant="destructive">error</Badge>;
  if (s.connected) return <Badge variant="success">connected</Badge>;
  return <Badge variant="secondary">idle</Badge>;
}

function ServerCard({ s }: { s: McpServer }) {
  const [editing, setEditing] = useState(false);
  if (editing && s.definition) return <ServerForm initial={{ name: s.name, def: s.definition }} onDone={() => setEditing(false)} />;
  return <ServerView s={s} onEdit={() => setEditing(true)} />;
}

function ServerView({ s, onEdit }: { s: McpServer; onEdit: () => void }) {
  const [open, setOpen] = useState(false);
  const remove = useAction(() => del(`/admin/mcp/servers/${s.name}`), {
    invalidate: [["mcp-servers"], ["policy-view"]],
    success: `Removed ${s.name}`,
  });
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
          {s.source === "config" && <Badge variant="outline">config file</Badge>}
        </CardTitle>
        <CardDescription>{s.description || <Mono className="break-all">{s.url}</Mono>}</CardDescription>
        <CardAction className="flex gap-2">
          <Button variant="outline" size="sm" disabled={refresh.isPending} onClick={() => refresh.mutate(undefined)}>
            <RefreshCw className={refresh.isPending ? "animate-spin" : undefined} />
            Refresh
          </Button>
          {s.source === "vault" && (
            <>
              <Button variant="outline" size="sm" onClick={onEdit}>
                <Pencil /> Edit
              </Button>
              <Confirm
                title={`Remove ${s.name}?`}
                description="Its definition, secrets and login are deleted from OpenBao. Policies that list it must drop it first."
                action="Remove"
                onConfirm={() => remove.mutate(undefined)}
              >
                <Button variant="outline" size="sm" className="text-destructive" disabled={remove.isPending} aria-label={`Remove ${s.name}`}>
                  <Trash2 />
                </Button>
              </Confirm>
            </>
          )}
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
        {s.last_error && !s.oauth?.needs_login && (
          <p className="bg-danger-muted text-destructive rounded-md px-3 py-2 break-words">{s.last_error}</p>
        )}
        {s.oauth && <OAuthLogin server={s.name} oauth={s.oauth} />}
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

interface FormState {
  name: string;
  url: string;
  description: string;
  auth: "headers" | "oauth";
  headers: { name: string; value: string }[];
  timeout: string;
  clientId: string;
  scopes: string;
}

function toForm(initial: { name: string; def: McpServerDefinition } | null): FormState {
  const d = initial?.def;
  return {
    name: initial?.name ?? "",
    url: d?.url ?? "",
    description: d?.description ?? "",
    auth: d?.auth ?? "headers",
    headers: Object.entries(d?.headers ?? {}).map(([name, value]) => ({ name, value })),
    timeout: d?.timeout_secs ? String(d.timeout_secs) : "",
    clientId: d?.oauth?.client_id ?? "",
    scopes: (d?.oauth?.scopes ?? []).join(" "),
  };
}

function toDefinition(f: FormState): McpServerDefinition {
  const oauth = f.auth === "oauth";
  return {
    url: f.url.trim(),
    description: f.description.trim(),
    headers: Object.fromEntries(f.headers.filter((h) => h.name.trim()).map((h) => [h.name.trim(), h.value])),
    timeout_secs: f.timeout.trim() ? Number(f.timeout) : null,
    auth: f.auth,
    oauth: {
      client_id: oauth && f.clientId.trim() ? f.clientId.trim() : null,
      scopes: oauth ? f.scopes.split(/\s+/).filter(Boolean) : [],
    },
  };
}

function ServerForm({ initial, onDone }: { initial: { name: string; def: McpServerDefinition } | null; onDone: () => void }) {
  const isNew = initial === null;
  const [f, setF] = useState<FormState>(() => toForm(initial));
  const set = <K extends keyof FormState>(k: K, v: FormState[K]) => setF((x) => ({ ...x, [k]: v }));
  const name = f.name.trim();
  const save = useAction(
    () => (isNew ? post("/admin/mcp/servers", { name, ...toDefinition(f) }) : put(`/admin/mcp/servers/${name}`, toDefinition(f))),
    {
      invalidate: [["mcp-servers"], ["mcp-tools", name], ["policy-view"]],
      success: isNew ? `Added ${name}` : `Saved ${name}`,
      onSuccess: onDone,
    },
  );
  const id = (k: string) => `server-${initial?.name ?? "new"}-${k}`;

  return (
    <Card>
      <CardHeader>
        <CardTitle>{isNew ? "Add MCP server" : <span className="font-mono">{initial.name}</span>}</CardTitle>
        <CardDescription>
          A Streamable HTTP MCP server. Saved to <Mono>mcp-servers/{name || "…"}</Mono> in OpenBao and used right away.
        </CardDescription>
      </CardHeader>
      <CardContent>
        <form
          className="flex flex-col gap-5 text-sm"
          onSubmit={(e) => {
            e.preventDefault();
            save.mutate(undefined);
          }}
        >
          <div className="grid gap-4 md:grid-cols-[minmax(0,16rem)_1fr]">
            <div className="flex flex-col gap-2">
              <Label htmlFor={id("name")}>Name</Label>
              <Input
                id={id("name")}
                className="font-mono"
                placeholder="github-work"
                value={f.name}
                disabled={!isNew}
                onChange={(e) => set("name", e.target.value)}
              />
              <span className="text-muted-foreground text-xs">
                Lowercase. In code: <Mono>{name.replace(/-/g, "_") || "…"}</Mono>
              </span>
            </div>
            <div className="flex flex-col gap-2">
              <Label htmlFor={id("url")}>URL</Label>
              <Input
                id={id("url")}
                className="font-mono"
                placeholder="https://mcp.example.com/mcp"
                value={f.url}
                onChange={(e) => set("url", e.target.value)}
              />
            </div>
          </div>
          <div className="flex flex-col gap-2">
            <Label htmlFor={id("desc")}>Description</Label>
            <Input id={id("desc")} placeholder="Shown to clients by search" value={f.description} onChange={(e) => set("description", e.target.value)} />
          </div>
          <div className="grid gap-4 md:grid-cols-[minmax(0,16rem)_minmax(0,10rem)]">
            <div className="flex flex-col gap-2">
              <Label htmlFor={id("auth")}>Authentication</Label>
              <Select value={f.auth} onValueChange={(v) => set("auth", v as FormState["auth"])}>
                <SelectTrigger id={id("auth")}>
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="headers">Headers</SelectItem>
                  <SelectItem value="oauth">OAuth login</SelectItem>
                </SelectContent>
              </Select>
            </div>
            <div className="flex flex-col gap-2">
              <Label htmlFor={id("timeout")}>Call timeout (s)</Label>
              <Input
                id={id("timeout")}
                type="number"
                min={1}
                placeholder="60"
                value={f.timeout}
                onChange={(e) => set("timeout", e.target.value)}
              />
            </div>
          </div>
          {f.auth === "oauth" && (
            <div className="grid gap-4 md:grid-cols-2">
              <div className="flex flex-col gap-2">
                <Label htmlFor={id("client")}>Pre-registered client id (optional)</Label>
                <Input id={id("client")} className="font-mono" value={f.clientId} onChange={(e) => set("clientId", e.target.value)} />
              </div>
              <div className="flex flex-col gap-2">
                <Label htmlFor={id("scopes")}>Scopes (optional, space-separated)</Label>
                <Input id={id("scopes")} className="font-mono" value={f.scopes} onChange={(e) => set("scopes", e.target.value)} />
              </div>
            </div>
          )}
          <fieldset className="flex flex-col gap-2">
            <legend className="pb-2 font-medium">Headers</legend>
            <p className="text-muted-foreground text-xs">
              <Mono>{"{field}"}</Mono> is filled from the server's secret; set the values after saving. E.g. <Mono>Authorization</Mono> ={" "}
              <Mono>{"Bearer {token}"}</Mono>.
            </p>
            {f.headers.map((h, i) => (
              <div key={i} className="flex flex-wrap gap-2">
                <Input
                  aria-label="Header name"
                  className="w-48 font-mono"
                  placeholder="Authorization"
                  value={h.name}
                  onChange={(e) => set("headers", f.headers.map((x, j) => (j === i ? { ...x, name: e.target.value } : x)))}
                />
                <Input
                  aria-label="Header value"
                  className="min-w-64 flex-1 font-mono"
                  placeholder="Bearer {token}"
                  value={h.value}
                  onChange={(e) => set("headers", f.headers.map((x, j) => (j === i ? { ...x, value: e.target.value } : x)))}
                />
                <Button
                  type="button"
                  variant="ghost"
                  size="icon"
                  aria-label={`Remove header ${h.name}`}
                  onClick={() => set("headers", f.headers.filter((_, j) => j !== i))}
                >
                  <Trash2 />
                </Button>
              </div>
            ))}
            <Button type="button" variant="outline" size="sm" className="self-start" onClick={() => set("headers", [...f.headers, { name: "", value: "" }])}>
              <Plus /> Add header
            </Button>
          </fieldset>
          <div className="flex gap-2">
            <Button type="submit" disabled={!name || !f.url.trim() || save.isPending}>
              {isNew ? "Add server" : "Save"}
            </Button>
            <Button type="button" variant="ghost" onClick={onDone}>
              Cancel
            </Button>
          </div>
        </form>
      </CardContent>
    </Card>
  );
}

const REGISTRATIONS: Record<NonNullable<McpOAuthStatus["registration"]>, string> = {
  "pre-registered": "pre-registered client",
  "client-metadata-document": "client metadata document",
  dynamic: "dynamic registration",
};

function OAuthLogin({ server, oauth }: { server: string; oauth: McpOAuthStatus }) {
  const [flow, setFlow] = useState<McpLoginStart | null>(null);
  const start = useAction(() => post<McpLoginStart>(`/admin/mcp/servers/${server}/oauth/start`), {
    onSuccess: (f) => {
      setFlow(f);
      window.open(f.url, "_blank", "noopener,noreferrer");
    },
  });
  const signOut = useAction(() => del(`/admin/mcp/servers/${server}/oauth`), {
    invalidate: [["mcp-servers"]],
    success: `Signed out of ${server}`,
  });

  return (
    <div className="flex flex-col gap-3">
      <h3 className="font-medium">OAuth</h3>
      {oauth.needs_login ? (
        <p className="bg-warning-muted text-warning rounded-md px-3 py-2">Sign-in needed: {oauth.needs_login}.</p>
      ) : (
        <dl className="grid gap-x-6 gap-y-2 sm:grid-cols-[auto_1fr]">
          <dt className="text-muted-foreground">Signed in</dt>
          <dd>{oauth.signed_in_at ? dateTime(oauth.signed_in_at) : "yes"}</dd>
          <dt className="text-muted-foreground">Access token</dt>
          <dd>
            {oauth.expires_at ? `expires ${relative(oauth.expires_at)}` : "no expiry given"}
            {oauth.refreshable ? ", refreshed automatically" : ", no refresh token: sign in again when it expires"}
          </dd>
          {oauth.scopes.length > 0 && (
            <>
              <dt className="text-muted-foreground">Scopes</dt>
              <dd>
                <Mono>{oauth.scopes.join(" ")}</Mono>
              </dd>
            </>
          )}
        </dl>
      )}
      {(oauth.registration || oauth.issuer) && (
        <p className="text-muted-foreground text-xs">
          {oauth.registration && <>Client: {REGISTRATIONS[oauth.registration]}. </>}
          {oauth.issuer && (
            <>
              Authorization server <Mono>{oauth.issuer}</Mono>.
            </>
          )}
        </p>
      )}
      {oauth.unsaved && (
        <p className="bg-danger-muted text-destructive rounded-md px-3 py-2">
          The latest tokens could not be written to OpenBao; xolotl keeps retrying.
        </p>
      )}
      {flow ? (
        <LoginFlow flow={flow} onDone={() => setFlow(null)} />
      ) : (
        <div className="flex flex-wrap gap-2">
          <Button variant={oauth.needs_login ? "default" : "outline"} size="sm" disabled={start.isPending} onClick={() => start.mutate(undefined)}>
            <LogIn />
            {oauth.needs_login ? "Sign in" : "Sign in again"}
          </Button>
          {!oauth.needs_login && (
            <Confirm
              title={`Sign out of ${server}?`}
              description="xolotl forgets the tokens; tool calls fail until someone signs in again."
              action="Sign out"
              onConfirm={() => signOut.mutate(undefined)}
            >
              <Button variant="ghost" size="sm" disabled={signOut.isPending}>
                Sign out
              </Button>
            </Confirm>
          )}
        </div>
      )}
    </div>
  );
}

function LoginFlow({ flow, onDone }: { flow: McpLoginStart; onDone: () => void }) {
  const [pasted, setPasted] = useState("");
  const status = useQuery({
    queryKey: ["mcp-login", flow.flow_id],
    queryFn: () => get<McpLoginStatus>(`/admin/mcp/oauth/flows/${flow.flow_id}`),
    enabled: flow.callback,
    refetchInterval: (q) => (q.state.data?.status === "pending" || !q.state.data ? 2000 : false),
  });
  const complete = useAction(() => post(`/admin/mcp/oauth/flows/${flow.flow_id}/complete`, { redirect_url: pasted.trim() }), {
    invalidate: [["mcp-servers"], ["mcp-tools", flow.server], ["policy-view"]],
    success: `Signed in to ${flow.server}`,
    onSuccess: onDone,
  });
  const qc = useQueryClient();
  const state = status.data?.status;
  // Completed through xolotl's callback.
  useEffect(() => {
    if (state !== "complete") return;
    for (const key of [["mcp-servers"], ["mcp-tools", flow.server], ["policy-view"]]) qc.invalidateQueries({ queryKey: key });
    toast.success(`Signed in to ${flow.server}`);
    onDone();
  }, [state, qc, flow.server, onDone]);
  const id = `login-${flow.flow_id}`;

  return (
    <div className="bg-muted/40 flex flex-col gap-3 rounded-md border p-3">
      <p>
        Approve access in the tab that opened.{" "}
        <a href={flow.url} target="_blank" rel="noreferrer noopener" className="inline-flex items-center gap-1">
          Open the sign-in page again <ExternalLink className="size-3" />
        </a>
      </p>
      {flow.callback ? (
        status.data?.status === "failed" ? (
          <p className="text-destructive">{status.data.error}</p>
        ) : (
          <p className="text-muted-foreground">Waiting for the browser to come back to xolotl…</p>
        )
      ) : (
        <form
          className="flex flex-col gap-2"
          onSubmit={(e) => {
            e.preventDefault();
            complete.mutate(undefined);
          }}
        >
          <Label htmlFor={id}>
            <span>
              The browser then opens <Mono>{flow.redirect_uri}</Mono>, which doesn't load. Paste that page's full URL here.
            </span>
          </Label>
          <div className="flex max-w-2xl gap-2">
            <Input
              id={id}
              autoComplete="off"
              className="font-mono"
              placeholder={`${flow.redirect_uri}?code=…`}
              value={pasted}
              onChange={(e) => setPasted(e.target.value)}
            />
            <Button type="submit" disabled={!pasted.trim() || complete.isPending}>
              Finish
            </Button>
          </div>
        </form>
      )}
      <Button variant="ghost" size="sm" className="self-start" onClick={onDone}>
        Cancel
      </Button>
    </div>
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
                    <Badge variant={c.status === "cancelled" ? "warning" : "destructive"}>
                      {c.status === "tool_error" ? "tool error" : c.status === "cancelled" ? "cancelled" : "failed"}
                    </Badge>
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
