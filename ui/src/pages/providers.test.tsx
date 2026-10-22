// ---
// relationships:
//   tests: providers
// ---
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { renderToStaticMarkup } from "react-dom/server";
import { MemoryRouter } from "react-router";
import { expect, test } from "vitest";
import type { CatalogRoute } from "@/lib/api";
import { ProvidersPage } from "./providers";

function render(metadata: CatalogRoute["metadata"]) {
  const client = new QueryClient({ defaultOptions: { queries: { staleTime: Infinity, retry: false } } });
  client.setQueryData(["providers"], []);
  client.setQueryData(["catalog-status"], {});
  client.setQueryData(["catalog-routes"], [{provider: "sample", model: "model-a", endpoints: ["/v1/chat/completions"], translated_endpoints: [], metadata}]);
  return renderToStaticMarkup(<QueryClientProvider client={client}><MemoryRouter><ProvidersPage /></MemoryRouter></QueryClientProvider>);
}

test("catalog exposes limits, USD prices, cache rates, source and stale data", () => {
  const html = render({max_input_tokens: 128000, max_output_tokens: 8000, input_cost_per_token: 0.000002, output_cost_per_token: 0.000008, cache_read_input_token_cost: 0, cache_creation_input_token_cost: 0.0000025, source: "LiteLLM:sample/model-a", fetched_at: 1, age_secs: 10, error: "fetch failed"});
  for (const text of ["128K", "8K", "$2.00", "$8.00", "Cache read", "$0.00", "$2.50", "LiteLLM:sample/model-a", "ago", "Refresh failed; cached data retained", "API-equivalent / Mtok"]) expect(html).toContain(text);
});

test("unknown metadata stays unknown and unpriced", () => {
  const html = render(undefined);
  expect(html).toContain("Unknown"); expect(html).toContain("No data"); expect(html).not.toContain("$0.00");
});
