import assert from "node:assert/strict";
import { test } from "vitest";

import { highlighted, suggestions } from "../src/lib/suggest.ts";

const options = ["llama/a", "llama/model", "llama/model-plus"];

test("suggestions leave out the value and match the typed text", () => {
  assert.deepEqual(suggestions(options, ""), options);
  assert.deepEqual(suggestions(options, "MODEL"), ["llama/model", "llama/model-plus"]);
  assert.deepEqual(suggestions(options, "llama/model"), ["llama/model-plus"], "the picked value is not offered again");
  assert.equal(suggestions(Array.from({ length: 20 }, (_, i) => `m${i}`), "").length, 8);
});

test("the highlighted index stays within a list that shrank under it", () => {
  // ArrowDown then Enter picks llama/model at index 1; reopening shows one option.
  const shown = suggestions(options, "llama/model");
  assert.equal(shown[highlighted(1, shown)], "llama/model-plus");
  assert.equal(highlighted(5, options), 2);
  assert.equal(highlighted(-1, options), 0);
  assert.equal(highlighted(0, []), -1, "nothing to pick");
});
