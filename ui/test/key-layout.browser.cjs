// Layout regression for long key identifiers and shared rate limits. Run against built ui/dist, or set
// KEY_LAYOUT_URL to a standard-container URL. Requires playwright@1.58.2.
// CI runs this in mcr.microsoft.com/playwright:v1.58.2-noble after pnpm build.
const assert = require("node:assert/strict");
const fs = require("node:fs/promises");
const path = require("node:path");
const { chromium } = require("playwright");
const { serveBuild } = require("./serve-build.cjs");

const limits = { requests_per_minute: 600, requests_per_day: 10000, tokens_per_minute: 200000, tokens_per_day: 10000000, per: "policy" };
const usage = { requests_per_minute: 42, requests_per_day: 3456, tokens_per_minute: 123456, tokens_per_day: 2345678 };
const expected = ["42/600 req/min", "3,456/10,000 req/day", "123,456/200,000 tok/min", "2,345,678/10,000,000 tok/day"];
const shared = {
  client_id: "team/editor",
  policy: "team",
  name: "editor",
  description: "Editor workstation",
  vault_path: "keys/team/editor",
  created_at: "2026-11-20T10:00:00Z",
  rotated_at: "2026-11-24T07:00:00Z",
  expires_at: "2999-11-26T18:00:00Z",
  previous_expires_at: "2999-11-24T08:00:00Z",
  disabled: false,
  usage,
  limit_usage: usage,
  limits,
};
const free = {
  ...shared,
  client_id: "services/reference",
  policy: "services",
  name: "reference",
  vault_path: "keys/services/reference",
  description: "Background jobs",
  rotated_at: null,
  expires_at: null,
  previous_expires_at: null,
  limits: undefined,
  limit_usage: undefined,
};
const longKeys = [
  { name: "acceptance-hub", description: "Hub service", disabled: false },
  { name: "acceptance-hub-recovery-worker", description: "Recovery worker", disabled: true },
].map((key) => ({
  ...free,
  ...key,
  policy: "management-only",
  client_id: `management-only/${key.name}`,
  vault_path: `keys/management-only/${key.name}`,
}));
const keys = [shared, free, ...longKeys];
const status = {
  version: "fixture",
  vault: { ok: true },
  database: { ok: true },
  unloaded_policies: [],
  recent_errors: [],
  vault_mount: "secret",
  vault_prefix: "xolotl",
  subscription: { strategy: "least-used" },
};

async function main() {
  const output = process.env.KEY_LAYOUT_EVIDENCE_DIR;
  if (output) await fs.mkdir(output, { recursive: true });
  const report = {
    passed: false,
    viewport: 1440,
    fixture: { limits, usage, keys: keys.map(({ client_id, vault_path }) => ({ client_id, vault_path })) },
    cases: [],
  };
  let server, browser;
  try {
    let url = process.env.KEY_LAYOUT_URL;
    if (!url) ({ server, url } = await serveBuild());
    browser = await chromium.launch({ args: ["--no-sandbox"] });
    for (const colorScheme of ["light", "dark"]) {
      const page = await browser.newPage({ viewport: { width: 1440, height: 1100 }, locale: "en-US", colorScheme });
      const errors = [];
      page.on("pageerror", (error) => errors.push(error.message));
      await page.route("**/admin/**", (route) => {
        const pathname = new URL(route.request().url()).pathname;
        const data = {
          "/admin/session": { authenticated: true, configured: true, user: "Fixture operator", csrf_token: "synthetic-only" },
          "/admin/status": status,
          "/admin/keys": keys,
          "/admin/policies": [{ name: "team" }, { name: "services" }, { name: "management-only" }],
          "/admin/usage": [
            { key: "team/editor", requests: 1200 },
            { key: "services/reference", requests: 40 },
          ],
        }[pathname];
        return route.fulfill({ status: data === undefined ? 404 : 200, contentType: "application/json", body: JSON.stringify(data ?? {}) });
      });
      await page.goto(url + "/#/keys");
      const table = page.getByRole("table");
      await table.getByText(longKeys.at(-1).client_id, { exact: true }).waitFor();
      await page.evaluate(() => document.fonts.ready);
      const metrics = await page.locator("[data-slot=table-container]").evaluate((el) => {
        const bounds = el.getBoundingClientRect();
        const table = el.querySelector("table");
        const leftEdge = Math.max(bounds.left, 0);
        const rightEdge = Math.min(bounds.right, document.documentElement.clientWidth);
        const fits = ({ left, right }) => left >= leftEdge - 1 && right <= rightEdge + 1;
        const clippedCells = [...table.querySelectorAll("th, td")]
          .filter((cell) => !fits(cell.getBoundingClientRect()) || cell.scrollWidth > cell.clientWidth + 1)
          .map((cell) => ({ row: cell.parentElement.rowIndex, column: cell.cellIndex, text: cell.textContent.trim() }));
        return {
          containerWidth: el.clientWidth,
          tableWidth: el.scrollWidth,
          scrollLeft: el.scrollLeft,
          clippedCells,
          rows: [...table.tBodies[0].rows].map((row) => ({
            client: row.cells[0].querySelector("span").textContent,
            vaultPath: row.cells[2].textContent,
            columnCount: row.cells.length,
            actions: [...row.querySelectorAll("button")].map((button) => {
              const rect = button.getBoundingClientRect();
              return {
                label: button.textContent.trim(),
                left: rect.left,
                right: rect.right,
                visibleWithoutScrolling: fits(rect) && button.checkVisibility({ visibilityProperty: true, opacityProperty: true }),
              };
            }),
          })),
        };
      });
      const sharedRow = table.getByRole("row").filter({ hasText: shared.client_id });
      const limitText = await sharedRow.getByRole("cell").nth(3).textContent();
      const result = { colorScheme, ...metrics, missingLimits: expected.filter((value) => !limitText.includes(value)), errors };
      report.cases.push(result);
      if (output)
        await page.screenshot({ path: path.join(output, `keys-layout-${colorScheme}.png`), fullPage: true, animations: "disabled" });
      await page.close();
    }
    for (const result of report.cases) {
      const { colorScheme, scrollLeft, tableWidth, containerWidth, clippedCells, rows, missingLimits, errors } = result;
      assert.equal(scrollLeft, 0);
      assert.ok(tableWidth <= containerWidth + 1, `${colorScheme}: table needs horizontal scrolling (${tableWidth}px in ${containerWidth}px)`);
      assert.deepEqual(clippedCells, [], `${colorScheme}: clipped table cells`);
      assert.deepEqual(
        rows.map(({ client, vaultPath, columnCount }) => ({ client, vaultPath, columnCount })),
        keys.map((key) => ({ client: key.client_id, vaultPath: key.vault_path, columnCount: 6 })),
      );
      for (const [index, row] of rows.entries()) {
        assert.deepEqual(row.actions.map(({ label }) => label), ["Rotate…", keys[index].disabled ? "Enable" : "Disable", "Revoke"]);
        assert.ok(row.actions.every((action) => action.visibleWithoutScrolling), `${colorScheme}: actions clipped for ${row.client}`);
      }
      assert.deepEqual(missingLimits, [], `${colorScheme}: exact usage/limit values missing`);
      assert.deepEqual(errors, []);
    }
    report.passed = true;
  } catch (error) {
    report.error = error.message;
    throw error;
  } finally {
    if (output) await fs.writeFile(path.join(output, "key-layout-report.json"), JSON.stringify(report, null, 2));
    console.log(JSON.stringify(report, null, 2));
    if (browser) await browser.close();
    if (server) await new Promise((resolve) => server.close(resolve));
  }
}
main().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
