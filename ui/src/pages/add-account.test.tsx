// ---
// relationships:
//   tests: add-account
// ---
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { renderToStaticMarkup } from "react-dom/server";
import { MemoryRouter } from "react-router";
import { expect, test } from "vitest";
import type { Account, AccountKindInfo, FlowStart, ProviderAccount } from "@/lib/api";
import { AccountsPage } from "./accounts";
import { AddAccountPage, DeviceCode, ImportOutcome, parseHeaders } from "./add-account";

const kind = (change: Partial<AccountKindInfo>): AccountKindInfo => ({
  kind: "codex",
  label: "Sample",
  description: "A sample kind.",
  placement: "pool",
  provider: null,
  add: ["device-login"],
  default_base_url: null,
  ...change,
});

const KINDS: AccountKindInfo[] = [
  kind({ kind: "codex", label: "Codex (ChatGPT)", provider: "codex", add: ["device-login", "browser-login", "import"] }),
  kind({ kind: "opencode-go", label: "Pool with a key", provider: "opencode-go", add: ["api-key"] }),
  kind({ kind: "vercel", label: "Hosted gateway", placement: "provider", add: ["provider"], default_base_url: "https://gateway.example" }),
  kind({ kind: "openai-compatible", label: "Self-hosted", placement: "provider", add: ["provider"] }),
];

function render(page: React.ReactNode, data: [unknown[], unknown][], path = "/") {
  const client = new QueryClient({ defaultOptions: { queries: { staleTime: Infinity, retry: false } } });
  for (const [key, value] of data) client.setQueryData(key, value);
  return renderToStaticMarkup(
    <QueryClientProvider client={client}>
      <MemoryRouter initialEntries={[path]}>{page}</MemoryRouter>
    </QueryClientProvider>,
  );
}

test("add account lists pool kinds and one provider form from the server's kinds", () => {
  const html = render(<AddAccountPage />, [[["account-kinds"], KINDS]]);
  expect(html).toContain("Codex (ChatGPT)");
  expect(html).toContain("Pool with a key");
  expect(html).toContain('id="add-opencode-go-key"');
  expect(html).toContain("Import a login");
  expect(html).toContain('for="import-label"');
  expect(html).toContain("Label (optional)");
  expect(html).toContain("Provider account");
  expect(html).toContain('id="provider-name"');
  // The first provider kind has a default URL, so the field is optional.
  expect(html).toContain("Base URL (optional)");
  expect(html).toContain('placeholder="https://gateway.example"');
});

test("a kind with no form of its own adds nothing", () => {
  const html = render(<AddAccountPage />, [[["account-kinds"], [kind({ kind: "opencode-go", label: "No form", add: ["browser-login"] })]]]);
  expect(html).toContain("Subscriptions");
  expect(html).not.toContain("No form");
  expect(html).not.toContain("Providers</h2>");
});

test("headers parse from Name: value lines", () => {
  expect(parseHeaders("X-Team: blue\n\nX-Other:  a:b ")).toEqual({ "X-Team": "blue", "X-Other": "a:b" });
  expect(parseHeaders("no colon")).toBe('"no colon" is not "Name: value"');
});

const provider = (change: Partial<ProviderAccount>): ProviderAccount => ({
  name: "sample",
  kind: "openai-compatible",
  source: "vault",
  enabled: true,
  base_url: "http://upstream.example",
  no_auth: false,
  headers: [],
  secret: "providers/sample",
  has_key: true,
  workload_identity: false,
  ...change,
});

test("accounts lists provider accounts next to pool accounts", () => {
  const html = render(<AccountsPage />, [
    [["accounts"], []],
    [["account-kinds"], KINDS],
    [["providers"], [provider({}), provider({ name: "from-file", kind: "vercel", source: "config", has_key: false, base_url: null })]],
  ]);
  expect(html).toContain("Provider accounts");
  expect(html).toContain("Providers (2)");
  expect(html).toContain("All (2)");
  expect(html).toContain("Self-hosted");
  expect(html).toContain("Hosted gateway");
  expect(html).toContain("Added here");
  expect(html).toContain("Config file");
  expect(html).toContain("Set key");
  expect(html).toContain("Replace key");
  // Only accounts added here can be removed here.
  expect(html.match(/>Remove</g)?.length).toBe(1);
});

test("a workload identity account shows its auth state and offers the key action", () => {
  const html = render(<AccountsPage />, [
    [["accounts"], []],
    [["providers"], [provider({ name: "federated", kind: "anthropic", source: "config", has_key: false, workload_identity: true })]],
  ], "/accounts?kind=providers");
  expect(html).toContain("workload identity");
  expect(html).not.toContain("missing");
  expect(html).toContain('aria-label="Set key for federated"');
});

test("the providers tab shows only provider accounts", () => {
  const html = render(<AccountsPage />, [
    [["accounts"], []],
    [["providers"], [provider({})]],
  ], "/accounts?kind=providers");
  expect(html).toContain("Provider accounts");
  expect(html).not.toContain("No subscription accounts yet");
});


const flow: FlowStart = { flow_id: "expired", provider: "codex", needs_code: false, url: "https://example.test/device", user_code: "CODE", instructions: "Authorize" };

test("device poll HTTP failure replaces waiting and does not confirm success", () => {
  const client = new QueryClient({ defaultOptions: { queries: { retryOnMount: false, staleTime: Infinity } } });
  client.getQueryCache().build(client, { queryKey: ["flow", flow.flow_id] })
    .setState({ status: "error", error: new Error("unknown or expired flow") });
  const html = renderToStaticMarkup(<QueryClientProvider client={client}><DeviceCode flow={flow} name="Codex" onDone={() => {}} onRestart={() => {}} /></QueryClientProvider>);
  for (const value of ['role="alert"', "Could not check login status", "unknown or expired flow", "A successful login has not been confirmed", "Start new login", "Cancel"]) expect(html).toContain(value);
  expect(html).not.toContain("Waiting for authorization");
});

const imported = { id: "codex-work", provider: "codex", label: "Work subscription", email: "work@example.test" } as Account;

test.each([ ["Work subscription", "work@example.test", "Work subscription"], ["", "work@example.test", "work@example.test"], ["", null, "codex-work"] ])(
  "saved import shows label %s or identity %s and zero models",
  (label, email, identity) => {
    const html = render(<ImportOutcome account={{ ...imported, label, email, catalog_refresh: { provider: "codex", models: 0 } }} />, []);
    for (const value of [`Imported ${identity}`, "Account ID: codex-work", "provider: codex", "The imported credentials are saved", "codex: 0 models found"]) expect(html).toContain(value);
    expect(html).not.toContain("Model discovery failed");
  },
);

test("saved import keeps the discovery error and offers Refresh models", () => {
  const html = render(<ImportOutcome account={{ ...imported, catalog_refresh: { provider: "codex", error: "Upstream returned 503" } }} />, [[["catalog-status"], {}]]);
  for (const value of ["Imported Work subscription", "Account ID: codex-work", "provider: codex", "The imported credentials are saved", 'role="alert"', "Model discovery failed", "Upstream returned 503", "Refresh models"]) expect(html).toContain(value);
  expect(html).not.toContain("models found");
});
