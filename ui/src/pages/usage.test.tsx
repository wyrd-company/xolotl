import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { renderToStaticMarkup } from "react-dom/server";
import { MemoryRouter } from "react-router";
import { expect, test } from "vitest";
import { UsagePage } from "./usage";

function render(search: string, period: number | null, filters: [string, string][]) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false, staleTime: Infinity } } });
  client.setQueryData(["usage", "client", period, filters, false], []);
  client.setQueryData(["sessions", filters.find(([g]) => g === "client")?.[1]], []);
  client.setQueryData(["tools", filters.find(([g]) => g === "client")?.[1]], []);
  return renderToStaticMarkup(<QueryClientProvider client={client}><MemoryRouter initialEntries={[`/usage${search}`]}><UsagePage /></MemoryRouter></QueryClientProvider>);
}

test("URL filters populate direct entry even with no matching rows", () => {
  const html = render("?since=129600&client=&model=missing-model", 129600, [["client", ""], ["model", "missing-model"]]);
  expect(html).toContain('value="36h"');
  expect(html).toContain('value="missing-model"');
  expect(html).toContain("admin token");
  expect(html).toContain("No matching requests in this period");
  for (const name of ["client", "account", "provider", "model"]) expect(html).toContain(`Filter by ${name}`);
});

test("an invalid bookmarked duration is visible and does not silently become 24h", () => {
  const html = render("?since=bad", null, []);
  expect(html).toContain('value="bad"');
  expect(html).toContain("Apply a valid look-back period to load usage");
});
