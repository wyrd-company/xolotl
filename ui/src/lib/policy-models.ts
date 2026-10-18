// The policy editor's model list: ordered rules, per-model switches,
// sorting, aliases, and provider health.

/** A catalog model of an allowed provider (`Policy::candidates`). */
export interface Candidate {
  provider: string;
  model: string;
  allowed: boolean;
  /** The glob that decided, if any. */
  rule: string | null;
  /** The name that reaches it first; null if denied, it lost its plain name, or the name was taken. */
  name: string | null;
  /** What serves the name it would have had instead. */
  taken_by?: { kind: "alias" } | { kind: "rename"; target: string };
}

/**
 * Every alias with `target` among its targets. Aliases are explicit grants:
 * they reach the target even when the policy denies it.
 */
export function aliasesReaching(aliases: { name: string; targets: string[] }[], target: string) {
  return aliases.flatMap((a, index) => {
    const at = a.targets.indexOf(target);
    return at < 0 ? [] : [{ index, name: a.name, fallback: at > 0 }];
  });
}

export const qualified = (c: { provider: string; model: string }) => `${c.provider}/${c.model}`;

/** `*` matches any run of characters (`keys::glob_match`). */
export function globMatch(pattern: string, text: string): boolean {
  const parts = pattern.split("*");
  if (parts.length === 1) return pattern === text;
  const first = parts[0];
  const last = parts[parts.length - 1];
  if (!text.startsWith(first) || text.length < first.length + last.length || !text.endsWith(last)) return false;
  let rest = text.slice(first.length, text.length - last.length);
  for (const mid of parts.slice(1, -1)) {
    const i = rest.indexOf(mid);
    if (i < 0) return false;
    rest = rest.slice(i + mid.length);
  }
  return true;
}

const matches = (pattern: string, c: { provider: string; model: string }) => globMatch(pattern, c.model) || globMatch(pattern, qualified(c));

/** A model rule: an allow or deny glob. */
export interface ModelRule {
  pattern: string;
  allow: boolean;
}

/** How a policy decides models: rules in order, then switches, then the default. */
export interface ModelRules {
  rules: ModelRule[];
  /** `provider/model` -> on or off. */
  models: Record<string, boolean>;
  /** Deny models that no rule matches and that have no switch. */
  denyOther: boolean;
}

export interface Decision {
  allowed: boolean;
  /** The first rule that matched; the switch doesn't count then. `index` is its place in `rules`. */
  rule?: { index: number; allow: boolean; pattern: string };
}

/**
 * Whether a model of an allowed provider is allowed (`Policy::model_rule`):
 * the first rule that matches, else its switch, else the default.
 */
export function decide(rules: ModelRules, c: { provider: string; model: string }): Decision {
  const index = rules.rules.findIndex((r) => r.pattern.trim() && matches(r.pattern.trim(), c));
  if (index >= 0) {
    const r = rules.rules[index];
    return { allowed: r.allow, rule: { index, allow: r.allow, pattern: r.pattern.trim() } };
  }
  return { allowed: switchOf(rules, c) };
}

/** The model's own switch: its `models` entry, else the default. */
export const switchOf = (rules: ModelRules, c: { provider: string; model: string }) => rules.models[qualified(c)] ?? !rules.denyOther;

/** An override: a rule for exactly this model. */
export const isOverride = (pattern: string, c: { provider: string; model: string }) => pattern.trim() === qualified(c);

/** The models a pattern matches. */
export const matching = (pattern: string, rows: { provider: string; model: string }[]) =>
  pattern ? rows.filter((c) => matches(pattern, c)) : [];

/** Turn each row's switch on or off. Rules are left alone. */
export function setToggles(models: Record<string, boolean>, rows: { provider: string; model: string }[], on: boolean) {
  const next = { ...models };
  for (const c of rows) next[qualified(c)] = on;
  return next;
}

/** Move the item at `index` by `by` places. */
export function move<T>(items: T[], index: number, by: number): T[] {
  const to = index + by;
  if (to < 0 || to >= items.length) return items;
  const next = [...items];
  [next[index], next[to]] = [next[to], next[index]];
  return next;
}

export type SortBy = "allowed" | "name" | "provider";

/** Default: allowed first, then name. */
export function sortRows(rows: Candidate[], by: SortBy): Candidate[] {
  const name = (c: Candidate) => c.name ?? c.model;
  const cmp = (a: string, b: string) => (a < b ? -1 : a > b ? 1 : 0);
  return [...rows].sort((a, b) => {
    if (by === "allowed" && a.allowed !== b.allowed) return a.allowed ? -1 : 1;
    if (by === "provider" && a.provider !== b.provider) return cmp(a.provider, b.provider);
    return cmp(name(a), name(b)) || cmp(a.provider, b.provider);
  });
}

export type Health = { kind: "pool"; total: number; available: number } | { kind: "gateway"; ready: boolean } | { kind: "unknown" };

/** The subscription provider of an account id: ids are `<provider>-<suffix>` (`policy::account_provider`). */
export function accountProvider(id: string, pools: readonly string[]): string | undefined {
  return pools.find((p) => id.startsWith(`${p}-`) && id.length > p.length + 1);
}

/** Whether a policy's `accounts` lets keys use an account of an allowed provider (`Policy::allows_account`). No entry: none. */
export function allowsAccount(accounts: Record<string, string[]>, provider: string, id: string): boolean {
  const ids = accounts[provider] ?? [];
  return ids.includes("*") || ids.includes(id);
}

/** Accounts of a subscription pool the policy uses, or the key state of a gateway provider. */
export function providerHealth(
  provider: string,
  pools: readonly string[],
  accounts: { id: string; provider: string; available: boolean }[],
  gateway: { name: string; has_key: boolean; no_auth: boolean }[],
  /** The policy's `accounts`; without it, every account counts. */
  policyAccounts?: Record<string, string[]>,
): Health {
  if (pools.includes(provider)) {
    const mine = accounts.filter((a) => a.provider === provider && (!policyAccounts || allowsAccount(policyAccounts, provider, a.id)));
    return { kind: "pool", total: mine.length, available: mine.filter((a) => a.available).length };
  }
  const g = gateway.find((p) => p.name === provider);
  return g ? { kind: "gateway", ready: g.has_key || g.no_auth } : { kind: "unknown" };
}

/** Whether requests can reach the provider right now. */
export const usable = (h: Health) => (h.kind === "pool" ? h.available > 0 : h.kind === "gateway" ? h.ready : true);
