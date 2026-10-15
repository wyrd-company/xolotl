import { useEffect, useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { ArrowDown, ArrowUp, Plus, Trash2 } from "lucide-react";
import { Link, Navigate, useNavigate, useParams } from "react-router";

import { CatalogRefresh } from "@/components/catalog-refresh";
import { Confirm } from "@/components/confirm";
import { Endpoints, EndpointsLegend } from "@/components/endpoints";
import { Empty, Mono, PageHeader, QueryError } from "@/components/layout";
import { PolicyLimits } from "@/components/policy-limits";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Checkbox } from "@/components/ui/checkbox";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { Textarea } from "@/components/ui/textarea";
import {
  del,
  get,
  post,
  put,
  SUB_PROVIDERS,
  type GatewayProvider,
  type KeyInfo,
  type Limits,
  type McpServer,
  type Policy,
  type PolicyView,
  type Scope,
} from "@/lib/api";
import { useAction } from "@/lib/hooks";
import { cn } from "@/lib/utils";

/** Route segment for an unsaved policy; `+` can't appear in policy names. */
const NEW = "+new";
const SCOPES: [Scope, string][] = [
  ["proxy", "proxy"],
  ["management", "management (T3Code hubs)"],
  ["mcp", "mcp (MCP gateway)"],
  ["admin", "admin"],
];

export function PoliciesPage() {
  const { name } = useParams();
  const policies = useQuery({ queryKey: ["policies"], queryFn: () => get<Policy[]>("/admin/policies") });
  const keys = useQuery({ queryKey: ["keys"], queryFn: () => get<KeyInfo[]>("/admin/keys") });
  const list = policies.data ?? [];
  const keyCount = (p: string) => (keys.data ?? []).filter((k) => k.policy === p).length;

  if (!name && list.length) return <Navigate to={`/policies/${list[0].name}`} replace />;
  const current = name === NEW ? null : list.find((p) => p.name === name);

  return (
    <>
      <PageHeader
        title="Policies"
        description="Every client key belongs to a policy: which providers and models it may use, under what names, and which MCP tools."
      >
        <Button asChild>
          <Link to={`/policies/${NEW}`}>
            <Plus /> New policy
          </Link>
        </Button>
      </PageHeader>
      <QueryError error={policies.error} />
      <div className="flex flex-col gap-6 lg:flex-row lg:items-start">
        <aside aria-label="Policies" className="flex shrink-0 flex-col gap-1 lg:w-56">
          {list.map((p) => (
            <Link
              key={p.name}
              to={`/policies/${p.name}`}
              aria-current={p.name === name ? "page" : undefined}
              className={cn(
                "flex flex-col rounded-md border border-transparent px-3 py-2 text-left no-underline",
                p.name === name ? "bg-card border-border shadow-xs" : "hover:bg-card/60",
              )}
            >
              <span className="text-foreground font-mono font-medium">{p.name}</span>
              <span className="text-muted-foreground text-xs">
                {p.providers.includes("*") ? "all providers" : `${p.providers.length} provider${p.providers.length === 1 ? "" : "s"}`} ·{" "}
                {p.mcp_servers?.length ? `${p.mcp_servers.includes("*") ? "all" : p.mcp_servers.length} MCP · ` : ""}
                {keyCount(p.name)} key{keyCount(p.name) === 1 ? "" : "s"}
              </span>
            </Link>
          ))}
          {policies.isSuccess && list.length === 0 && name !== NEW && <p className="text-muted-foreground px-3 text-sm">No policies yet.</p>}
        </aside>
        <div className="min-w-0 flex-1">
          {name === NEW && <PolicyEditor key={NEW} initial={null} keyCount={0} />}
          {current && <PolicyEditor key={current.name} initial={current} keyCount={keyCount(current.name)} />}
          {name && name !== NEW && policies.isSuccess && !current && <Empty>No policy named {name}.</Empty>}
        </div>
      </div>
    </>
  );
}

interface Draft {
  name: string;
  description: string;
  providers: string[];
  scopes: Scope[];
  allow: string;
  deny: string;
  precedence: string[];
  winners: Record<string, string>;
  aliases: { name: string; targets: string }[];
  mcpServers: string[];
  allowTools: string;
  denyTools: string;
  limits: Limits;
}

