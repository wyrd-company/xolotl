// ---
// relationships:
//   tests: reset-credits
// ---
import { expect, test } from "vitest";
import { resetCreditsSummary } from "./reset-credits";

const account = (count: number | null | undefined) => ({ usage: { account_id: "example", fetched_at: 0, windows: [], raw: { reset_credits: count } } });

test("sums the whole pool, including disabled and cooling accounts", () => {
  expect(resetCreditsSummary([account(1)])).toBe("1 reset credit available");
  expect(resetCreditsSummary([account(0)])).toBe("0 reset credits available");
  const pool = [account(1), { ...account(2), enabled: false }, { ...account(3), cooldown_until: 9999999999 }];
  expect(resetCreditsSummary(pool)).toBe("6 reset credits available");
});

test("distinguishes unknown counts from zero and labels partial totals", () => {
  expect(resetCreditsSummary([])).toBe("Reset credits unknown");
  expect(resetCreditsSummary([{ usage: null }, account(null)])).toBe("Reset credits unknown");
  expect(resetCreditsSummary([account(0), { usage: { ...account(0).usage, raw: null } }])).toBe("0 reset credits available · 1 account unknown");
  expect(resetCreditsSummary([account(2), account(undefined)])).toBe("2 reset credits available · 1 account unknown");
  expect(resetCreditsSummary([account(2), account(undefined), { usage: null }])).toBe("2 reset credits available · 2 accounts unknown");
});
