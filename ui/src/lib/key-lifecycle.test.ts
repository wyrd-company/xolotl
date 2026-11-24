// ---
// relationships:
//   tests: key-lifecycle
// ---
import { expect, test } from "vitest";
import { keyDuration, revokeMessage } from "./key-lifecycle";

test("custom key durations support CLI units and exact seconds", () => {
  for (const [value, seconds] of [
    ["36h", 129600],
    [" 2w ", 1209600],
    ["15m", 900],
    ["7d", 604800],
    ["45s", 45],
    ["37", 37],
  ] as const) {
    expect(keyDuration(value)).toBe(seconds);
  }
  expect(keyDuration("0s", true)).toBe(0);
});

test("expiry cannot be zero; grace can, and malformed or inexact values are rejected", () => {
  expect(() => keyDuration("0s")).toThrow("positive duration");
  for (const value of ["", "36hours", "-1h", "1.5h", "Infinity", "1e3", "9007199254740992s", "9007199254740991w"]) {
    expect(() => keyDuration(value)).toThrow();
    expect(() => keyDuration(value, true)).toThrow();
  }
});

test("revocation reports the server outcome", () => {
  expect(revokeMessage({ revoked: "team/editor", status: "revoked" })).toBe("Revoked team/editor.");
  expect(revokeMessage({ revoked: "team/editor", status: "already_revoked" })).toBe("Already revoked team/editor.");
  expect(revokeMessage({ revoked: "team/missing", status: "absent" })).toBe("No key found for team/missing.");
});
