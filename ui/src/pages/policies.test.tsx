// ---
// relationships:
//   tests: policies
// ---
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { renderToStaticMarkup } from "react-dom/server";
import { MemoryRouter, Route, Routes } from "react-router";
import { afterEach, expect, test, vi } from "vitest";
import type { Policy, UnloadedPolicy } from "@/lib/api";
import { PoliciesPage } from "./policies";

const policy: Policy = {
  name: "sample",
  providers: [],
  scopes: ["proxy"],
};

function render(path: string, withKeys = false, unloaded: UnloadedPolicy[] = []) {
  vi.stubGlobal("localStorage", { getItem: () => null });
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false, staleTime: Infinity } },
  });
  client.setQueryData(["policies"], [policy]);
  client.setQueryData(["status"], { unloaded_policies: unloaded });
  client.setQueryData(["keys"], withKeys ? [{ policy: "sample" }] : []);
  client.setQueryData(["providers"], []);
  client.setQueryData(["accounts"], []);
  client.setQueryData(["mcp-servers"], []);
  client.setQueryData(["catalog-status"], {});
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
