// ---
// relationships:
//   tests: layout
// ---
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { renderToStaticMarkup } from "react-dom/server";
import { MemoryRouter, Route, Routes } from "react-router";
import { afterEach, expect, test, vi } from "vitest";
import { Layout } from "./layout";

// The app is client-rendered; its theme store has no server snapshot.
vi.mock("@/lib/theme", () => ({ useTheme: () => "system", setTheme: vi.fn() }));

afterEach(() => vi.unstubAllGlobals());

test.each(["/", "/policies/sample", "/usage"])("repository help stays in shared navigation at %s", (path) => {
  vi.stubGlobal("localStorage", { getItem: () => null });
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const html = renderToStaticMarkup(
    <QueryClientProvider client={client}>
      <MemoryRouter initialEntries={[path]}>
        <Routes>
          <Route element={<Layout />}>
            <Route path="*" element={<h1>Sample screen</h1>} />
          </Route>
        </Routes>
      </MemoryRouter>
    </QueryClientProvider>,
  );
  const nav = html.slice(html.indexOf("<nav"), html.indexOf("</nav>"));
  const links = nav.match(/<a\b[^>]*>Repository \/ report issues[\s\S]*?<\/a>/g);
  expect(links).toHaveLength(1);
  const link = links![0];
  expect(link).toContain('href="https://github.com/wyrd-company/xolotl"');
  expect(link).toContain('target="_blank"');
  expect(link).toContain('rel="noopener noreferrer"');
  expect(link).toContain("opens in a new tab");
  // A native anchor stays in the tab order; its label stays visible at all widths.
  expect(link).not.toMatch(/tabindex=|aria-hidden=|\bhidden\b/);
  expect(link).toContain("focus-visible:outline-2");
  expect(link).toContain("focus-visible:outline-white");
  // Keep help above the growing spacer so long forms do not push it below the fold.
  expect(nav.indexOf(link)).toBeLessThan(nav.indexOf("hidden min-h-6 grow"));
  expect(html).toContain("Sample screen");
});
