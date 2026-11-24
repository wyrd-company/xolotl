// ---
// relationships:
//   tests: keys
// ---
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { renderToStaticMarkup } from "react-dom/server";
import { expect, test } from "vitest";
import type { KeyInfo } from "@/lib/api";
import { KeysPage } from "./keys";

const key: KeyInfo = {
  client_id: "team/editor",
  policy: "team",
  name: "editor",
  description: "Editor workstation",
  vault_path: "keys/team/editor",
  created_at: "2026-11-20T10:00:00Z",
  rotated_at: null,
  expires_at: null,
  disabled: false,
  usage: {
    requests_per_minute: 12,
    requests_per_day: 320,
    tokens_per_minute: 8400,
    tokens_per_day: 82000,
  },
};
function render(keys: KeyInfo[], failed: string[] = []) {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false, staleTime: Infinity } },
  });
  client.setQueryData(["keys"], keys);
  client.setQueryData(["policies"], [{ name: "team" }]);
  client.setQueryData(["status"], {
    vault_mount: "custom-mount",
    vault_prefix: "custom-prefix",
  });
  client.setQueryData(["usage", "client", 604800], [{ key: "team/editor", requests: 1200 }]);
  for (const name of failed) {
    const query = client.getQueryCache().find({ queryKey: [name], exact: false })!;
    query.setState({
      status: "error",
      error: new Error(`${name} failed: unavailable`),
    });
  }
  return renderToStaticMarkup(
    <QueryClientProvider client={client}>
      <KeysPage />
    </QueryClientProvider>,
  );
}

test("all current counters are visible without policy limits and timestamps have a creation fallback", () => {
  const html = render([key]);
  for (const value of [
    "12 req/min",
    "320 req/day",
    "8,400 tok/min",
    "82,000 tok/day",
    "1,200",
    "Rotation reference: created",
    "never rotated",
  ])
    expect(html).toContain(value);
  expect(html).toContain("Default expiry: Never");
  expect(html).toContain("Revoke by client ID");
});

test("rotation grace preserves descriptions and creation and rotation dates", () => {
  const html = render([
    {
      ...key,
      rotated_at: "2026-11-24T07:00:00Z",
      previous_expires_at: "2999-01-01T00:00:00Z",
    },
  ]);
  for (const value of ["Editor workstation", "Old key in grace", "Grace ends", "Created", "Rotated"]) expect(html).toContain(value);
  expect(html).not.toContain("never rotated");
});

test("limited windows use policy totals while unbounded windows retain key counters", () => {
  const html = render([
    {
      ...key,
      limits: { requests_per_minute: 60, per: "policy" },
      limit_usage: {
        requests_per_minute: 42,
        requests_per_day: 999,
        tokens_per_minute: 999,
        tokens_per_day: 999,
      },
    },
  ]);
  expect(html).toContain("42/60 req/min");
  expect(html).toContain("320 req/day");
  expect(html).toContain("8,400 tok/min");
  expect(html).toContain("limited windows shared by the policy");
});

test("four realistic shared windows keep exact used and limit values in the row", () => {
  const html = render([
    {
      ...key,
      limits: { requests_per_minute: 600, requests_per_day: 10000, tokens_per_minute: 200000, tokens_per_day: 10000000, per: "policy" },
      limit_usage: { requests_per_minute: 42, requests_per_day: 3456, tokens_per_minute: 123456, tokens_per_day: 2345678 },
    },
  ]);
  const text = html.replace(/<[^>]*>/g, "");
  for (const value of ["42/600 req/min", "3,456/10,000 req/day", "123,456/200,000 tok/min", "2,345,678/10,000,000 tok/day"]) {
    expect(text).toContain(value);
    expect(html).toContain(`title="${value}"`);
  }
});

test("failed queries show causes and retry controls and do not turn usage into zero", () => {
  const html = render([key], ["status", "usage", "policies"]);
  for (const value of [
    "status failed: unavailable",
    "usage failed: unavailable",
    "policies failed: unavailable",
    "Retry",
    "Unavailable",
    "Vault retrieval details are unavailable",
    "Key issuance is unavailable",
  ])
    expect(html).toContain(value);
  expect(html).not.toContain(">1,200<");
  expect(html).toMatch(/type="submit" disabled=""/);
});

test("missing counters stay unavailable rather than being fabricated as zero", () => {
  const html = render([{ ...key, usage: undefined }]);
  expect(html).toContain("unavailable req/min");
  expect(html).not.toContain(">0 req/min");
});
