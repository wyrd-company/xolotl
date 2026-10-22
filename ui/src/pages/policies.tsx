// ---
// relationships:
//   implements: web-ui
// ---
import { useEffect, useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Plus } from "lucide-react";
import { Link, Navigate, useNavigate, useParams } from "react-router";

import { Confirm } from "@/components/confirm";
import { Empty, Mono, PageHeader, QueryError } from "@/components/layout";
import { PolicyLimits } from "@/components/policy-limits";
import { SaveToolbar } from "@/components/save-toolbar";
import { Section, SectionContent } from "@/components/section";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Textarea } from "@/components/ui/textarea";
import {
  del,
  get,
  post,
  put,
  SUB_PROVIDERS,
  type Account,
  type GatewayProvider,
  type KeyInfo,
  type Limits,
  type McpServer,
  type Policy,
  type PolicyView,
  type Scope,
} from "@/lib/api";
import { useAction } from "@/lib/hooks";
import { accountProvider, allowsAccount, providerHealth } from "@/lib/policy-models";
import { cn } from "@/lib/utils";

import { lines, ModelList } from "./policy-model-list";
import { ProviderAccess } from "./policy-providers";

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

export interface Draft {
  name: string;
  description: string;
  providers: string[];
  /** Subscription provider -> account ids or `*`; no entry: no accounts. */
  accounts: Record<string, string[]>;
  scopes: Scope[];
  /** Allow and deny rules in order. `id` keeps a row's identity while editing. */
  rules: { id: string; pattern: string; allow: boolean }[];
  /** `provider/model` -> on or off. */
  models: Record<string, boolean>;
  /** Deny models that no rule matches and that have no switch. */
  denyOther: boolean;
  precedence: string[];
  winners: Record<string, string>;
  aliases: { id: string; name: string; targets: string[] }[];
  renames: Record<string, string>;
  mcpServers: string[];
  allowTools: string;
  denyTools: string;
  limits: Limits;
}

export type SetDraft = <K extends keyof Draft>(k: K, v: Draft[K]) => void;

export const rowId = () => Math.random().toString(36).slice(2);

function toDraft(p: Policy | null): Draft {
  return {
    name: p?.name ?? "",
    description: p?.description ?? "",
    providers: p?.providers ?? [],
    accounts: { ...p?.accounts },
    scopes: p?.scopes ?? ["proxy"],
    rules: (p?.rules ?? []).map((r) => ("allow" in r ? { id: rowId(), pattern: r.allow, allow: true } : { id: rowId(), pattern: r.deny, allow: false })),
    models: p?.models ?? {},
    denyOther: p?.deny_other_models ?? false,
    precedence: p?.precedence ?? [],
    winners: p?.winners ?? {},
    aliases: Object.entries(p?.aliases ?? {}).map(([name, t]) => ({ id: rowId(), name, targets: Array.isArray(t) ? t : [t] })),
    renames: p?.renames ?? {},
    mcpServers: p?.mcp_servers ?? [],
    allowTools: (p?.allow_tools ?? []).join("\n"),
    denyTools: (p?.deny_tools ?? []).join("\n"),
    limits: p?.limits ?? {},
  };
}

