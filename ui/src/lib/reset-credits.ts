// ---
// relationships:
//   references: ../../../docs/user/accounts.md
// ---
import type { Account, AccountUsage } from "./api";

export function resetCredits(usage: AccountUsage | null): number | undefined {
  const raw = usage?.raw;
  if (!raw || typeof raw !== "object" || !("reset_credits" in raw)) return undefined;
  const count = raw.reset_credits;
  return typeof count === "number" && Number.isSafeInteger(count) && count >= 0 ? count : undefined;
}

/** A partial pool total must not imply unknown accounts have zero credits. */
export function resetCreditsSummary(pool: Pick<Account, "usage">[]): string {
  const known = pool.map((a) => resetCredits(a.usage)).filter((count) => count !== undefined);
  if (known.length === 0) return "Reset credits unknown";
  const total = known.reduce((sum, count) => sum + count, 0);
  const label = `${total} reset credit${total === 1 ? "" : "s"} available`;
  const unknown = pool.length - known.length;
  return unknown === 0 ? label : `${label} · ${unknown} account${unknown === 1 ? "" : "s"} unknown`;
}
