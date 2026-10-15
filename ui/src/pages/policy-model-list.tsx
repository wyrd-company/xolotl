import { useMemo, useState } from "react";
import { ChevronDown, ChevronRight, Plus, Trash2 } from "lucide-react";

import { CatalogRefresh } from "@/components/catalog-refresh";
import { Empty, Mono } from "@/components/layout";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Checkbox } from "@/components/ui/checkbox";
import { Input } from "@/components/ui/input";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import type { PolicyView } from "@/lib/api";
import {
  aliasesReaching,
  qualified,
  rulesAllow,
  splitTargets,
  withoutTarget,
  setAllowed,
  sortRows,
  usable,
  type Candidate,
  type Health,
  type SortBy,
} from "@/lib/policy-models";
import { cn } from "@/lib/utils";

import type { Draft, SetDraft } from "./policies";

const ALL = "__all";
const SORTS: [SortBy, string][] = [
  ["allowed", "Allowed first"],
  ["name", "Name"],
  ["provider", "Provider"],
];

export function HealthBadge({ health }: { health: Health }) {
  if (health.kind === "pool") {
    const { total, available } = health;
    if (total === 0) return <Badge variant="secondary">no accounts</Badge>;
    if (available === 0) return <Badge variant="warning">0 of {total} available</Badge>;
    return (
      <Badge variant="success">{available === total ? `${total} account${total === 1 ? "" : "s"}` : `${available} of ${total} available`}</Badge>
    );
  }
  if (health.kind === "gateway" && !health.ready) return <Badge variant="warning">no key</Badge>;
  return null;
}

/** The targets after the first, as typed. */
function fallbacks(targets: string): string {
  const i = targets.indexOf(",");
  return i < 0 ? "" : targets.slice(i + 1).replace(/^ /, "");
}

