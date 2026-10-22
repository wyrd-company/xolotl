// ---
// relationships:
//   tests: account-state
// ---
import { renderToStaticMarkup } from "react-dom/server";
import { expect, test } from "vitest";
import { WindowBars, WindowsSummary } from "./account-state";

const windows = [
  { name: "iguana_necktie", used_percent: 100, resets_at: null, window_secs: null },
  { name: "7d-fable", used_percent: 73, resets_at: 4_070_908_800, window_secs: 604800 },
];

test("both account summaries and quota bars hide cached internal windows and show Fable", () => {
  for (const element of [<WindowsSummary windows={windows} />, <WindowBars windows={windows} />]) {
    const html = renderToStaticMarkup(element);
    expect(html).not.toContain("iguana_necktie");
    expect(html).toContain("7d-fable");
    expect(html).toContain("73%");
  }
});
