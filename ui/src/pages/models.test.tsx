// ---
// relationships:
//   tests: models
// ---
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { renderToStaticMarkup } from "react-dom/server";
import { MemoryRouter } from "react-router";
import { expect, test } from "vitest";
import type { CatalogPricing, CatalogRoute } from "@/lib/api";
import { ModelsPage } from "./models";

const success: CatalogPricing = { enabled: true, attempted_at: 100, fetched_at: 100, age_secs: 7200, error: null };

function render(metadata: CatalogRoute["metadata"], pricing: CatalogPricing = success, empty = false, refreshing = false) {
  const client = new QueryClient({ defaultOptions: { queries: { staleTime: Infinity, retry: false } } });
  client.setQueryData(["catalog-status"], {});
  client.setQueryData(["catalog-pricing"], pricing);
  client.setQueryData(["catalog-routes"], empty ? [] : [{provider: "sample", model: "model-a", endpoints: ["/v1/chat/completions"], translated_endpoints: [], metadata}]);
  if (refreshing) void client.getMutationCache().build(client, { mutationKey: ["catalog-refresh"], mutationFn: async () => undefined }).execute(undefined);
  return renderToStaticMarkup(<QueryClientProvider client={client}><MemoryRouter><ModelsPage /></MemoryRouter></QueryClientProvider>);
}

test("catalog exposes limits, USD prices, cache rates, source and stale data", () => {
  const html = render({max_input_tokens: 128000, max_output_tokens: 8000, input_cost_per_token: 0.000002, output_cost_per_token: 0.000008, cache_read_input_token_cost: 0, cache_creation_input_token_cost: 0.0000025, source: "LiteLLM:sample/model-a", fetched_at: 1, age_secs: 10, error: "fetch failed"});
  for (const text of ["128K", "8K", "$2.0000", "$8.0000", "Cache read", "$0.00", "$2.50", "LiteLLM:sample/model-a", "ago", "Pricing refresh failed; see reason above", "API-equivalent / Mtok"]) expect(html).toContain(text);
});

test("pricing success is independent of provider discovery and catalog rows", () => {
  const html = render(undefined, success, true);
  for (const text of ["Provider discovery", "Pricing refresh succeeded", "Latest attempt:", "Last good registry fetch:", "catalog is empty"]) expect(html).toContain(text);
  expect(html).not.toContain("Latest pricing refresh failed");
});

test("return visits show the latest background failure, underlying reason and cached-price age", () => {
  const html = render(undefined, { ...success, error: "LiteLLM registry fetch failed: request timed out" }, true);
  for (const text of ["Latest pricing refresh failed", "Latest attempt:", "request timed out", "Last good registry fetch:", "ago", "Cached prices retained", "Cached registry prices are 2 hours old", "Retry Refresh models"]) expect(html).toContain(text);
  expect(html).not.toContain("Pricing refresh succeeded");
});

test("failed first fetch does not invent a last-good time or cached prices", () => {
  const html = render(undefined, { ...success, fetched_at: null, age_secs: null, error: "registry returned 503" }, true);
  expect(html).toContain("registry returned 503");
  expect(html).toContain("No cached registry prices are available");
  expect(html).not.toContain("Last good registry fetch:");
  expect(html).not.toContain("Cached prices retained");
});

test("no attempt and disabled refresh have distinct states", () => {
  const initial = { ...success, attempted_at: null, fetched_at: null, age_secs: null };
  expect(render(undefined, initial)).toContain("Prices have not been fetched");
  expect(render(undefined, { ...initial, enabled: false })).toContain("Registry refresh is disabled");
  expect(render(undefined, { ...success, attempted_at: null })).toContain("Prices loaded from cache");
});

test("refreshing disables repeat submissions and labels pricing work", () => {
  const html = render(undefined, { ...success, error: "previous outage" }, false, true);
  expect(html).toContain("Refreshing models…");
  expect(html).toContain("Refreshing prices…");
  expect(html).toContain('disabled=""');
  expect(html).toContain("Cached registry prices are 2 hours old");
  expect(html).not.toContain("previous outage");
  expect(html).not.toContain("Pricing refresh succeeded");
});

test("unknown metadata stays unknown and unpriced", () => {
  const html = render(undefined);
  expect(html).toContain("Unknown"); expect(html).toContain("No data"); expect(html).not.toContain("$0.00");
});