/** The list of models keys under the policy may use: toggles, names, extra names. */
export function ModelList({
  allowedProviders,
  draft,
  set,
  view,
  fetching,
  error,
  health,
}: {
  allowedProviders: string[];
  draft: Draft;
  set: SetDraft;
  view: PolicyView | undefined;
  fetching: boolean;
  error: Error | null;
  health: (provider: string) => Health;
}) {
  const [provider, setProvider] = useState(ALL);
  const [query, setQuery] = useState("");
  const [sortBy, setSortBy] = useState<SortBy>("allowed");
  const [open, setOpen] = useState<Set<string>>(new Set());

  const rules = { allow: lines(draft.allow), deny: lines(draft.deny) };
  // Allowed as edited, before the debounced preview catches up.
  const rows: Candidate[] = (view?.catalog ?? []).map((c) => ({ ...c, allowed: rulesAllow(rules, c) }));
  const byName = new Map(rows.map((c) => [qualified(c), c]));
  // The order is kept while toggling, so rows don't move under the pointer:
  // it is sorted again only when the rows or the sort change.
  const rowsKey = rows.map(qualified).join("\n");
  const order = useMemo(() => sortRows(rows, sortBy).map(qualified), [rowsKey, sortBy]);
  const q = query.trim().toLowerCase();
  const shown = order
    .map((k) => byName.get(k))
    .filter((c): c is Candidate => !!c)
    .filter((c) => provider === ALL || c.provider === provider)
    .filter((c) => !q || [qualified(c), draft.renames[qualified(c)] ?? ""].some((s) => s.toLowerCase().includes(q)));
  const providers = [...new Set(rows.map((c) => c.provider))];
  const allowedCount = rows.filter((c) => c.allowed).length;

  function toggle(cs: Candidate[], on: boolean) {
    const next = setAllowed(rules, cs, on);
    set("allow", next.allow.join("\n"));
    set("deny", next.deny.join("\n"));
  }
  function rename(target: string, name: string) {
    const r = { ...draft.renames };
    if (name) r[target] = name;
    else delete r[target];
    set("renames", r);
  }
  function setWinner(model: string, winner: string | null) {
    const w = { ...draft.winners };
    if (winner) w[model] = winner;
    else delete w[model];
    set("winners", w);
  }
  const setAlias = (i: number, a: Draft["aliases"][number] | null) =>
    set("aliases", a ? draft.aliases.map((x, j) => (j === i ? a : x)) : draft.aliases.filter((_, j) => j !== i));
  const addAlias = (targets: string) => set("aliases", [...draft.aliases, { name: "", targets }]);
  const flip = (k: string) =>
    setOpen((s) => {
      const n = new Set(s);
      if (!n.delete(k)) n.add(k);
      return n;
    });

  // Extra names belong to the row of their first target.
  const aliasesOf = (target: string) => draft.aliases.map((a, i) => ({ a, i })).filter(({ a }) => splitTargets(a.targets)[0] === target);
  const other = draft.aliases.map((a, i) => ({ a, i })).filter(({ a }) => !byName.has(splitTargets(a.targets)[0] ?? ""));

  return (
    <Card className="pb-0">
      <CardHeader>
        <CardTitle className="text-base">Models</CardTitle>
        <CardDescription>
          What keys under this policy see{view ? ` (${allowedCount} of ${rows.length} allowed)` : ""}. Turn models on or off and rename them;{" "}
          <Mono>provider/model</Mono> always reaches an allowed model.
        </CardDescription>
      </CardHeader>
      <CardContent className="flex flex-col gap-3">
        <CatalogRefresh providers={provider === ALL ? allowedProviders : allowedProviders.filter((p) => p === provider)} />
        <div className="flex flex-wrap items-center gap-2">
          <Input aria-label="Filter models" placeholder="Filter" className="h-9 w-56" value={query} onChange={(e) => setQuery(e.target.value)} />
          <Select value={provider} onValueChange={setProvider}>
            <SelectTrigger aria-label="Provider" className="h-9 w-48">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value={ALL}>All providers</SelectItem>
              {providers.map((p) => (
                <SelectItem key={p} value={p}>
                  {p}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          <Select value={sortBy} onValueChange={(v) => setSortBy(v as SortBy)}>
            <SelectTrigger aria-label="Sort by" className="h-9 w-40">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {SORTS.map(([v, label]) => (
                <SelectItem key={v} value={v}>
                  {label}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          <div className="grow" />
          <Button type="button" variant="outline" size="sm" disabled={!shown.length} onClick={() => toggle(shown, true)}>
            Allow all{shown.length < rows.length ? ` ${shown.length} shown` : ""}
          </Button>
          <Button type="button" variant="outline" size="sm" disabled={!shown.length} onClick={() => toggle(shown, false)}>
            Deny all{shown.length < rows.length ? ` ${shown.length} shown` : ""}
          </Button>
        </div>
        {error && <p className="bg-danger-muted text-destructive rounded-md px-3 py-2 text-sm">{error.message}</p>}
      </CardContent>
      <Table className="min-w-[720px]">
        <TableHeader>
          <TableRow>
            <TableHead className="w-16">Allowed</TableHead>
            <TableHead>Name</TableHead>
            <TableHead>Routes to</TableHead>
            <TableHead className="w-12">
              <span className="sr-only">More names</span>
            </TableHead>
          </TableRow>
        </TableHeader>
        <TableBody className={cn(fetching && "opacity-60")}>
          {shown.map((c) => {
            const k = qualified(c);
            const h = health(c.provider);
            const mine = aliasesOf(k);
            const reaching = aliasesReaching(draft.aliases, k);
            const asFallback = reaching.filter((r) => r.fallback);
            const expanded = open.has(k);
            const shared = view?.shared.find((s) => s.model === c.model && s.providers.includes(c.provider));
            return [
              <TableRow key={k} className={cn(!c.allowed && "text-muted-foreground")}>
                <TableCell>
                  <Checkbox checked={c.allowed} aria-label={`Allow ${k}`} onCheckedChange={(on) => toggle([c], on === true)} />
                </TableCell>
                <TableCell>
                  <div className="flex flex-col gap-1">
                    <Input
                      aria-label={`Name for ${k}`}
                      className="h-8 w-64 font-mono text-xs"
                      placeholder={c.model}
                      value={draft.renames[k] ?? ""}
                      onChange={(e) => rename(k, e.target.value)}
                    />
                    <span className="text-muted-foreground flex flex-wrap items-center gap-x-2 gap-y-1 text-xs">
                      <span className="font-mono">{k}</span>
                      <RowNote candidate={c} target={k} shared={shared} winners={draft.winners} renamed={!!draft.renames[k]} setWinner={setWinner} />
                    </span>
                    {!c.allowed && reaching.length > 0 && (
                      <span className="text-warning text-xs">
                        Still reachable as {reaching.map((r, j) => [j > 0 && ", ", <Mono key={r.index}>{r.name || "…"}</Mono>])}: more names are
                        explicit grants. Remove them to deny it.
                      </span>
                    )}
                    {reaching.length > 0 && (
                      <span className="flex flex-wrap gap-1">
                        {mine.map(({ a, i }) => (
                          <Badge key={i} variant="outline" className="font-mono">
                            {a.name || "…"}
                            {splitTargets(a.targets).length > 1 && (
                              <span className="text-muted-foreground font-sans">+{splitTargets(a.targets).length - 1} fallback</span>
                            )}
                          </Badge>
                        ))}
                        {asFallback.map((r) => (
                          <Badge key={r.index} variant="outline" className="font-mono">
                            {r.name || "…"}
                            <span className="text-muted-foreground font-sans">as fallback</span>
                          </Badge>
                        ))}
                      </span>
                    )}
                  </div>
                </TableCell>
                <TableCell>
                  <span className={cn("flex items-center gap-2", !usable(h) && "text-muted-foreground")}>
                    <span className="font-mono text-xs">{c.provider}</span>
                    <HealthBadge health={h} />
                  </span>
                </TableCell>
                <TableCell>
                  <Button
                    type="button"
                    size="icon"
                    variant="ghost"
                    aria-label={`More names for ${k}`}
                    aria-expanded={expanded}
                    onClick={() => flip(k)}
                  >
                    {expanded ? <ChevronDown /> : <ChevronRight />}
                  </Button>
                </TableCell>
              </TableRow>,
              expanded && (
                <TableRow key={`${k}-names`} className="hover:bg-transparent">
                  <TableCell />
                  <TableCell colSpan={3}>
                    <div className="flex flex-col gap-2 pb-2">
                      <p className="text-muted-foreground text-xs">
                        More names for <Mono>{k}</Mono>, each with fallbacks tried in order on 429 or 5xx.
                      </p>
                      {mine.map(({ a, i }) => (
                        <AliasRow
                          key={i}
                          name={a.name}
                          targets={fallbacks(a.targets)}
                          targetsLabel="Fallbacks"
                          targetsPlaceholder="codex/gpt-6-codex-mini"
                          onChange={(name, rest) => setAlias(i, { name, targets: rest.trim() ? `${k}, ${rest}` : k })}
                          onRemove={() => setAlias(i, null)}
                        />
                      ))}
                      {asFallback.map((r) => (
                        <div key={r.index} className="flex flex-wrap items-center gap-2 text-sm">
                          <Mono>{r.name || "…"}</Mono> tries this model as a fallback.
                          <Button
                            type="button"
                            variant="ghost"
                            size="sm"
                            onClick={() => setAlias(r.index, { name: r.name, targets: withoutTarget(draft.aliases[r.index].targets, k) })}
                          >
                            Remove from its fallbacks
                          </Button>
                        </div>
                      ))}
                      <Button type="button" variant="outline" size="sm" className="self-start" onClick={() => addAlias(k)}>
                        <Plus /> Add a name
                      </Button>
                    </div>
                  </TableCell>
                </TableRow>
              ),
            ];
          })}
        </TableBody>
      </Table>
      {draft.providers.length === 0 && <Empty>No providers: keys under this policy see no models.</Empty>}
      {view && rows.length === 0 && draft.providers.length > 0 && <Empty>The allowed providers have no models in the catalog.</Empty>}
      {rows.length > 0 && shown.length === 0 && <Empty>No models match the filter.</Empty>}
      <CardContent className="flex flex-col gap-2 border-t pt-4 pb-6">
        <h3 className="text-sm font-semibold">Other names</h3>
        <p className="text-muted-foreground text-sm">
          Names for targets not in the list above: comma-separated provider/model targets, tried in order.
        </p>
        {other.map(({ a, i }) => (
          <AliasRow
            key={i}
            name={a.name}
            targets={a.targets}
            targetsLabel="Targets"
            targetsPlaceholder="codex/gpt-6-codex-mini, vercel/openai/gpt-6-mini"
            onChange={(name, targets) => setAlias(i, { name, targets })}
            onRemove={() => setAlias(i, null)}
          />
        ))}
        <Button type="button" variant="outline" size="sm" className="self-start" onClick={() => addAlias("")}>
          <Plus /> Add a name
        </Button>
      </CardContent>
    </Card>
  );
}

/** Why a row is denied, or who serves its plain name. */
function RowNote({
  candidate: c,
  target,
  shared,
  winners,
  renamed,
  setWinner,
}: {
  candidate: Candidate;
  target: string;
  shared: PolicyView["shared"][number] | undefined;
  winners: Record<string, string>;
  renamed: boolean;
  setWinner: (model: string, winner: string | null) => void;
}) {
  if (!c.allowed) {
    return c.rule && c.rule !== target ? (
      <span>
        denied by <Mono>{c.rule}</Mono> (model patterns)
      </span>
    ) : null;
  }
  if (c.taken_by) {
    return (
      <span>
        <Mono>{c.model}</Mono> is taken by {c.taken_by.kind === "alias" ? "an alias of that name" : <Mono>{c.taken_by.target}</Mono>}
        {c.taken_by.kind === "rename" ? " (renamed)" : ""}
      </span>
    );
  }
  if (renamed || !shared) return null;
  if (shared.winner !== c.provider) {
    return (
      <span className="flex items-center gap-1">
        <Mono>{c.model}</Mono> goes to {shared.winner}
        <Button type="button" variant="link" size="sm" className="h-auto p-0 text-xs" onClick={() => setWinner(c.model, c.provider)}>
          Serve it from here
        </Button>
      </span>
    );
  }
  const others = shared.providers.filter((p) => p !== c.provider).join(", ");
  return (
    <span className="flex items-center gap-1">
      also on {others}
      {winners[c.model] === c.provider && (
        <Button type="button" variant="link" size="sm" className="h-auto p-0 text-xs" onClick={() => setWinner(c.model, null)}>
          Use precedence
        </Button>
      )}
    </span>
  );
}

function AliasRow({
  name,
  targets,
  targetsLabel,
  targetsPlaceholder,
  onChange,
  onRemove,
}: {
  name: string;
  targets: string;
  targetsLabel: string;
  targetsPlaceholder: string;
  onChange: (name: string, targets: string) => void;
  onRemove: () => void;
}) {
  return (
    <div className="flex flex-wrap gap-2">
      <Input
        aria-label="Name"
        className="h-8 w-48 font-mono text-xs"
        placeholder="fast"
        value={name}
        onChange={(e) => onChange(e.target.value, targets)}
      />
      <Input
        aria-label={targetsLabel}
        className="h-8 min-w-64 flex-1 font-mono text-xs"
        placeholder={targetsPlaceholder}
        value={targets}
        onChange={(e) => onChange(name, e.target.value)}
      />
      <Button type="button" variant="ghost" size="icon" aria-label={`Remove name ${name}`} onClick={onRemove}>
        <Trash2 />
      </Button>
    </div>
  );
}

export const lines = (s: string) =>
  s
    .split("\n")
    .map((l) => l.trim())
    .filter(Boolean);
