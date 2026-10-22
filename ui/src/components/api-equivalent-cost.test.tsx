// ---
// relationships:
//   tests: api-equivalent-cost
// ---
import { renderToStaticMarkup } from "react-dom/server";
import { expect, test } from "vitest";
import type { UsageRow } from "@/lib/api";
import { ApiEquivalentCost } from "./api-equivalent-cost";
const row: UsageRow = {key: "sample", requests: 2, errors: 0, input_tokens: 20, output_tokens: 10, cached_tokens: 0};
test("cost exposes partial pricing coverage without claiming full spend", () => {
  const html = renderToStaticMarkup(<ApiEquivalentCost rows={[{...row, api_equivalent_cost: 1.25, priced_requests: 1}]} />);
  expect(html).toContain("$1.2500"); expect(html).toContain("1/2 priced");
});
test("absent prices differ from known free usage", () => {
  expect(renderToStaticMarkup(<ApiEquivalentCost rows={[row]} />)).toContain("Unknown");
  expect(renderToStaticMarkup(<ApiEquivalentCost rows={[{...row, api_equivalent_cost: 0, priced_requests: 2}]} />)).toContain("$0.0000");
});
