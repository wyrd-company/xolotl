// ---
// relationships:
//   tests: usage-sessions
// ---
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { renderToStaticMarkup } from "react-dom/server";
import { expect, test } from "vitest";
import type { SessionRow, ToolUsageRow } from "@/lib/api";
import { clientQuery, SessionsCard, ToolsCard } from "./usage-sessions";

const base: SessionRow = {
  client_id: "team/alpha", session_key: "0123456789abcdef", source: "codex", first_seen: 1, last_seen: 2, turns: 7,
  current_tokens: 40000, peak_tokens: 80000, prompt_tokens: 1000, cached_tokens: 250, context_window: 160000,
  provider: "sample", model: "model-a", utilization: 0.25, peak_utilization: 0.5, cache_hit_ratio: 0.25,
  lock_name: null, lock_provider: null, lock_model: null, lock_account: null, locked_at: null,
  lock_failures: 0, lock_breaks: 0, last_break_reason: null, last_break_at: null,
};

function sessions(rows: SessionRow[], client?: string) {
  const cache = new QueryClient({ defaultOptions: { queries: { staleTime: Infinity, retry: false } } });
  cache.setQueryData(["sessions", client], rows);
  return renderToStaticMarkup(<QueryClientProvider client={cache}><SessionsCard client={client} /></QueryClientProvider>);
}

test("a session shows its size against the window, peak, turns and cache hits", () => {
  const html = sessions([base]);
  for (const text of ["team/alpha", "01234567", "codex", "sample/model-a", "40K / 160K (25%)", "80K (50%)", ">7<", ">25%<", "none"]) expect(html).toContain(text);
  expect(html).not.toContain("0123456789abcdef<");
});

test("an unknown window stays unknown", () => {
  const html = sessions([{ ...base, context_window: null, utilization: null, peak_utilization: null, cache_hit_ratio: null }]);
  expect(html).toContain("40K / ? (unknown)");
  expect(html).not.toContain("NaN");
});

test("a lock names its account, its failures and its breaks", () => {
  const html = sessions([{ ...base, lock_provider: "sample", lock_model: "model-a", lock_name: "fast", lock_account: "sample-1a2b", lock_failures: 2, lock_breaks: 1, last_break_reason: "cooldown" }]);
  for (const text of ["sample-1a2b", "2 failed", "broken 1× (cooldown)"]) expect(html).toContain(text);
  expect(html).not.toContain(">none<");
});

test("a gateway lock names its target", () => {
  expect(sessions([{ ...base, lock_provider: "gw", lock_model: "model-b", lock_name: "fast" }])).toContain("gw/model-b");
});

test("the client filter reaches the API, and the admin token's empty filter stays a filter", () => {
  expect(clientQuery(undefined)).toBe("");
  expect(clientQuery("team/alpha")).toBe("?client=team%2Falpha");
  expect(clientQuery("")).toBe("?client=");
});

test("no sessions says so", () => {
  expect(sessions([])).toContain("No sessions yet.");
});

test("tools name their route and count per key", () => {
  const rows: ToolUsageRow[] = [
    { client_id: "team/alpha", tool: "read", source: "llm", calls: 3, last_used: 1 },
    { client_id: "team/alpha", tool: "srv/lookup", source: "mcp", calls: 1200, last_used: 1 },
  ];
  const cache = new QueryClient({ defaultOptions: { queries: { staleTime: Infinity, retry: false } } });
  cache.setQueryData(["tools", undefined], rows);
  const html = renderToStaticMarkup(<QueryClientProvider client={cache}><ToolsCard /></QueryClientProvider>);
  for (const text of ["read", "LLM response", ">3<", "srv/lookup", "/mcp"]) expect(html).toContain(text);
});