function toDraft(p: Policy | null): Draft {
  return {
    name: p?.name ?? "",
    description: p?.description ?? "",
    providers: p?.providers ?? [],
    scopes: p?.scopes ?? ["proxy"],
    allow: (p?.allow_models ?? []).join("\n"),
    deny: (p?.deny_models ?? []).join("\n"),
    precedence: p?.precedence ?? [],
    winners: p?.winners ?? {},
    aliases: Object.entries(p?.aliases ?? {}).map(([name, t]) => ({ name, targets: (Array.isArray(t) ? t : [t]).join(", ") })),
    mcpServers: p?.mcp_servers ?? [],
    allowTools: (p?.allow_tools ?? []).join("\n"),
    denyTools: (p?.deny_tools ?? []).join("\n"),
    limits: p?.limits ?? {},
  };
}

function toPolicy(d: Draft): Policy {
  const lines = (s: string) =>
    s
      .split("\n")
      .map((l) => l.trim())
      .filter(Boolean);
  const aliases: Record<string, string | string[]> = {};
  for (const a of d.aliases) {
    const targets = a.targets
      .split(",")
      .map((t) => t.trim())
      .filter(Boolean);
    if (a.name.trim()) aliases[a.name.trim()] = targets.length === 1 ? targets[0] : targets;
  }
  return {
    name: d.name.trim(),
    description: d.description.trim() || undefined,
    providers: d.providers,
    scopes: d.scopes,
    allow_models: lines(d.allow),
    deny_models: lines(d.deny),
    precedence: d.precedence,
    winners: d.winners,
    aliases,
    mcp_servers: d.mcpServers,
    allow_tools: lines(d.allowTools),
    deny_tools: lines(d.denyTools),
    limits: d.limits,
  };
}

/** The server's default order: subscription pools, then gateway providers alphabetically. */
function defaultOrder(providers: string[]): string[] {
  const subs = SUB_PROVIDERS.filter((p) => providers.includes(p));
  return [...subs, ...providers.filter((p) => !(SUB_PROVIDERS as string[]).includes(p)).sort()];
}

function useDebounced<T>(value: T, ms: number): T {
  const [v, setV] = useState(value);
  useEffect(() => {
    const t = setTimeout(() => setV(value), ms);
    return () => clearTimeout(t);
  }, [value, ms]);
  return v;
}

