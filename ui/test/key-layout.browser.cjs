// Regression for the shared-rate-limit row: run against built ui/dist, or set
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
  const report = { passed: false, viewport: 1440, fixture: { limits, usage }, cases: [] };
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
          "/admin/keys": [shared, free],
          "/admin/policies": [{ name: "team" }, { name: "services" }],
          "/admin/usage": [
            { key: "team/editor", requests: 1200 },
            { key: "services/reference", requests: 40 },
          ],
        }[pathname];
        return route.fulfill({ status: data === undefined ? 404 : 200, contentType: "application/json", body: JSON.stringify(data ?? {}) });
      });
      await page.goto(url + "/#/keys");
      const row = page.getByRole("row").filter({ hasText: "team/editor" });
      await row.waitFor();
      await page.evaluate(() => document.fonts.ready);
      await page.locator("[data-slot=table-container]").evaluate((el) => {
        el.scrollLeft = 0;
      });
      const metrics = await page.locator("[data-slot=table-container]").evaluate((el) => {
        const bounds = el.getBoundingClientRect();
        return {
          containerWidth: el.clientWidth,
          tableWidth: el.scrollWidth,
          scrollLeft: el.scrollLeft,
          actions: [...el.querySelectorAll("tbody tr")].map((row) => {
            const button = [...row.querySelectorAll("button")].find((button) => button.textContent.trim() === "Revoke");
            const rect = button.getBoundingClientRect();
            return {
              client: row.querySelector("td").innerText.split("\n")[0],
              left: rect.left,
              right: rect.right,
              clippingRight: bounds.right,
              visibleWithoutScrolling: rect.left >= bounds.left - 1 && rect.right <= bounds.right + 1,
            };
          }),
        };
      });
      const exactText = await row.locator("td").nth(3).textContent();
      const result = { colorScheme, ...metrics, exactValuesPresent: expected.every((value) => exactText.includes(value)), errors };
      report.cases.push(result);
      if (output)
        await page.screenshot({ path: path.join(output, `keys-four-limits-${colorScheme}.png`), fullPage: true, animations: "disabled" });
      assert.equal(metrics.scrollLeft, 0);
      assert.ok(metrics.tableWidth <= metrics.containerWidth + 1, `Table needs horizontal scrolling: ${JSON.stringify(result)}`);
      assert.equal(metrics.actions.length, 2);
      assert.ok(
        metrics.actions.every((action) => action.visibleWithoutScrolling),
        `Revoke clipped: ${JSON.stringify(result)}`,
      );
      assert.ok(result.exactValuesPresent, `Exact usage/limit values missing: ${exactText}`);
      assert.deepEqual(errors, []);
      await page.close();
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
