// ---
// relationships:
//   tests: account-state
// ---
import { renderToStaticMarkup } from "react-dom/server";
import { expect, test } from "vitest";
import { AccountStateBadge, AccountUsageSnapshot, WindowBars, WindowsSummary } from "./account-state";
import type { Account } from "@/lib/api";

const windows = [
  { name: "iguana_necktie", used_percent: 100, resets_at: null, window_secs: null },
  { name: "7d-fable", used_percent: 73, resets_at: 4_070_908_800, window_secs: 604800 },
];

test("both account summaries and quota bars hide cached internal windows and show Fable", () => {
  for (const element of [<WindowsSummary windows={windows} />, <WindowBars windows={windows} />]) {
    const html = renderToStaticMarkup(element);
    expect(html).not.toContain("iguana_necktie");
    expect(html).toContain("7d-fable");
    expect(html).toContain("73%");
  }
});

test("usage diagnostics retain provider JSON even when no public windows are available", () => {
  const usage = {
    account_id: "example",
    fetched_at: 1_795_503_360,
    windows: windows.slice(0, 1),
    raw: { reset_credits: 0, rate_limit: { allowed: false }, extra: ["<example>", null] },
  };
  const html = renderToStaticMarkup(<AccountUsageSnapshot usage={usage} />);
  expect(html).toContain("Usage · fetched");
  expect(html).toContain("<details");
  expect(html).toContain("Provider usage diagnostics (returned JSON)");
  expect(html).toContain("&quot;account_id&quot;: &quot;example&quot;");
  expect(html).toContain("&quot;fetched_at&quot;: 1795503360");
  expect(html).toContain("&quot;windows&quot;: []");
  expect(html).toContain("&quot;reset_credits&quot;: 0");
  expect(html).toContain("&quot;allowed&quot;: false");
  expect(html).toContain("&lt;example&gt;");
  expect(html).not.toContain("iguana_necktie");
});

test("unsaved token replaces Available and appears alongside Disabled", () => {
  const account: Account = {
    id: "example",
    provider: "codex",
    label: "Example",
    email: null,
    external_id: null,
    plan: null,
    enabled: true,
    priority: 0,
    priority_decimal: "0",
    cooldown_until: null,
    cooldown_reason: null,
    auth_failed: false,
    last_error: null,
    last_used_at: null,
    created_at: 0,
    auth_index: "example",
    available: true,
    expires_at: null,
    unsaved: true,
    usage: null,
  };
  const available = renderToStaticMarkup(<AccountStateBadge account={account} />);
  expect(available).toContain("Unsaved token");
  expect(available).not.toContain("Available");
  const disabled = renderToStaticMarkup(<AccountStateBadge account={{ ...account, enabled: false }} />);
  expect(disabled).toContain("Disabled");
  expect(disabled).toContain("Unsaved token");
});
