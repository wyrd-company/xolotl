import { expect, test } from "vitest";
import { price } from "./format";

test("per-Mtok prices distinguish unknown, free and small nonzero rates", () => {
  expect(price(null)).toBe("Unknown");
  expect(price(undefined)).toBe("Unknown");
  expect(price(0)).toBe("$0.0000");
  expect(price(0.000002)).toBe("$2.0000");
  expect(price(0.000000005, 2)).toBe("$0.005");
  expect(price(0.0000000000005, 2)).toBe("$0.0000005");
});
