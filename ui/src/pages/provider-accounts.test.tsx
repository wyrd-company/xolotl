import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { ReactNode } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { MemoryRouter } from "react-router";
import { expect, test } from "vitest";

import type { ProviderAccount } from "@/lib/api";
import { ProviderAccounts, ProviderKeyOutcome } from "./provider-accounts";

function render(node: ReactNode) {
  return renderToStaticMarkup(
    <QueryClientProvider client={new QueryClient()}>
      <MemoryRouter>{node}</MemoryRouter>
    </QueryClientProvider>,
  );
}

test("provider key actions are available across all provider states", () => {
  const base: ProviderAccount = {
    name: "gateway",
    kind: "openai-compatible",
    source: "config",
    enabled: true,
    base_url: null,
    no_auth: false,
    headers: [],
    secret: "providers/gateway",
    has_key: false,
    workload_identity: false,
  };
  const providers = [
    { ...base, has_key: true },
    { ...base, name: "disabled", enabled: false },
    { ...base, name: "no-auth", no_auth: true },
    { ...base, name: "federated", workload_identity: true },
  ];
  const html = render(<ProviderAccounts providers={providers} loaded />);
  expect(html).toContain('aria-label="Replace key for gateway"');
  for (const name of ["disabled", "no-auth", "federated"]) {
    expect(html).toContain(`aria-label="Set key for ${name}"`);
  }
  expect(render(<ProviderAccounts providers={[]} loaded />)).toContain("Add a provider account");
});

test("a key save reports the returned model count, including zero", () => {
  const html = render(<ProviderKeyOutcome provider="gateway" discovery={{ provider: "gateway", models: 0 }} />);
  expect(html).toContain('role="status"');
  expect(html).toContain("Key saved for gateway");
  expect(html).toContain("0 models found.");
  expect(html).not.toContain("Model discovery failed");
});

test("a key save without discovery still confirms the write", () => {
  const html = render(<ProviderKeyOutcome provider="disabled" discovery={null} />);
  expect(html).toContain("Key saved for disabled");
  expect(html).toContain("No model discovery result");
  expect(html).not.toContain("Refresh models");
});

test("failed discovery shows the upstream reason and a retry alongside the successful write", () => {
  const html = render(<ProviderKeyOutcome provider="gateway" discovery={{ provider: "gateway", error: "Upstream returned 503" }} />);
  expect(html).toContain("Key saved for gateway");
  expect(html).toContain('role="alert"');
  expect(html).toContain("Model discovery failed");
  expect(html).toContain("Upstream returned 503");
  expect(html).toContain("cached or static models remain available");
  expect(html).toContain("Refresh models");
  expect(html).not.toContain("models found");
});
