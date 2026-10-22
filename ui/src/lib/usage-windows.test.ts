// ---
// relationships:
//   tests: usage-windows
// ---
import { expect, test } from "vitest";
import { publicUsageWindows } from "./usage-windows";

const window = (name: string) => ({ name, used_percent: 25, resets_at: null, window_secs: null });

test("hides the internal window in old snapshots while preserving public and scoped windows", () => {
  const windows = [window("5h"), window("iguana_necktie"), window("7d-fable"), window("future_public")];
  expect(publicUsageWindows(windows)).toEqual([windows[0], windows[2], windows[3]]);
  expect(windows).toHaveLength(4);
  expect(publicUsageWindows(undefined)).toEqual([]);
});
