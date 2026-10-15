// The policy editor's model list: allow/deny toggles written as exact
// `provider/model` entries, sorting, more names, and provider health. No imports, so
// `node --test` runs it without a bundler.

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

export const splitTargets = (s: string) =>
  s
    .split(",")
    .map((t) => t.trim())
    .filter(Boolean);

/**
 * Every alias with `target` among its targets. Aliases are explicit grants:
 * they reach the target even when the model globs deny it.
 */
export function aliasesReaching(aliases: { name: string; targets: string }[], target: string) {
  return aliases.flatMap((a, index) => {
    const at = splitTargets(a.targets).indexOf(target);
    return at < 0 ? [] : [{ index, name: a.name, fallback: at > 0 }];
  });
}

/** `targets` without `target`. */
export const withoutTarget = (targets: string, target: string) =>
  splitTargets(targets)
    .filter((t) => t !== target)
    .join(", ");

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

const matches = (patterns: string[], c: { provider: string; model: string }) =>
  patterns.some((p) => globMatch(p, c.model) || globMatch(p, qualified(c)));

export interface ModelRules {
  allow: string[];
  deny: string[];
}

/** Whether the globs allow a model of an allowed provider (`Policy::allows_model`). */
export const rulesAllow = (rules: ModelRules, c: { provider: string; model: string }) => {
  return !matches(rules.deny, c) && (rules.allow.length === 0 || matches(rules.allow, c));
};

/**
 * Allow or deny each row. Turning a model off adds its `provider/model` to
 * `deny` (deny wins) rather than emptying `allow`, which would allow every
 * model. Turning it on removes that entry, and adds one to a non-empty
 * `allow` that doesn't match it yet. A deny glob other than the exact entry
 * still wins; the row shows it.
 */
export function setAllowed(rules: ModelRules, rows: Candidate[], on: boolean): ModelRules {
  let { allow, deny } = rules;
  for (const c of rows) {
    if (c.allowed === on) continue;
    const q = qualified(c);
    if (on) {
      deny = deny.filter((p) => p !== q);
      if (allow.length > 0 && !matches(allow, c)) allow = [...allow, q];
    } else {
      if (allow.length > 1 && allow.includes(q)) allow = allow.filter((p) => p !== q);
      if ((allow.length === 0 || matches(allow, c)) && !deny.includes(q)) deny = [...deny, q];
    }
  }
  return { allow, deny };
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

/** Accounts of a subscription pool, or the key state of a gateway provider. */
export function providerHealth(
  provider: string,
  pools: readonly string[],
  accounts: { provider: string; available: boolean }[],
  gateway: { name: string; has_key: boolean; no_auth: boolean }[],
): Health {
  if (pools.includes(provider)) {
    const mine = accounts.filter((a) => a.provider === provider);
    return { kind: "pool", total: mine.length, available: mine.filter((a) => a.available).length };
  }
  const g = gateway.find((p) => p.name === provider);
  return g ? { kind: "gateway", ready: g.has_key || g.no_auth } : { kind: "unknown" };
}

/** Whether requests can reach the provider right now. */
export const usable = (h: Health) => (h.kind === "pool" ? h.available > 0 : h.kind === "gateway" ? h.ready : true);
