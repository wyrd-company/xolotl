import type { GroupBy } from "@/lib/api";

export const GROUPS: [GroupBy, string][] = [
  ["client", "Client"],
  ["account", "Account"],
  ["provider", "Provider"],
  ["model", "Model"],
];
export const PERIODS: [string, number][] = [
  ["Last 24 hours", 86_400],
  ["Last 7 days", 7 * 86_400],
  ["Last 30 days", 30 * 86_400],
  ["Last 90 days", 90 * 86_400],
];
// After narrowing to a row, show the first of these that is not already narrowed.
const NEXT_GROUP: GroupBy[] = ["model", "account", "client", "provider"];

// The view lives in the hash route (`/#/usage?group=model&client=…`) so it can be linked to.
// A filter is a `client`, `account`, `provider` or `model` parameter; an empty value is the row with no key.
export function readView(params: URLSearchParams) {
  const groupParam = params.get("group");
  const group = GROUPS.find(([g]) => g === groupParam)?.[0] ?? "client";
  const sinceParam = params.get("since");
  const period = PERIODS.find(([, secs]) => String(secs) === sinceParam)?.[1] ?? PERIODS[0][1];
  const filters = GROUPS.flatMap(([g]) => {
    const value = params.get(g);
    return value === null ? [] : [[g, value] as const];
  });
  return { group, period, filters };
}

export function withParams(params: URLSearchParams, change: Record<string, string | null>) {
  const next = new URLSearchParams(params);
  for (const [name, value] of Object.entries(change)) {
    if (value === null) next.delete(name);
    else next.set(name, value);
  }
  return `?${next}`;
}

/**
 * The link that narrows the view to a row of the current grouping, or null once the current grouping is
 * itself narrowed. The last filter keeps its grouping, since every other one is narrowed already.
 */
export function narrowTo(params: URLSearchParams, key: string | null) {
  const { group, filters } = readView(params);
  const filtered = new Set(filters.map(([g]) => g));
  if (filtered.has(group)) return null;
  const next = NEXT_GROUP.find((g) => g !== group && !filtered.has(g)) ?? group;
  return withParams(params, { [group]: key ?? "", group: next });
}
