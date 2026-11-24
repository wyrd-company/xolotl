// ---
// relationships:
//   tests: usage-view
// ---
import { describe, expect, it } from "vitest";

import { narrowTo, readView } from "@/lib/usage-view";

const params = (search: string) => new URLSearchParams(search.replace(/^\?/, ""));

describe("narrowTo", () => {
  it("narrows through every grouping until all four are filtered", () => {
    let view = params("group=client&since=604800");
    const steps: [string, string][] = [];
    for (const key of ["c1", "m1", "a1", "p1"]) {
      const to = narrowTo(view, key);
      expect(to).not.toBeNull();
      view = params(to!);
      steps.push([readView(view).group, key]);
    }
    expect(steps.map(([g]) => g)).toEqual(["model", "account", "provider", "provider"]);
    expect(readView(view)).toEqual({
      group: "provider",
      period: 604800,
      filters: [
        ["client", "c1"],
        ["account", "a1"],
        ["provider", "p1"],
        ["model", "m1"],
      ],
    });
    expect(narrowTo(view, "p1")).toBeNull();
  });

  it("keeps rows of an unfiltered grouping selectable when others are filtered", () => {
    expect(narrowTo(params("group=client&model=m1&account=a1&provider=p1"), "c1")).not.toBeNull();
  });

  it("stops at a grouping that is already filtered", () => {
    expect(narrowTo(params("group=client&client=c1"), "c1")).toBeNull();
  });

  it("narrows to a row without a key with an empty value", () => {
    expect(readView(params(narrowTo(params("group=client"), null)!)).filters).toEqual([["client", ""]]);
  });
});


it("retains the errors-only filter when narrowing to a provider", () => {
  expect(narrowTo(params("group=provider&errors=true"), "gateway-a")).toBe("?group=model&errors=true&provider=gateway-a");
});

describe("readView", () => {
  it("keeps arbitrary look-back periods through row navigation", () => {
    const view = params("since=129600&group=client&provider=a%2Fb&model=");
    expect(readView(view).period).toBe(129600);
    const next = readView(params(narrowTo(view, "team/editor")!));
    expect(next.period).toBe(129600);
    expect(next.filters).toEqual([["client", "team/editor"], ["provider", "a/b"], ["model", ""]]);
  });

  it("accepts CLI-style durations and defaults only an absent period", () => {
    expect(readView(params("")).period).toBe(86400);
    expect(readView(params("since=36h")).period).toBe(129600);
    for (const value of ["", "nope", "0", "-1", "9007199254740992"]) {
      expect(readView(params(`since=${value}`)).period).toBeNull();
    }
  });
});
