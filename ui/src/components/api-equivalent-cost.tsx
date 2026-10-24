// ---
// relationships:
//   implements: ../../../docs/user/web-ui.md
// ---
import type { UsageRow } from "@/lib/api";

export function ApiEquivalentCost({ rows }: { rows: UsageRow[] }) {
  const priced = rows.reduce((n, r) => n + (r.priced_requests ?? 0), 0);
  const requests = rows.reduce((n, r) => n + r.requests, 0);
  const cost = rows.reduce((n, r) => n + (r.api_equivalent_cost ?? 0), 0);
  return <span>{priced ? `$${cost.toFixed(4)}` : "Unknown"}{priced < requests && <span className="text-muted-foreground"> · {priced}/{requests} priced</span>}</span>;
}
