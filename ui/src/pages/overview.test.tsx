// ---
// relationships:
//   tests: overview
// ---
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { renderToStaticMarkup } from "react-dom/server";
import { MemoryRouter } from "react-router";
import { expect, test } from "vitest";
import type { Account, GatewayProvider, Status } from "@/lib/api";
import { OverviewPage } from "./overview";

const account = (change: Partial<Account> = {}): Account => ({
  id: "account-a",
  provider: "codex",
  label: "Primary",
  email: null,
  external_id: null,
  plan: null,
  enabled: true,
  priority: 0,
  cooldown_until: null,
  cooldown_reason: null,
  auth_failed: false,
  last_error: null,
  last_used_at: null,
  created_at: 0,
  auth_index: "",
  available: true,
  expires_at: null,
  unsaved: false,
  usage: null,
  ...change,
});
const status: Status = {
  version: "0.3.0",
  vault: { ok: true },
  database: { ok: true },
  catalog_refreshed_secs_ago: 10,
  unsaved_credentials: 0,
  recent_errors: [],
  dashboard_url: null,
  vault_mount: "secret",
  vault_prefix: "sample",
  subscription: {
    strategy: "least-used",
    soft_limit_percent: 80,
    session_affinity_secs: 60,
    usage_poll_secs: 60,
  },
};
const gateway = (change: Partial<GatewayProvider>): GatewayProvider => ({
  name: "gateway-a",
  kind: "openai-compatible",
  enabled: true,
  base_url: null,
  no_auth: false,
  secret: "sample",
  has_key: true,
  ...change,
});
function render(
  accounts: Account[],
  providers: GatewayProvider[] = [],
  s = status,
) {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false, staleTime: Infinity } },
  });
  client.setQueryData(["status"], s);
  client.setQueryData(["accounts"], accounts);
  client.setQueryData(["providers"], providers);
  client.setQueryData(
    ["catalog-routes"],
    [{ provider: "gateway-a", model: "model-a" }],
  );
  client.setQueryData(["catalog-status"], {});
  client.setQueryData(
    ["usage", "provider", 86400],
    [
      {
        key: "gateway-a",
        requests: 4,
        errors: 1,
        input_tokens: 100,
        output_tokens: 25,
        cached_tokens: 0,
      },
    ],
  );
  return renderToStaticMarkup(
    <QueryClientProvider client={client}>
      <MemoryRouter>
        <OverviewPage />
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

test("action rows name the fix and link to existing account details", () => {
  const html = render([
    account({
      id: "needs/key &",
      provider: "opencode-go",
      auth_failed: true,
      available: false,
      last_error: "Upstream 401",
    }),
    account({ id: "needs-login", auth_failed: true, available: false }),
    account({
      id: "disabled",
      enabled: false,
      auth_failed: true,
      available: false,
    }),
  ]);
  const action = html.slice(
    html.indexOf('aria-label="Needs action"'),
    html.indexOf('aria-label="Health"'),
  );
  expect(action).toContain('href="/accounts?account=needs%2Fkey%20%26"');
  expect(action).toContain('href="/accounts?account=needs-login"');
  expect(action).toContain("Replace key");
  expect(action).toContain("Log in again");
  expect(action).toContain("Upstream 401");
  expect(action).not.toContain("account=disabled");
  expect(action).not.toContain("/accounts/add");
});

test("transient states stay under Health while failures of OpenBao and SQLite need action", () => {
  const accounts = [
    account({
      available: false,
      cooldown_until: Date.now() / 1000 + 3600,
      cooldown_reason: "Quota reset",
      unsaved: true,
    }),
  ];
  const html = render(accounts, [], { ...status, unsaved_credentials: 1 });
  expect(html).not.toContain('aria-label="Needs action"');
  const health = html.slice(html.indexOf('aria-label="Health"'));
  expect(health).toContain("cooling down until");
  expect(health).toContain("keeps retrying");
  expect(health).toContain("Primary: Cooling down · credentials not saved");
  expect(html).not.toContain("all credentials saved");
  const broken = render(accounts, [], {
    ...status,
    vault: { ok: false, error: "Vault offline" },
    database: { ok: false, error: "Disk unavailable" },
  });
  const action = broken.slice(
    broken.indexOf('aria-label="Needs action"'),
    broken.indexOf('aria-label="Health"'),
  );
  expect(action).toContain("Vault offline");
  expect(action).toContain("Disk unavailable");
  expect(action).toContain("#restore-openbao");
  expect(action).toContain("#restore-sqlite");
});

test("health rows show configured pools and keyed or no-auth gateways only", () => {
  const html = render(
    [account()],
    [
      gateway({}),
      gateway({ name: "missing-gateway", has_key: false }),
      gateway({ name: "local-gateway", no_auth: true, has_key: false }),
      gateway({ name: "disabled-gateway", enabled: false }),
    ],
  );
  expect(html).toContain("1 of 1 available");
  expect(html).not.toContain("0 of 0");
  const table = html.slice(html.indexOf("<table"), html.indexOf("</table>"));
  expect(table).toContain("gateway-a");
  expect(table).toContain("local-gateway");
  expect(table).toContain("disabled-gateway");
  expect(table).not.toContain("missing-gateway");
  expect(html).toContain(
    "Not set up: Claude · Grok · OpenCode Go · missing-gateway (no key)",
  );
});

test("least-used available reported quota preserves scoped windows and reset counts", () => {
  const windows = (used: number) => ({
    account_id: "account-a",
    fetched_at: 0,
    raw: { reset_credits: 2 },
    windows: [
      {
        name: "7d-fable",
        used_percent: used,
        resets_at: null,
        window_secs: 604800,
      },
      {
        name: "iguana_necktie",
        used_percent: 100,
        resets_at: null,
        window_secs: null,
      },
    ],
  });
  const html = render([
    account({ id: "unknown" }),
    account({ id: "low", label: "Low", usage: windows(30) }),
    account({ id: "high", label: "High", usage: windows(70) }),
    account({
      id: "failed",
      label: "Failed",
      available: false,
      auth_failed: true,
      usage: windows(1),
    }),
  ]);
  expect(html).toContain("Least-used available account (Low)");
  expect(html).toContain("7d-fable");
  expect(html).toContain("30%");
  expect(html).not.toContain("iguana_necktie");
  expect(html).toContain("6 reset credits available · 1 account unknown");
  expect(html).toContain('aria-label="Codex account states"');
});

test("traffic includes failed count and provider split, with a real errors-only Usage link", () => {
  const html = render([]);
  expect(html).toContain("25.0%");
  expect(html).toContain("1 failed");
  expect(html).toContain("Requests by provider");
  expect(html).toContain('href="/usage?errors=true"');
  expect(html).toContain("Nothing needs action");
});
