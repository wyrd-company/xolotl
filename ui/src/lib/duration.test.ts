import { expect, test } from "vitest";
import { durationSeconds, formatDuration } from "./duration";

test.each([["90", 90], ["30s", 30], ["15m", 900], ["36h", 129600], ["7d", 604800], ["2w", 1209600], [" 3h ", 10800]])("converts CLI-style duration %s to seconds", (value, seconds) => {
  expect(durationSeconds(value as string)).toBe(seconds);
});

test.each(["", "0", "0s", "-2h", "1.5h", "36hours", "1e3", "Infinity", "2H", "9007199254740992", "9007199254740991w"])("rejects invalid or unsafe duration %s", (value) => {
  expect(durationSeconds(value)).toBeNull();
});

test.each([1, 90, 3600, 129600, 604800, 9007199254740991])("formats %s seconds without changing the window", (seconds) => {
  expect(durationSeconds(formatDuration(seconds))).toBe(seconds);
});