function PolicyEditor({ initial, keyCount }: { initial: Policy | null; keyCount: number }) {
  const navigate = useNavigate();
  const isNew = initial === null;
  const [draft, setDraft] = useState<Draft>(() => toDraft(initial));
  const set = <K extends keyof Draft>(k: K, v: Draft[K]) => setDraft((d) => ({ ...d, [k]: v }));

  const gateway = useQuery({ queryKey: ["providers"], queryFn: () => get<GatewayProvider[]>("/admin/providers") });
  const known = [...SUB_PROVIDERS, ...(gateway.data ?? []).filter((p) => p.enabled).map((p) => p.name)];
  const all = draft.providers.includes("*");
  const allowed = all ? known : known.filter((p) => draft.providers.includes(p));
  // Precedence as shown: the saved order first, then the rest in default order.
  const order = [...draft.precedence.filter((p) => allowed.includes(p)), ...defaultOrder(allowed).filter((p) => !draft.precedence.includes(p))];

  const policy = useMemo(() => toPolicy(draft), [draft]);
  const debounced = useDebounced(policy, 400);
  const preview = useQuery({
    queryKey: ["policy-view", debounced],
    queryFn: () => post<PolicyView>("/admin/policy-preview", debounced),
    enabled: debounced.providers.length > 0 || (debounced.mcp_servers ?? []).length > 0,
    placeholderData: (prev) => prev,
    retry: false,
  });

  const dirty = JSON.stringify(policy) !== JSON.stringify(toPolicy(toDraft(initial)));
  const save = useAction(() => put<Policy>(`/admin/policies/${policy.name}`, policy), {
    invalidate: [["policies"]],
    success: `Saved policy ${policy.name}`,
    onSuccess: (p) => isNew && navigate(`/policies/${p.name}`),
  });
  const remove = useAction(() => del(`/admin/policies/${initial!.name}`), {
    invalidate: [["policies"]],
    success: `Deleted policy ${initial?.name}`,
    onSuccess: () => navigate("/policies"),
  });

  function toggleProvider(p: string, on: boolean) {
    set("providers", on ? [...draft.providers, p] : draft.providers.filter((x) => x !== p));
  }
  function move(i: number, by: number) {
    const next = [...order];
    [next[i], next[i + by]] = [next[i + by], next[i]];
    set("precedence", next);
  }

  return (
    <form
      className="flex flex-col gap-4"
      onSubmit={(e) => {
        e.preventDefault();
        save.mutate(undefined);
      }}
    >
      <Card>
        <CardContent className="flex flex-col gap-5">
          <div className="grid gap-4 md:grid-cols-[minmax(0,16rem)_1fr]">
            <div className="flex flex-col gap-2">
              <Label htmlFor="p-name">Name</Label>
              <Input
                id="p-name"
                className="font-mono"
                value={draft.name}
                disabled={!isNew}
                placeholder="devcontainer"
                onChange={(e) => set("name", e.target.value)}
              />
            </div>
            <div className="flex flex-col gap-2">
              <Label htmlFor="p-desc">Description</Label>
              <Input id="p-desc" value={draft.description} onChange={(e) => set("description", e.target.value)} />
            </div>
          </div>
          <fieldset className="flex flex-col gap-3">
            <legend className="pb-3 text-sm font-medium">Providers</legend>
            <div className="flex flex-wrap gap-x-6 gap-y-3">
              {known.map((p) => (
                <label key={p} className="flex items-center gap-2 text-sm">
                  <Checkbox checked={all || draft.providers.includes(p)} disabled={all} onCheckedChange={(c) => toggleProvider(p, c === true)} />
                  <span className="font-mono">{p}</span>
                </label>
              ))}
              <label className="flex items-center gap-2 text-sm">
                <Checkbox checked={all} onCheckedChange={(c) => set("providers", c === true ? ["*"] : [])} />
                all (<span className="font-mono">*</span>)
              </label>
            </div>
          </fieldset>
          <fieldset className="flex flex-col gap-3">
            <legend className="pb-3 text-sm font-medium">Scopes</legend>
            <div className="flex flex-wrap gap-x-6 gap-y-3">
              {SCOPES.map(([s, label]) => (
                <label key={s} className="flex items-center gap-2 text-sm">
                  <Checkbox
                    checked={draft.scopes.includes(s)}
                    onCheckedChange={(c) => set("scopes", c === true ? [...draft.scopes, s] : draft.scopes.filter((x) => x !== s))}
                  />
                  {label}
                </label>
              ))}
            </div>
          </fieldset>
          <div className="grid gap-4 md:grid-cols-2">
            <div className="flex flex-col gap-2">
              <Label htmlFor="p-allow">Allow models (globs, one per line; empty allows all)</Label>
              <Textarea id="p-allow" className="font-mono" rows={3} value={draft.allow} onChange={(e) => set("allow", e.target.value)} />
            </div>
            <div className="flex flex-col gap-2">
              <Label htmlFor="p-deny">Deny models (deny wins)</Label>
              <Textarea id="p-deny" className="font-mono" rows={3} value={draft.deny} onChange={(e) => set("deny", e.target.value)} />
            </div>
          </div>
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">Precedence</CardTitle>
          <CardDescription>
            Who serves a model name that several providers offer. The others drop the plain name; <Mono>provider/model</Mono> still reaches them.
            {draft.precedence.length === 0 ? " Using the default order." : null}
          </CardDescription>
        </CardHeader>
        <CardContent className="flex flex-col gap-4">
          <ol className="flex flex-col gap-1">
            {order.map((p, i) => (
              <li key={p} className="flex max-w-sm items-center gap-2 rounded-md border px-3 py-1.5">
                <span className="text-muted-foreground w-5 font-mono text-xs">{i + 1}</span>
                <span className="flex-1 font-mono text-sm">{p}</span>
                <Button type="button" size="icon" variant="ghost" aria-label={`Move ${p} up`} disabled={i === 0} onClick={() => move(i, -1)}>
                  <ArrowUp />
                </Button>
                <Button
                  type="button"
                  size="icon"
                  variant="ghost"
                  aria-label={`Move ${p} down`}
                  disabled={i === order.length - 1}
                  onClick={() => move(i, 1)}
                >
                  <ArrowDown />
                </Button>
              </li>
            ))}
          </ol>
          {draft.precedence.length > 0 && (
            <Button type="button" variant="ghost" size="sm" className="self-start" onClick={() => set("precedence", [])}>
              Reset to the default order
            </Button>
          )}
          <SharedNames
            shared={preview.data?.shared ?? []}
            winners={draft.winners}
            onChange={(model, winner) => {
              const w = { ...draft.winners };
              if (winner) w[model] = winner;
              else delete w[model];
              set("winners", w);
            }}
          />
        </CardContent>
      </Card>

      <McpAccess draft={draft} set={set} tools={preview.data?.tools ?? []} />

      <PolicyLimits limits={draft.limits} onChange={(l) => set("limits", l)} usage={isNew ? undefined : preview.data?.usage} />

      <Card>
        <CardHeader>
          <CardTitle className="text-base">Aliases</CardTitle>
          <CardDescription>Extra names, each for one or more comma-separated provider/model targets, tried in order on 429 or 5xx.</CardDescription>
        </CardHeader>
        <CardContent className="flex flex-col gap-2">
          {draft.aliases.map((a, i) => (
            <div key={i} className="flex flex-wrap gap-2">
              <Input
                aria-label="Alias name"
                className="w-48 font-mono"
                placeholder="fast"
                value={a.name}
                onChange={(e) => set("aliases", draft.aliases.map((x, j) => (j === i ? { ...x, name: e.target.value } : x)))}
              />
              <Input
                aria-label="Targets"
                className="min-w-64 flex-1 font-mono"
                placeholder="codex/gpt-6-codex-mini, vercel/openai/gpt-6-mini"
                value={a.targets}
                onChange={(e) => set("aliases", draft.aliases.map((x, j) => (j === i ? { ...x, targets: e.target.value } : x)))}
              />
              <Button
                type="button"
                variant="ghost"
                size="icon"
                aria-label={`Remove alias ${a.name}`}
                onClick={() => set("aliases", draft.aliases.filter((_, j) => j !== i))}
              >
                <Trash2 />
              </Button>
            </div>
          ))}
          <Button type="button" variant="outline" size="sm" className="self-start" onClick={() => set("aliases", [...draft.aliases, { name: "", targets: "" }])}>
            <Plus /> Add alias
          </Button>
        </CardContent>
      </Card>

      <Card className="pb-0">
        <CardHeader>
          <CardTitle className="text-base">What keys under this policy see</CardTitle>
          <CardDescription>
            Updates as you edit. <Mono>GET /v1/models</Mono> lists these names with the endpoints each is reachable on. Claude subscription accounts
            serve only <Mono>/v1/messages</Mono> and are never translated.
          </CardDescription>
        </CardHeader>
        <CardContent className="flex flex-col gap-3">
          <CatalogRefresh providers={allowed} />
          <EndpointsLegend />
          {preview.error && <p className="bg-danger-muted text-destructive rounded-md px-3 py-2 text-sm">{preview.error.message}</p>}
        </CardContent>
        <Table className="min-w-[720px]">
          <TableHeader>
            <TableRow>
              <TableHead>Name</TableHead>
              <TableHead>Routes to</TableHead>
              <TableHead>Endpoints</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody className={cn(preview.isFetching && "opacity-60")}>
            {(preview.data?.models ?? []).map((m) => (
              <TableRow key={m.name}>
                <TableCell className="font-mono text-xs">
                  {m.name}
                  {m.alias && (
                    <Badge variant="outline" className="ml-2 font-sans">
                      alias
                    </Badge>
                  )}
                </TableCell>
                <TableCell className="font-mono text-xs">{m.targets.join(", ")}</TableCell>
                <TableCell>
                  <Endpoints native={m.endpoints} translated={m.translated_endpoints} />
                </TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
        {draft.providers.length === 0 && <Empty>No providers: keys under this policy see no models.</Empty>}
        {preview.data?.models.length === 0 && draft.providers.length > 0 && <Empty>No models match this policy.</Empty>}
      </Card>

      <div className="flex flex-wrap items-center gap-3">
        <Button type="submit" disabled={!dirty || !policy.name || !!preview.error || save.isPending}>
          {isNew ? "Create policy" : "Save policy"}
        </Button>
        {dirty && !isNew && (
          <Button type="button" variant="ghost" onClick={() => setDraft(toDraft(initial))}>
            Discard changes
          </Button>
        )}
        {!isNew && (
          <Confirm
            title={`Delete policy ${initial.name}?`}
            description="This removes it from OpenBao."
            action="Delete"
            onConfirm={() => remove.mutate(undefined)}
          >
            <Button type="button" variant="outline" className="text-destructive" disabled={keyCount > 0 || remove.isPending}>
              Delete
            </Button>
          </Confirm>
        )}
        <span className="text-muted-foreground text-sm">
          {!isNew && keyCount > 0 ? "A policy with keys can't be deleted. " : ""}
          Saved to <Mono>policies/{policy.name || "…"}</Mono>.
        </span>
      </div>
    </form>
  );
}

function McpAccess({
  draft,
  set,
  tools,
}: {
  draft: Draft;
  set: <K extends keyof Draft>(k: K, v: Draft[K]) => void;
  tools: string[];
}) {
  const servers = useQuery({ queryKey: ["mcp-servers"], queryFn: () => get<McpServer[]>("/admin/mcp/servers") });
  const names = (servers.data ?? []).map((s) => s.name);
  const all = draft.mcpServers.includes("*");
  if (names.length === 0 && draft.mcpServers.length === 0) return null;
  const toggle = (name: string, on: boolean) => set("mcpServers", on ? [...draft.mcpServers, name] : draft.mcpServers.filter((x) => x !== name));

  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-base">MCP servers</CardTitle>
        <CardDescription>
          The upstream servers and tools keys may use through <Mono>/mcp</Mono>. Needs the <Mono>mcp</Mono> scope.
        </CardDescription>
      </CardHeader>
      <CardContent className="flex flex-col gap-5">
        <div className="flex flex-wrap gap-x-6 gap-y-3">
          {names.map((n) => (
            <label key={n} className="flex items-center gap-2 text-sm">
              <Checkbox checked={all || draft.mcpServers.includes(n)} disabled={all} onCheckedChange={(c) => toggle(n, c === true)} />
              <span className="font-mono">{n}</span>
            </label>
          ))}
          <label className="flex items-center gap-2 text-sm">
            <Checkbox checked={all} onCheckedChange={(c) => set("mcpServers", c === true ? ["*"] : [])} />
            all (<span className="font-mono">*</span>)
          </label>
        </div>
        <div className="grid gap-4 md:grid-cols-2">
          <div className="flex flex-col gap-2">
            <Label htmlFor="p-allow-tools">
              <span>
                Allow tools (globs on <Mono>tool</Mono> or <Mono>server/tool</Mono>; empty allows all)
              </span>
            </Label>
            <Textarea
              id="p-allow-tools"
              className="font-mono"
              rows={3}
              value={draft.allowTools}
              onChange={(e) => set("allowTools", e.target.value)}
            />
          </div>
          <div className="flex flex-col gap-2">
            <Label htmlFor="p-deny-tools">Deny tools (deny wins)</Label>
            <Textarea
              id="p-deny-tools"
              className="font-mono"
              rows={3}
              placeholder="delete_*"
              value={draft.denyTools}
              onChange={(e) => set("denyTools", e.target.value)}
            />
          </div>
        </div>
        {draft.mcpServers.length > 0 && (
          <div className="flex flex-col gap-2">
            <h3 className="text-sm font-semibold">Tools keys under this policy can call ({tools.length})</h3>
            {tools.length > 0 ? (
              <ul className="flex flex-wrap gap-1.5">
                {tools.map((t) => (
                  <li key={t}>
                    <Badge variant="outline" className="font-mono">
                      {t}
                    </Badge>
                  </li>
                ))}
              </ul>
            ) : (
              <p className="text-muted-foreground text-sm">None, or the servers' tool lists haven't been read yet.</p>
            )}
          </div>
        )}
      </CardContent>
    </Card>
  );
}

function SharedNames({
  shared,
  winners,
  onChange,
}: {
  shared: PolicyView["shared"];
  winners: Record<string, string>;
  onChange: (model: string, winner: string | null) => void;
}) {
  if (!shared.length) return null;
  const PRECEDENCE = "__precedence";
  return (
    <div className="flex flex-col gap-2">
      <h3 className="text-sm font-semibold">Shared model names</h3>
      <div className="overflow-hidden rounded-md border">
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Model</TableHead>
              <TableHead>Served by</TableHead>
              <TableHead>Winner</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {shared.map((s) => {
              const id = `winner-${s.model}`;
              return (
                <TableRow key={s.model}>
                  <TableCell className="font-mono text-xs">{s.model}</TableCell>
                  <TableCell className="font-mono text-xs">{s.providers.join(", ")}</TableCell>
                  <TableCell>
                    <Label htmlFor={id} className="sr-only">
                      Winner for {s.model}
                    </Label>
                    <Select value={winners[s.model] ?? PRECEDENCE} onValueChange={(v) => onChange(s.model, v === PRECEDENCE ? null : v)}>
                      <SelectTrigger id={id} className="h-9 w-64">
                        <SelectValue />
                      </SelectTrigger>
                      <SelectContent>
                        <SelectItem value={PRECEDENCE}>by precedence ({s.providers[0]})</SelectItem>
                        {s.providers.map((p) => (
                          <SelectItem key={p} value={p}>
                            {p}
                          </SelectItem>
                        ))}
                      </SelectContent>
                    </Select>
                  </TableCell>
                </TableRow>
              );
            })}
          </TableBody>
        </Table>
      </div>
    </div>
  );
}
