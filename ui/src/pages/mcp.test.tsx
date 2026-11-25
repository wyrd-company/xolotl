// ---
// relationships:
//   tests: web-ui
// ---
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { ReactNode } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { afterEach, expect, test, vi } from "vitest";
import type { McpLoginStart, McpOAuthStatus, McpServer } from "@/lib/api";
import { LoginFlow, McpPage, OAuthLogin, RefreshResult, SecretField, ToolDefinition } from "./mcp";

function render(node: ReactNode, client = new QueryClient({ defaultOptions: { queries: { retry: false } } })) {
  return renderToStaticMarkup(<QueryClientProvider client={client}>{node}</QueryClientProvider>);
}
const flow: McpLoginStart = { server: "archive", flow_id: "expired", callback: true, url: "https://example.test/authorize", redirect_uri: "https://xolotl.test/callback", registration: "pre-registered" };

afterEach(() => vi.unstubAllGlobals());

function renderUsage(client: QueryClient) {
  vi.stubGlobal("window", { location: { origin: "https://xolotl.test" } });
  client.setQueryData(["mcp-servers"], []);
  return render(<McpPage />, client);
}

test("historical totals and recent calls are visible with no configured servers", () => {
  const client = new QueryClient();
  client.setQueryData(["mcp-usage", "server", 86_400], [
    { key: "retired-server", calls: 12, errors: 1, avg_latency_ms: 240 },
  ]);
  client.setQueryData(["mcp-calls"], [
    { ts: 100, client_id: "team/editor", server: "retired-server", tool: "lookup", status: "tool_error", error: "Lookup failed", latency_ms: 240 },
  ]);

  const html = renderUsage(client);
  for (const value of ["No MCP servers yet", "Last 24h", "Apply period", "role=\"combobox\"", "Group by", "retired-server/lookup", "team/editor", "tool error", ">12<", ">1<", ">240<"]) {
    expect(html).toContain(value);
  }
  expect(html).not.toContain('role="tab"');
});

test("aggregate failure shows its detail and retry while recent calls stay available", async () => {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false, retryOnMount: false } } });
  await expect(client.fetchQuery({
    queryKey: ["mcp-usage", "server", 86_400],
    queryFn: () => Promise.reject(new Error("Database unavailable")),
  })).rejects.toThrow("Database unavailable");
  client.setQueryData(["mcp-calls"], [
    { ts: 100, server: "retired-server", tool: "lookup", status: "ok", latency_ms: 240 },
  ]);

  const html = renderUsage(client);
  for (const value of ["Could not load tool-call totals", "Database unavailable", "Retry totals", "retired-server/lookup"]) {
    expect(html).toContain(value);
  }
  expect(html).not.toContain("No tool calls in this period");
});

test("successful empty totals and empty call history show distinct messages", () => {
  const client = new QueryClient();
  client.setQueryData(["mcp-usage", "server", 86_400], []);
  client.setQueryData(["mcp-calls"], []);

  const html = renderUsage(client);
  expect(html).toContain("No tool calls in this period");
  expect(html).toContain("No tool calls yet");
  expect(html).not.toContain("Could not load");
});

test("global refresh reports each server and retained tools on mixed failure", () => {
  const html = render(<RefreshResult servers={[
    { name: "archive", tools: 3, last_error: null },
    { name: "tracker", tools: 2, last_error: "upstream returned 503" },
  ] as McpServer[]} />);
  for (const value of ["1 server failed", "archive", "3 tools", "tracker", "upstream returned 503", "Previous tool list retained (2 tools)"]) expect(html).toContain(value);
  expect(render(<RefreshResult servers={[{ name: "archive", tools: 3, last_error: null }] as McpServer[]} />)).toContain("0 servers failed");
});

test("tool inspector preserves multiline descriptions, schema and upstream extensions", () => {
  const html = render(<ToolDefinition tool={{ name: "lookup", description: "First line.\nSecond line.", inputSchema: { type: "object", required: ["id"], properties: { id: { type: "string" } } }, outputSchema: { type: "object" }, annotations: { readOnlyHint: true }, _meta: { fixture: true } }} />);
  for (const value of ["full tool definition", "Second line", "required", "properties", "outputSchema", "readOnlyHint", "_meta", "fixture"]) expect(html).toContain(value);
});

test("callback HTTP error does not appear as waiting or confirmed and offers recovery", async () => {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  await client.fetchQuery({ queryKey: ["mcp-login", flow.flow_id], queryFn: () => Promise.reject(new Error("No such login, or it expired")) }).catch(() => {});
  const html = render(<LoginFlow flow={flow} onDone={() => {}} />, client);
  for (const value of ["Could not check sign-in status", "No such login, or it expired", "Sign-in has not been confirmed", "Retry status", "start a new sign-in"]) expect(html).toContain(value);
  expect(html).not.toContain("Waiting for the browser");
});

test("OAuth terminal rejection is actionable and unusable tokens can be forgotten", () => {
  const client = new QueryClient();
  client.setQueryData(["mcp-login", flow.flow_id], { status: "failed", error: "Authorization denied" });
  expect(render(<LoginFlow flow={flow} onDone={() => {}} />, client)).toContain("Sign-in failed: Authorization denied");
  const oauth: McpOAuthStatus = { signed_in: false, signed_in_at: null, expires_at: null, refreshable: false, scopes: [], registration: null, issuer: null, needs_login: "Stored token was rejected", unsaved: false };
  const html = render(<OAuthLogin server="archive" oauth={oauth} />);
  expect(html).toContain("Stored token was rejected");
  expect(html).toContain("Forget tokens");
});

test("secret controls allow arbitrary field names and expose confirmed removal", () => {
  const arbitrary = render(<SecretField server="archive" />);
  expect(arbitrary).toContain("Other secret field name");
  expect(arbitrary).toContain('type="password"');
  expect(arbitrary).toContain("Save secret");
  expect(arbitrary).toContain("Remove secret…");
  const required = render(<SecretField server="archive" field="token" set />);
  expect(required).toContain("{token}");
  expect(required).toContain("Remove secret…");
});
