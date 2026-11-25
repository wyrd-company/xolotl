import { expect, test } from "vitest";
import { normalizePriority, priorityError } from "./priority";

test.each(["-9223372036854775808", "9223372036854775807", "9007199254740993", "-9007199254740993", "0", " -0017 "])("accepts exact signed priority %s", (input) => {
  expect(priorityError(input)).toBeNull();
  expect(JSON.parse(JSON.stringify({ priority: normalizePriority(input) })).priority).toBe(BigInt(input.trim()).toString());
});

test.each(["9223372036854775808", "-9223372036854775809", "1.5", "1e3", "", " ", "-", "+1", "NaN", "Infinity", "0x10"])("rejects invalid priority %s", (input) => {
  expect(priorityError(input)).toContain("whole number");
});
