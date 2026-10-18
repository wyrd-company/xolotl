import { useMemo, useState } from "react";
import { ArrowDown, ArrowLeft, ArrowRight, ArrowUp, Plus, Trash2 } from "lucide-react";

import { CatalogRefresh } from "@/components/catalog-refresh";
import { Combobox } from "@/components/combobox";
import { Empty, Mono } from "@/components/layout";
import { Section, SectionContent } from "@/components/section";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import type { PolicyView } from "@/lib/api";
import {
  aliasesReaching,
  decide,
  isOverride,
  matching,
  move,
  qualified,
  setToggles,
  sortRows,
  switchOf,
  usable,
  type Candidate,
  type Decision,
  type Health,
  type ModelRules,
  type SortBy,
} from "@/lib/policy-models";
import { cn } from "@/lib/utils";

import { rowId, type Draft, type SetDraft } from "./policies";

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

/** The provider order for shared names, as shown, and how to change it. */
export interface Precedence {
  order: string[];
  move: (index: number, by: number) => void;
  /** Present when the policy sets its own order. */
  reset?: () => void;
}

const SHOW_DENIED = "policy-models:show-denied";
const MODE = "policy-models:mode";
type Mode = "rules" | "models";

type RuleRow = Draft["rules"][number];
type AliasDraft = Draft["aliases"][number];

/**
 * What keys under the policy may use, in two modes: the rules as written
 * (in the order they apply) and the effective model list. The first rule
 * that matches a model decides; else its switch; else the default. Aliases
 * reach their targets whatever the rules say.
 */