function toPolicy(d: Draft): Policy {
  const aliases: Record<string, string | string[]> = {};
  for (const a of d.aliases) {
    const targets = a.targets.map((t) => t.trim()).filter(Boolean);
    if (a.name.trim()) aliases[a.name.trim()] = targets.length === 1 ? targets[0] : targets;
  }
  // A blank name, or the plain name itself, is no rename.
  const renames = Object.fromEntries(
    Object.entries(d.renames)
      .map(([t, n]) => [t, n.trim()])
      .filter(([t, n]) => n && n !== t.slice(t.indexOf("/") + 1)),
  );
  const all = d.providers.includes("*");
  const allows = (p: string) => all || d.providers.includes(p);
  // Limits of providers and accounts the policy no longer uses go with them.
  const usesAccount = (id: string) => {
    const p = accountProvider(id, SUB_PROVIDERS);
    return !!p && allows(p) && allowsAccount(d.accounts, p, id);
  };
  const limits: Limits = { ...d.limits };
  const keep = <T,>(m: Record<string, T> | undefined, ok: (k: string) => boolean) => {
    const kept = Object.fromEntries(Object.entries(m ?? {}).filter(([k]) => ok(k)));
    return Object.keys(kept).length ? kept : undefined;
  };
  limits.providers = keep(d.limits.providers, allows);
  limits.accounts = keep(d.limits.accounts, usesAccount);
  if (!limits.providers) delete limits.providers;
  if (!limits.accounts) delete limits.accounts;
  return {
    name: d.name.trim(),
    description: d.description.trim() || undefined,
    providers: d.providers,
    accounts: Object.fromEntries(Object.entries(d.accounts).filter(([p]) => allows(p))),
    scopes: d.scopes,
    rules: d.rules.filter((r) => r.pattern.trim()).map((r) => (r.allow ? { allow: r.pattern.trim() } : { deny: r.pattern.trim() })),
    models: d.models,
    deny_other_models: d.denyOther || undefined,
    precedence: d.precedence,
    winners: d.winners,
    aliases,
    renames,
    mcp_servers: d.mcpServers,
    allow_tools: lines(d.allowTools),
    deny_tools: lines(d.denyTools),
    limits,
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
  const set: SetDraft = (k, v) => setDraft((d) => ({ ...d, [k]: v }));

  const gateway = useQuery({ queryKey: ["providers"], queryFn: () => get<GatewayProvider[]>("/admin/providers") });
  const accounts = useQuery({ queryKey: ["accounts"], queryFn: () => get<Account[]>("/admin/accounts"), refetchInterval: 30_000 });
  const known = [...SUB_PROVIDERS, ...(gateway.data ?? []).filter((p) => p.enabled).map((p) => p.name)];
  const health = (p: string) => providerHealth(p, SUB_PROVIDERS, accounts.data ?? [], gateway.data ?? [], draft.accounts);
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
      <SaveToolbar label="Policy actions">
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
        <span className="text-muted-foreground min-w-0 text-sm [overflow-wrap:anywhere]">
          {!isNew && keyCount > 0 ? "A policy with keys can't be deleted. " : ""}
          Saved to <Mono>policies/{policy.name || "…"}</Mono>.
        </span>
      </SaveToolbar>

      <Section id="policy-general" title="Policy" summary={draft.description || draft.scopes.join(", ")}>
        <SectionContent className="flex flex-col gap-5">
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
        </SectionContent>
      </Section>

      <ProviderAccess draft={draft} set={set} known={known} accounts={accounts.data ?? []} health={health} ready={accounts.isSuccess} />

      <ModelList
        allowedProviders={allowed}
        draft={draft}
        set={set}
        view={preview.data}
        fetching={preview.isFetching}
        error={preview.error}
        health={health}
        precedence={{ order, move, reset: draft.precedence.length > 0 ? () => set("precedence", []) : undefined }}
      />

      <McpAccess draft={draft} set={set} tools={preview.data?.tools ?? []} />

      <PolicyLimits
        limits={draft.limits}
        onChange={(l) => set("limits", l)}
        view={isNew ? undefined : preview.data}
        providers={allowed}
        accounts={(accounts.data ?? []).filter((a) => allowed.includes(a.provider) && allowsAccount(draft.accounts, a.provider, a.id))}
      />
    </form>
  );
}

function McpAccess({
  draft,
  set,
  tools,
}: {
  draft: Draft;
  set: SetDraft;
  tools: string[];
}) {
  const servers = useQuery({ queryKey: ["mcp-servers"], queryFn: () => get<McpServer[]>("/admin/mcp/servers") });
  const names = (servers.data ?? []).map((s) => s.name);
  const all = draft.mcpServers.includes("*");
  if (names.length === 0 && draft.mcpServers.length === 0) return null;
  const toggle = (name: string, on: boolean) => set("mcpServers", on ? [...draft.mcpServers, name] : draft.mcpServers.filter((x) => x !== name));

  return (
    <Section
      id="policy-mcp"
      title="MCP servers"
      summary={all ? "all servers" : `${draft.mcpServers.length} server${draft.mcpServers.length === 1 ? "" : "s"}`}
      description={
        <>
          The upstream servers and tools keys may use through <Mono>/mcp</Mono>. Needs the <Mono>mcp</Mono> scope.
        </>
      }
    >
      <SectionContent className="flex flex-col gap-5">
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
      </SectionContent>
    </Section>
  );
}
