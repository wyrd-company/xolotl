import { renderToStaticMarkup } from "react-dom/server";
import { afterEach, expect, test, vi } from "vitest";
import type { PolicyView } from "@/lib/api";
import { PolicyInspection, SavedPolicyWarnings } from "./policy-inspection";
import { PolicyQueryError } from "./policies";

const view: PolicyView = {
  policy: "sample", accounts: { codex: ["codex-one"], claude: [] }, catalog: [], shared: [], mcp_servers: [], tools: [],
  usage: { requests_per_minute: 0, requests_per_day: 0, tokens_per_minute: 0, tokens_per_day: 0 },
  models: [{ name: "fast", alias: true, targets: ["codex/first", "claude/second"], endpoints: ["/v1/responses"], translated_endpoints: ["/v1/messages", "/v1/chat/completions"] }],
};
const render = (data = view, error: Error | null = null) => {
  vi.stubGlobal("localStorage", { getItem: () => null });
  return renderToStaticMarkup(<PolicyInspection view={data} error={error} pending={false} retry={() => {}} />);
};
afterEach(() => vi.unstubAllGlobals());

test("inspection shows alias routing order, both endpoint kinds and explicit empty pools", () => {
  const html = render();
  for (const text of ["fast", "alias", "Native endpoints", "Translated endpoints", "/v1/responses", "/v1/messages", "/v1/chat/completions", "codex-one", "No accounts"]) expect(html).toContain(text);
  expect(html.indexOf("codex/first")).toBeLessThan(html.indexOf("claude/second"));
});
test("account read failure is distinct from empty accounts and preserves resolved routes", () => {
  const html = render({ ...view, accounts: null });
  expect(html).toContain("Could not read the accounts");
  expect(html).toContain("Retry accounts");
  expect(html).toContain("codex/first");
  expect(html).not.toContain("No subscription providers allowed.");
});
test("management-only preview has no routes or subscription providers", () => {
  const html = render({ ...view, accounts: {}, models: [] });
  expect(html).toContain("No resolved model routes.");
  expect(html).toContain("No subscription providers allowed.");
  expect(html).not.toContain("Could not read");
});
test("failed preview shows its cause and retry instead of cached routes", () => {
  const html = render(view, new Error("unknown provider fixture"));
  expect(html).toContain("unknown provider fixture");
  expect(html).toContain("Retry");
  expect(html).not.toContain("codex/first");
});
test("save warnings show every server warning as text and stay separate from save failure", () => {
  const html = renderToStaticMarkup(<SavedPolicyWarnings warnings={["No accounts for codex", "<server detail>"]} />);
  expect(html).toContain("Policy saved with warnings");
  expect(html).toContain("No accounts for codex");
  expect(html).toContain("&lt;server detail&gt;");
  expect(renderToStaticMarkup(<SavedPolicyWarnings warnings={[]} />)).toBe("");
});
test("unloaded-policy status failure exposes the cause and retry", () => {
  const html = renderToStaticMarkup(<PolicyQueryError title="Could not check for unloaded policies" error={new Error("fixture status unavailable")} pending={false} retry={() => {}} />);
  expect(html).toContain("Could not check for unloaded policies");
  expect(html).toContain("fixture status unavailable");
  expect(html).toContain("Retry");
});