export function ModelList({
  allowedProviders,
  draft,
  set,
  view,
  fetching,
  error,
  health,
  precedence,
}: {
  allowedProviders: string[];
  draft: Draft;
  set: SetDraft;
  view: PolicyView | undefined;
  fetching: boolean;
  error: Error | null;
  health: (provider: string) => Health;
  precedence: Precedence;
}) {
  const [mode, setModeState] = useState<Mode>(() => (localStorage.getItem(MODE) === "rules" ? "rules" : "models"));
  const setMode = (m: Mode) => {
    localStorage.setItem(MODE, m);
    setModeState(m);
  };
  const [provider, setProvider] = useState(ALL);
  const [query, setQuery] = useState("");
  const [sortBy, setSortBy] = useState<SortBy>("allowed");
  const [showDenied, setShowDeniedState] = useState(() => localStorage.getItem(SHOW_DENIED) !== "0");
  const setShowDenied = (on: boolean) => {
    localStorage.setItem(SHOW_DENIED, on ? "1" : "0");
    setShowDeniedState(on);
  };

  const rules: ModelRules = { rules: draft.rules, models: draft.models, denyOther: draft.denyOther };
  // Decided as edited, before the debounced preview catches up.
  const decided = new Map<string, Decision>();
  const rows: Candidate[] = (view?.catalog ?? []).map((c) => {
    const d = decide(rules, c);
    decided.set(qualified(c), d);
    return { ...c, allowed: d.allowed };
  });
  const byName = new Map(rows.map((c) => [qualified(c), c]));
  // The order is kept while toggling, so rows don't move under the pointer:
  // it is sorted again only when the rows or the sort change.
  const rowsKey = rows.map(qualified).join("\n");
  const order = useMemo(() => sortRows(rows, sortBy).map(qualified), [rowsKey, sortBy]);
  const q = query.trim().toLowerCase();
  const hit = (...texts: string[]) => !q || texts.some((s) => s.toLowerCase().includes(q));
  const shown = order
    .map((k) => byName.get(k))
    .filter((c): c is Candidate => !!c)
    .filter((c) => provider === ALL || c.provider === provider)
    .filter((c) => showDenied || c.allowed)
    .filter((c) => hit(qualified(c), draft.renames[qualified(c)] ?? ""));
  const aliases = draft.aliases.filter((a) => hit(a.name, ...a.targets));
  const providers = [...new Set(rows.map((c) => c.provider))];
  const allowedCount = rows.filter((c) => c.allowed).length;
  const targets = rows.map(qualified);

  const setRules = (next: RuleRow[]) => set("rules", next);
  const setRule = (id: string, r: RuleRow | null) => setRules(r ? draft.rules.map((x) => (x.id === id ? r : x)) : draft.rules.filter((x) => x.id !== id));
  const addRule = (allow: boolean, pattern = "") => setRules([{ id: rowId(), pattern, allow }, ...draft.rules]);
  const setAlias = (id: string, a: AliasDraft | null) =>
    set("aliases", a ? draft.aliases.map((x) => (x.id === id ? a : x)) : draft.aliases.filter((x) => x.id !== id));
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
  // Only switches no rule masks.
  const switchable = shown.filter((c) => !decided.get(qualified(c))?.rule);
  const filtered = shown.length < rows.length;
  const switches = Object.values(draft.models);
  const on = switches.filter(Boolean).length;

  return (
    <Section
      id="policy-models"
      className="pb-0"
      title="Models"
      summary={view ? `${allowedCount} of ${rows.length} allowed` : undefined}
      description={
        <>
          What keys under this policy see{view ? ` (${allowedCount} of ${rows.length} allowed)` : ""}. The first rule that matches a model decides,
          else its switch, else the default. <Mono>provider/model</Mono> always reaches an allowed model.
        </>
      }
    >
      <SectionContent className="flex flex-col gap-3">
        <PrecedenceRow precedence={precedence} />
        <CatalogRefresh providers={provider === ALL ? allowedProviders : allowedProviders.filter((p) => p === provider)} />
        <div className="flex flex-wrap items-center gap-2">
          <Tabs value={mode} onValueChange={(v) => setMode(v as Mode)}>
            <TabsList aria-label="Show">
              <TabsTrigger value="rules">Rules</TabsTrigger>
              <TabsTrigger value="models">Effective models</TabsTrigger>
            </TabsList>
          </Tabs>
          <Input aria-label="Filter" placeholder="Filter" className="h-9 w-56" value={query} onChange={(e) => setQuery(e.target.value)} />
          {mode === "models" && (
            <>
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
              <label className="flex items-center gap-2 px-1 text-sm">
                <Switch checked={showDenied} onCheckedChange={setShowDenied} />
                Show denied
              </label>
            </>
          )}
        </div>
        {mode === "rules" ? (
          <div className="flex flex-wrap items-center gap-2">
            <Button type="button" variant="outline" size="sm" onClick={() => addRule(true)}>
              <Plus /> Allow rule
            </Button>
            <Button type="button" variant="outline" size="sm" onClick={() => addRule(false)}>
              <Plus /> Deny rule
            </Button>
            <Button type="button" variant="outline" size="sm" onClick={() => set("aliases", [{ id: rowId(), name: "", targets: [""] }, ...draft.aliases])}>
              <Plus /> Alias
            </Button>
          </div>
        ) : (
          <div className="flex flex-wrap items-center gap-2">
            <div className="grow" />
            <Button type="button" variant="ghost" size="sm" disabled={!switchable.length} onClick={() => set("models", setToggles(draft.models, switchable, true))}>
              Turn on {filtered ? `${switchable.length} shown` : "all"}
            </Button>
            <Button type="button" variant="ghost" size="sm" disabled={!switchable.length} onClick={() => set("models", setToggles(draft.models, switchable, false))}>
              Turn off {filtered ? `${switchable.length} shown` : "all"}
            </Button>
          </div>
        )}
        {error && <p className="bg-danger-muted text-destructive rounded-md px-3 py-2 text-sm">{error.message}</p>}
      </SectionContent>
      <Table className="min-w-[720px]">
        <TableHeader>
          <TableRow>
            <TableHead className="w-16">{mode === "rules" ? "Allow" : "On"}</TableHead>
            <TableHead>Name</TableHead>
            <TableHead>{mode === "rules" ? "Matches" : "Routes to"}</TableHead>
            <TableHead className="w-28">
              <span className="sr-only">Actions</span>
            </TableHead>
          </TableRow>
        </TableHeader>
        <TableBody className={cn(fetching && "opacity-60")}>
          {mode === "rules" ? (
            <>
              {draft.rules.map((r, i) =>
                hit(r.pattern) ? (
                  <RuleTableRow
                    key={r.id}
                    rule={r}
                    index={i}
                    last={i === draft.rules.length - 1}
                    matched={matching(r.pattern.trim(), rows).length}
                    decides={rows.filter((c) => decided.get(qualified(c))?.rule?.index === i).length}
                    onChange={(next) => setRule(r.id, next)}
                    onMove={(by) => setRules(move(draft.rules, i, by))}
                  />
                ) : null,
              )}
              <TableRow className="bg-muted/30">
                <TableCell />
                <TableCell>
                  <div className="flex flex-col gap-1">
                    <span className="text-sm font-medium">Model switches</span>
                    <span className="text-muted-foreground text-xs">
                      A model no rule matches follows its own switch. {switches.length === 0 ? "None set." : `${on} on, ${switches.length - on} off.`}
                    </span>
                  </div>
                </TableCell>
                <TableCell />
                <TableCell>
                  <Button type="button" variant="link" size="sm" className="h-auto p-0 text-xs" onClick={() => setMode("models")}>
                    Set switches
                  </Button>
                </TableCell>
              </TableRow>
              <TableRow className="bg-muted/30">
                <TableCell>
                  <Switch checked={!draft.denyOther} aria-label="Allow every other model" onCheckedChange={(v) => set("denyOther", !v)} />
                </TableCell>
                <TableCell>
                  <div className="flex flex-col gap-1">
                    <span className="text-sm font-medium">Every other model</span>
                    <span className="text-muted-foreground text-xs">
                      {draft.denyOther ? "Denied" : "Allowed"}: a model no rule matches and no switch sets. New models in the catalog land here.
                    </span>
                  </div>
                </TableCell>
                <TableCell className="text-muted-foreground text-xs">
                  {rows.filter((c) => !decided.get(qualified(c))?.rule && !(qualified(c) in draft.models)).length} models
                </TableCell>
                <TableCell />
              </TableRow>
              {aliases.length > 0 && (
                <TableRow>
                  <TableCell colSpan={4} className="pt-5 text-sm font-semibold">
                    Aliases
                  </TableCell>
                </TableRow>
              )}
              {aliases.map((a) => (
                <AliasRow key={a.id} alias={a} options={targets} byName={byName} health={health} onChange={(next) => setAlias(a.id, next)} />
              ))}
            </>
          ) : (
            <>
              {aliases.map((a) => (
                <AliasRow key={a.id} alias={a} byName={byName} health={health} />
              ))}
              {shown.map((c) => {
                const k = qualified(c);
                const h = health(c.provider);
                const d = decided.get(k);
                const rule = d?.rule ? draft.rules[d.rule.index] : undefined;
                const override = !!rule && isOverride(rule.pattern, c);
                const masked = !!rule && !override;
                const reaching = aliasesReaching(draft.aliases, k);
                const shared = view?.shared.find((s) => s.model === c.model && s.providers.includes(c.provider));
                return (
                  <TableRow key={k} className={cn(!c.allowed && "text-muted-foreground", masked && "bg-muted/40")}>
                    <TableCell>
                      {override ? (
                        <Switch
                          checked={c.allowed}
                          className="data-[state=checked]:bg-warning data-[state=unchecked]:bg-warning/40 outline-warning outline-2 outline-offset-2 outline-dashed"
                          aria-label={`Override ${k}`}
                          title={`Overridden by rule ${d!.rule!.index + 1}`}
                          onCheckedChange={(v) => setRule(rule.id, { ...rule, allow: v })}
                        />
                      ) : (
                        <Switch
                          checked={switchOf(rules, c)}
                          disabled={masked}
                          aria-label={`Allow ${k}`}
                          title={masked ? `Masked by rule ${d!.rule!.index + 1}: ${rule.pattern}` : undefined}
                          onCheckedChange={(v) => set("models", setToggles(draft.models, [c], v))}
                        />
                      )}
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
                          {d?.rule && (
                            <span className={cn(override && "text-warning")}>
                              {d.rule.allow ? "allowed" : "denied"} by {override ? "an override" : "rule"} {d.rule.index + 1}
                              {!override && (
                                <>
                                  {" "}
                                  <Mono>{d.rule.pattern}</Mono>
                                </>
                              )}
                            </span>
                          )}
                          <RowNote candidate={c} shared={shared} winners={draft.winners} renamed={!!draft.renames[k]} setWinner={setWinner} />
                        </span>
                        {!c.allowed && reaching.length > 0 && (
                          <span className="text-warning text-xs">
                            Still reachable as {reaching.map((r, j) => [j > 0 && ", ", <Mono key={r.index}>{r.name || "…"}</Mono>])}: aliases are
                            explicit grants.
                          </span>
                        )}
                        {reaching.length > 0 && (
                          <span className="flex flex-wrap gap-1">
                            {reaching.map((r) => (
                              <Badge key={r.index} variant="outline" className="font-mono">
                                {r.name || "…"}
                                {r.fallback && <span className="text-muted-foreground font-sans">as fallback</span>}
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
                      {masked && (
                        <Button
                          type="button"
                          variant="link"
                          size="sm"
                          className="h-auto p-0 text-xs"
                          title={`Add a rule for ${k} above rule ${d!.rule!.index + 1}`}
                          onClick={() => addRule(!c.allowed, k)}
                        >
                          {c.allowed ? "Deny" : "Allow"} anyway
                        </Button>
                      )}
                      {override && (
                        <Button type="button" variant="link" size="sm" className="h-auto p-0 text-xs" onClick={() => setRule(rule.id, null)}>
                          Remove override
                        </Button>
                      )}
                    </TableCell>
                  </TableRow>
                );
              })}
            </>
          )}
        </TableBody>
      </Table>
      {draft.providers.length === 0 && <Empty>No providers: keys under this policy see no models.</Empty>}
      {view && rows.length === 0 && draft.providers.length > 0 && <Empty>The allowed providers have no models in the catalog.</Empty>}
      {mode === "models" && rows.length > 0 && shown.length === 0 && (
        <Empty>{showDenied ? "No models match the filter." : "No allowed models match the filter."}</Empty>
      )}
    </Section>
  );
}

/** A rule as written: allow or deny, its glob, and its place in the order. */
function RuleTableRow({
  rule: r,
  index,
  last,
  matched,
  decides,
  onChange,
  onMove,
}: {
  rule: RuleRow;
  index: number;
  last: boolean;
  matched: number;
  /** Models this rule decides: it matches them and no earlier rule does. */
  decides: number;
  onChange: (r: RuleRow | null) => void;
  onMove: (by: number) => void;
}) {
  const label = r.pattern || "new rule";
  return (
    <TableRow>
      <TableCell>
        <Switch checked={r.allow} aria-label={`Rule ${index + 1}: allow`} onCheckedChange={(allow) => onChange({ ...r, allow })} />
      </TableCell>
      <TableCell>
        <div className="flex flex-col gap-1">
          <div className="flex items-center gap-2">
            <span className="text-muted-foreground w-5 text-right font-mono text-xs">{index + 1}</span>
            <Input
              aria-label={`Rule ${index + 1}`}
              autoFocus={!r.pattern}
              className="h-8 w-64 font-mono text-xs"
              placeholder={r.allow ? "claude-*" : "*-preview"}
              value={r.pattern}
              onChange={(e) => onChange({ ...r, pattern: e.target.value })}
            />
          </div>
          <span className="text-muted-foreground pl-7 text-xs">
            {r.allow ? "Allow" : "Deny"} rule: {r.allow ? "allows" : "denies"} the models it matches that no earlier rule matched. Globs match{" "}
            <Mono>model</Mono> or <Mono>provider/model</Mono>.
          </span>
        </div>
      </TableCell>
      <TableCell className="text-muted-foreground text-xs">
        {r.pattern.trim() ? (
          <>
            {matched} model{matched === 1 ? "" : "s"}
            {decides < matched && <span className="block">{decides === 0 ? "all matched by earlier rules" : `${decides} not matched earlier`}</span>}
          </>
        ) : null}
      </TableCell>
      <TableCell>
        <div className="flex items-center">
          <Button type="button" size="icon" variant="ghost" className="size-7" aria-label={`Move ${label} up`} disabled={index === 0} onClick={() => onMove(-1)}>
            <ArrowUp />
          </Button>
          <Button type="button" size="icon" variant="ghost" className="size-7" aria-label={`Move ${label} down`} disabled={last} onClick={() => onMove(1)}>
            <ArrowDown />
          </Button>
          <Button type="button" size="icon" variant="ghost" className="size-7" aria-label={`Remove ${label}`} onClick={() => onChange(null)}>
            <Trash2 />
          </Button>
        </div>
      </TableCell>
    </TableRow>
  );
}

/** An alias: its name and targets. Read-only without `onChange`. */
function AliasRow({
  alias: a,
  options = [],
  byName,
  health,
  onChange,
}: {
  alias: AliasDraft;
  options?: string[];
  byName: Map<string, Candidate>;
  health: (provider: string) => Health;
  onChange?: (a: AliasDraft | null) => void;
}) {
  const target = byName.get(a.targets[0]?.trim() ?? "");
  const routes = target && (
    <span className={cn("flex items-center gap-2", !usable(health(target.provider)) && "text-muted-foreground")}>
      <span className="font-mono text-xs">{target.provider}</span>
      <HealthBadge health={health(target.provider)} />
    </span>
  );
  if (!onChange) {
    if (!a.name.trim()) return null;
    return (
      <TableRow className="bg-muted/30">
        <TableCell />
        <TableCell>
          <div className="flex flex-col gap-1">
            <span className="font-mono text-xs">{a.name}</span>
            <span className="text-muted-foreground text-xs">
              alias → {a.targets.filter(Boolean).map((t, i) => [i > 0 && ", then ", <Mono key={i}>{t}</Mono>])}
            </span>
          </div>
        </TableCell>
        <TableCell>{routes}</TableCell>
        <TableCell />
      </TableRow>
    );
  }
  return (
    <TableRow>
      <TableCell />
      <TableCell>
        <div className="flex flex-col gap-1.5">
          <div className="flex flex-wrap items-start gap-2">
            <Input
              aria-label="Alias name"
              autoFocus={!a.name}
              className="h-8 w-48 font-mono text-xs"
              placeholder="fast"
              value={a.name}
              onChange={(e) => onChange({ ...a, name: e.target.value })}
            />
            <span className="text-muted-foreground pt-1.5 text-xs">→</span>
            <div className="flex min-w-64 flex-1 flex-col gap-1">
              {a.targets.map((t, i) => (
                <div key={i} className="flex items-center gap-1">
                  <Combobox
                    aria-label={i === 0 ? "Alias target" : `Fallback ${i}`}
                    className="flex-1"
                    placeholder={i === 0 ? "find a model" : "fallback model"}
                    value={t}
                    options={options}
                    onChange={(v) => onChange({ ...a, targets: a.targets.map((x, j) => (j === i ? v : x)) })}
                  />
                  {i > 0 && (
                    <Button
                      type="button"
                      size="icon"
                      variant="ghost"
                      className="size-7"
                      aria-label={`Remove fallback ${t}`}
                      onClick={() => onChange({ ...a, targets: a.targets.filter((_, j) => j !== i) })}
                    >
                      <Trash2 />
                    </Button>
                  )}
                </div>
              ))}
            </div>
          </div>
          <span className="text-muted-foreground flex flex-wrap items-center gap-x-2 text-xs">
            Alias: another name, tried in order on 429 or 5xx. It reaches its targets whatever the rules say.
            <Button type="button" variant="link" size="sm" className="h-auto p-0 text-xs" onClick={() => onChange({ ...a, targets: [...a.targets, ""] })}>
              Add a fallback
            </Button>
          </span>
        </div>
      </TableCell>
      <TableCell>{routes}</TableCell>
      <TableCell>
        <Button type="button" size="icon" variant="ghost" aria-label={`Remove alias ${a.name}`} onClick={() => onChange(null)}>
          <Trash2 />
        </Button>
      </TableCell>
    </TableRow>
  );
}

/** Who serves a model name that several providers offer, unless a row picks one. */
function PrecedenceRow({ precedence: { order, move, reset } }: { precedence: Precedence }) {
  if (order.length < 2) return null;
  return (
    <div className="flex flex-col gap-1.5">
      <div className="flex flex-wrap items-center gap-x-2 text-sm">
        <span className="font-medium">Precedence</span>
        <span className="text-muted-foreground text-xs">
          who serves a name several providers offer, unless its row picks one{reset ? "" : " (default order)"}
        </span>
        {reset && (
          <Button type="button" variant="link" size="sm" className="h-auto p-0 text-xs" onClick={reset}>
            Reset to the default order
          </Button>
        )}
      </div>
      <ol className="flex flex-wrap items-center gap-1.5" aria-label="Precedence">
        {order.map((p, i) => (
          <li key={p} className="flex items-center gap-0.5 rounded-md border py-0.5 pr-0.5 pl-2">
            <span className="text-muted-foreground font-mono text-xs">{i + 1}</span>
            <span className="px-1 font-mono text-xs">{p}</span>
            <Button type="button" size="icon" variant="ghost" className="size-6" aria-label={`Move ${p} up`} disabled={i === 0} onClick={() => move(i, -1)}>
              <ArrowLeft />
            </Button>
            <Button
              type="button"
              size="icon"
              variant="ghost"
              className="size-6"
              aria-label={`Move ${p} down`}
              disabled={i === order.length - 1}
              onClick={() => move(i, 1)}
            >
              <ArrowRight />
            </Button>
          </li>
        ))}
      </ol>
    </div>
  );
}

/** Who serves the row's plain name, or what took it. */
function RowNote({
  candidate: c,
  shared,
  winners,
  renamed,
  setWinner,
}: {
  candidate: Candidate;
  shared: PolicyView["shared"][number] | undefined;
  winners: Record<string, string>;
  renamed: boolean;
  setWinner: (model: string, winner: string | null) => void;
}) {
  if (!c.allowed) return null;
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

export const lines = (s: string) =>
  s
    .split("\n")
    .map((l) => l.trim())
    .filter(Boolean);
