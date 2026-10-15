// ---
// relationships:
//   references: ../../../docs/subscriptions.md
// ---
import type { Account } from "./api";

/** A partial pool total must not imply unknown accounts have zero credits. */
export function resetCreditsSummary(pool: Pick<Account, "usage">[]): string {
  const counts = pool.map((a) => a.usage?.raw?.reset_credits);
  const known = counts.filter((count): count is number => count != null);
  if (known.length === 0) return "Reset credits unknown";
  const total = known.reduce((sum, count) => sum + count, 0);
  const label = `${total} reset credit${total === 1 ? "" : "s"} available`;
  const unknown = pool.length - known.length;
  return unknown === 0 ? label : `${label} · ${unknown} account${unknown === 1 ? "" : "s"} unknown`;
}
