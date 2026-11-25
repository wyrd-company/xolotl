// ---
// relationships:
//   tests: policies
// ---
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { renderToStaticMarkup } from "react-dom/server";
import { MemoryRouter, Route, Routes } from "react-router";
import { afterEach, expect, test, vi } from "vitest";
import { ApiError, type Policy, type UnloadedPolicy } from "@/lib/api";
import { PoliciesPage, toDraft, toPolicy } from "./policies";

const policy: Policy = {
  name: "sample",
  providers: [],
  scopes: ["proxy"],
};

function render(path: string, withKeys = false, unloaded: UnloadedPolicy[] = [], stored: Policy = policy, keysFailed = false, previewFailure?: Error) {
  vi.stubGlobal("localStorage", { getItem: () => null });
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false, retryOnMount: false, staleTime: Infinity } },
  });
  client.setQueryData(["policies"], [stored]);
  client.setQueryData(["status"], { unloaded_policies: unloaded });
  client.setQueryData(["keys"], withKeys ? [{ policy: "sample" }] : []);
  if (keysFailed) client.getQueryCache().find({ queryKey: ["keys"] })!.setState({ status: "error", error: new Error("keys unavailable") });
  client.setQueryData(["providers"], []);
  client.setQueryData(["accounts"], []);
  client.setQueryData(["mcp-servers"], []);
  client.setQueryData(["catalog-status"], {});
  if (previewFailure) {
    client.getQueryCache().build(client, { queryKey: ["policy-view", toPolicy(toDraft(path === "/policies/+new" ? null : stored))] })
      .setState({ status: "error", error: previewFailure });
  }
  return renderToStaticMarkup(
    <QueryClientProvider client={client}>
      <MemoryRouter initialEntries={[path]}>
        <Routes>
          <Route path="/policies/:name" element={<PoliciesPage />} />
        </Routes>
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

afterEach(() => vi.unstubAllGlobals());

test("lists stored policies that did not load", () => {
  const html = render("/policies/other", false, [{ name: "other", error: "unknown field `extra`" }]);
  expect(html).toContain("Stored policies did not load");
  expect(html).toContain("other");
  expect(html).toContain("unknown field `extra`");
  expect(html).not.toContain("No policy named");
});

test("shows no warning when every stored policy loaded", () => {
  const html = render("/policies/sample");
  expect(html).not.toContain("Stored policies did not load");
});

test("unloaded policy offers confirmed deletion when no keys use it", () => {
  const html = render("/policies/other", false, [{ name: "other", error: "bad policy" }]);
  expect(html).toMatch(/<button[^>]*aria-label="Delete unloaded policy other"[^>]*aria-haspopup="dialog"/);
  expect(html).toContain("Delete policy…");
});

test("unloaded policy deletion retains the key-use restriction", () => {
  const html = render("/policies/sample", true, [{ name: "sample", error: "bad policy" }]);
  expect(html).toMatch(/<button[^>]*disabled=""[^>]*aria-label="Delete unloaded policy sample"/);
  expect(html).toContain("A policy with keys can&#x27;t be deleted.");
});

test("a failed key check disables loaded and unloaded policy deletion", () => {
  const html = render("/policies/sample", false, [{ name: "other", error: "bad policy" }], policy, true);
  expect(html).toMatch(/<button[^>]*disabled=""[^>]*aria-label="Delete unloaded policy other"/);
  expect(actions(html)).toMatch(/<button[^>]*disabled=""[^>]*aria-haspopup="dialog"[^>]*>Delete/);
  expect(html).toContain("Could not check which policies have keys");
  expect(html).toContain("keys unavailable");
});

function actions(html: string) {
  const form = html.slice(html.indexOf("<form"), html.indexOf("</form>"));
  const start = form.indexOf('aria-label="Policy actions"');
  expect(start).toBeGreaterThan(0);
  const end = form.indexOf("</div>", start);
  // Actions are inside the form, before its first editable section.
  expect(end).toBeLessThan(form.indexOf('id="p-desc"'));
  expect(form.match(/type="submit"/g)).toHaveLength(1);
  return form.slice(start, end);
}

test("existing policy has one save action at the top and keeps delete confirmation", () => {
  const html = render("/policies/sample");
  const bar = actions(html);
  expect(bar).toMatch(/<button[^>]*type="submit"[^>]*disabled=""[^>]*>Save policy/);
  expect(bar).toMatch(/aria-haspopup="dialog"[^>]*>Delete/);
  expect(bar).toContain("policies/sample");
  expect(bar).not.toContain("Discard changes");
});

test("moving actions preserves the restriction on deleting a policy with keys", () => {
  const bar = actions(render("/policies/sample", true));
  expect(bar).toMatch(/<button[^>]*disabled=""[^>]*aria-haspopup="dialog"[^>]*>Delete/);
  expect(bar).toContain("A policy with keys can&#x27;t be deleted.");
});

test("new policy uses the same top actions and requires a name before creating", () => {
  const bar = actions(render("/policies/+new"));
  expect(bar).toMatch(/<button[^>]*type="submit"[^>]*disabled=""[^>]*>Create policy/);
  expect(bar).not.toContain(">Delete");
});

test("untouched New policy suppresses cached draft validation and retry failures", () => {
  const message = "policy must allow at least one provider or MCP server";
  const html = render("/policies/+new", false, [], policy, false, new ApiError(400, message));
  expect(html).not.toContain(message);
  expect(html).not.toContain("Could not resolve policy");
  expect(html).not.toContain(">Retry<");
  expect(html).toContain("Configure the policy to preview its routes.");
});

test("a draft validation error is shown once without a retryable resolution alert", () => {
  const message = "unknown provider fixture";
  const html = render("/policies/sample", false, [], { ...policy, providers: ["codex"] }, false, new ApiError(400, message));
  expect(html.split(message)).toHaveLength(2);
  expect(html).not.toContain("Could not resolve policy");
  expect(html).not.toContain(">Retry<");
});

const issuer: Policy = { name: "sample", providers: [], scopes: ["keys"], key_policies: ["first", "second"] };

test("a keys policy shows its key policies", () => {
  const html = render("/policies/sample", false, [], issuer);
  expect(html).toContain("keys (issue client keys)");
  expect(html).toContain('id="p-key-policies"');
  expect(html).toContain("first\nsecond");
});

test("key policies are asked for only under the keys scope", () => {
  expect(render("/policies/sample")).not.toContain('id="p-key-policies"');
});

test("saving a policy keeps its key policies", () => {
  expect(toPolicy(toDraft(issuer)).key_policies).toEqual(["first", "second"]);
  expect(toPolicy({ ...toDraft(issuer), keyPolicies: " first \n\n" }).key_policies).toEqual(["first"]);
});

const warning = (provider: string) => `No accounts checked, so keys under this policy cannot use ${provider}.`;

test("warns about an allowed subscription provider with no accounts", () => {
  const html = render("/policies/sample", false, [], { ...policy, providers: ["claude", "codex"], accounts: { codex: ["*"] } });
  expect(html).toContain(warning("claude"));
  expect(html).not.toContain(warning("codex"));
  expect(html).not.toContain(warning("grok"));
});

test("warns for every subscription provider under the wildcard", () => {
  const html = render("/policies/sample", false, [], { ...policy, providers: ["*"], accounts: { codex: ["*"] } });
  expect(html).toContain(warning("claude"));
  expect(html).not.toContain(warning("codex"));
});

test("warns under the admin scope, which also calls providers", () => {
  const html = render("/policies/sample", false, [], { ...policy, providers: ["claude"], scopes: ["admin"] });
  expect(html).toContain(warning("claude"));
});

test("does not warn when the policy's keys call no provider", () => {
  const html = render("/policies/sample", false, [], { ...policy, providers: ["claude"], scopes: ["mcp"] });
  expect(html).not.toContain(warning("claude"));
});
