const assert = require("node:assert/strict");
const fs = require("node:fs");
const { chromium } = require("/runner/node_modules/playwright");
const { default: AxeBuilder } = require("/runner/node_modules/@axe-core/playwright");

const evidence = "/evidence";
const origin = "http://localhost:35317";
const result = `${evidence}/recovery-result.json`;
const label = "Recovery subscription";
const diagnostics = "Provider usage diagnostics (returned JSON)";
const checks = [];

async function mode(value) {
  const response = await fetch(`http://localhost:35444/fixture/mode?value=${value}`);
  assert.equal(response.status, 200);
}

async function action(page, button, operation, status = 200) {
  const [response] = await Promise.all([
    page.waitForResponse((r) => r.request().method() === "POST" && r.url().endsWith(operation)),
    button.click(),
  ]);
  assert.equal(response.status(), status);
  return response.json();
}

async function capture(browser) {
  const context = await browser.newContext({
    storageState: `${evidence}/session.json`,
    viewport: { width: 1360, height: 1000 },
    recordVideo: { dir: evidence, size: { width: 1360, height: 1000 } },
  });
  const page = await context.newPage();
  const row = page.getByRole("row").filter({ hasText: label });
  await page.goto(`${origin}/#/accounts`);
  await row.getByText("Disabled", { exact: true }).waitFor();
  await page.waitForTimeout(1000);
  await action(page, row.getByRole("button", { name: "Refresh usage" }), "/usage");
  await row.getByText("5h 18% · 7d 42%").waitFor();
  await page.waitForTimeout(1200);
  await row.getByRole("button", { name: `Details for ${label}` }).click();
  const disclosure = page.getByText(diagnostics, { exact: true });
  await disclosure.waitFor();
  await page.waitForTimeout(1200);
  await disclosure.click();
  await page.waitForTimeout(1600);
  const video = page.video();
  await context.close();
  await video.saveAs(`${evidence}/account-recovery.webm`);
}

async function run(browser) {
  await mode("recovery-ok");
  const context = await browser.newContext({
    storageState: `${evidence}/session.json`,
    viewport: { width: 1440, height: 1100 },
  });
  const page = await context.newPage();
  await page.goto(`${origin}/#/accounts/add`);
  const form = page.locator('[data-slot="card"]').filter({ hasText: "Import a login" });
  await form.getByLabel("Label (optional)").fill(label);
  await form.getByLabel("Credential file").setInputFiles({
    name: "recovery.json",
    mimeType: "application/json",
    buffer: Buffer.from(JSON.stringify({
      tokens: { access_token: "synthetic-original", refresh_token: "synthetic-refresh", account_id: "recovery" },
    })),
  });
  await action(page, form.getByRole("button", { name: "Import", exact: true }), "/import");
  await page.getByRole("link", { name: "Back to accounts" }).click();

  const row = page.getByRole("row").filter({ hasText: label });
  const open = row.getByRole("button", { name: `Details for ${label}` });
  const dialog = page.getByRole("dialog");
  const retry = dialog.getByRole("button", { name: "Retry", exact: true });
  const refresh = dialog.getByRole("button", { name: "Refresh usage", exact: true });
  const disclosure = dialog.getByText(diagnostics, { exact: true });
  const json = dialog.locator("details pre");
  const toggle = (name) => row.getByRole("button", { name, exact: true });
  const badge = (name) => row.getByText(name, { exact: true });

  await row.waitFor();
  if (await badge("Disabled").isVisible())
    await action(page, toggle("Enable"), "/enable");
  await badge("Available").waitFor();
  await open.click();
  await dialog.getByText("Retry clears Xolotl’s auth failure and cooldown. It preserves the account’s enabled state, credential and provider quota.", { exact: true }).waitFor();
  assert.equal((await action(page, retry, "/reset")).catalog_refresh.models, 1);
  await dialog.getByText("Available", { exact: true }).waitFor();
  await page.getByText("Cooldown and auth failure cleared", { exact: true }).waitFor();
  await page.getByText("codex: 1 models found", { exact: true }).waitFor();
  await mode("recovery-model-error");
  const discoveryFailure = await action(page, retry, "/reset");
  assert.equal(discoveryFailure.reset, true);
  assert.match(discoveryFailure.catalog_refresh.error, /503 Service Unavailable/);
  await dialog.getByText(/Discovery failed: HTTP status server error \(503 Service Unavailable\)/).waitFor();
  await mode("recovery-ok");
  await page.keyboard.press("Escape");
  checks.push("Retry explains enabled-state, credential and quota preservation. Healthy account Retry succeeds and reports discovery separately");

  await action(page, toggle("Disable"), "/disable");
  await badge("Disabled").waitFor();
  await action(page, toggle("Refresh usage"), "/usage");
  await row.getByText("5h 18% · 7d 42%").waitFor();
  await row.getByText("2 reset credits available").waitFor();
  await open.click();
  const [snapshot] = await Promise.all([
    page.waitForResponse((r) => r.request().method() === "POST" && r.url().endsWith("/usage")).then((r) => r.json()),
    refresh.click(),
    dialog.getByRole("button", { name: "Refreshing…" }).waitFor(),
  ]);
  await dialog.getByText(/Usage · fetched/).waitFor();
  await dialog.getByRole("progressbar", { name: "5h window used" }).waitFor();
  await dialog.getByRole("progressbar", { name: "7d window used" }).waitFor();
  assert.equal(await json.isVisible(), false);
  await disclosure.focus();
  await page.keyboard.press("Enter");
  await json.waitFor();
  assert.deepEqual(JSON.parse(await json.innerText()), snapshot);
  assert.equal(snapshot.raw.reset_credits, 2);
  assert.deepEqual(snapshot.raw.extra_diagnostics, { buckets: ["fixture", null] });
  await dialog.getByText("Disabled", { exact: true }).waitFor();
  await dialog.evaluate((node) => { node.scrollTop = 0; });
  await page.screenshot({ path: `${evidence}/account-recovery.png` });
  checks.push("Disabled row and detail both refresh usage; keyboard disclosure shows all returned JSON");

  await mode("recovery-usage-error");
  await action(page, refresh, "/usage", 502);
  const usageError = dialog.getByRole("alert").filter({ hasText: "Usage refresh failed" });
  await usageError.getByText(/503.*fixture usage service unavailable/).waitFor();
  await usageError.getByText("The last fetched usage is still shown.").waitFor();
  assert.deepEqual(JSON.parse(await json.innerText()), snapshot);
  await usageError.scrollIntoViewIfNeeded();
  await page.screenshot({ path: `${evidence}/account-recovery-error.png` });
  await mode("recovery-ok");
  await action(page, dialog.getByRole("button", { name: "Retry usage fetch" }), "/usage");
  await usageError.waitFor({ state: "hidden" });
  await dialog.getByText("Disabled", { exact: true }).waitFor();
  checks.push("Upstream error stays beside cached usage; retry returns to the disabled account's diagnostics");

  const reset = await action(page, retry, "/reset");
  assert.equal(reset.reset, true);
  await page.reload();
  await dialog.getByText("Disabled", { exact: true }).waitFor();
  checks.push("Disabled account Retry succeeds and preserves disabled state");
  await page.keyboard.press("Escape");

  await action(page, toggle("Enable"), "/enable");
  await badge("Available").waitFor();
  await mode("recovery-unsaved");
  await action(page, toggle("Refresh usage"), "/usage");
  await badge("Unsaved token").waitFor();
  assert.equal(await badge("Available").isVisible(), false);
  await page.screenshot({ path: `${evidence}/account-recovery-unsaved.png` });
  await open.click();
  await dialog.getByText("Writing the refreshed token to OpenBao failed. Xolotl is retrying.").waitFor();
  checks.push("Failed OpenBao write after a real fixture OAuth refresh shows Unsaved token in the row and details");

  await mode("recovery-ok");
  await page.keyboard.press("Escape");
  await dialog.waitFor({ state: "hidden" });
  // Refresh the account query while Xolotl retries the write in the background.
  for (let elapsed = 0; elapsed < 25 && !(await badge("Available").isVisible()); elapsed++) {
    await page.waitForTimeout(1000);
    await page.reload();
    await row.waitFor();
  }
  await badge("Available").waitFor();
  await action(page, toggle("Disable"), "/disable");
  await badge("Disabled").waitFor();
  checks.push("Successful background Vault retry restores the available badge; the account can be disabled again");

  const accessibility = [];
  for (const scheme of ["light", "dark"]) {
    for (const width of [1440, 390]) {
      await page.emulateMedia({ colorScheme: scheme, reducedMotion: "reduce" });
      await page.setViewportSize({ width, height: 880 });
      await open.click();
      await disclosure.click();
      await json.waitFor();
      const overflow = await dialog.evaluate((node) => node.scrollWidth > node.clientWidth);
      assert.equal(overflow, false, `Diagnostics overflow at ${width}px in ${scheme}`);
      const scan = await new AxeBuilder({ page }).include('[role="dialog"]')
        .withTags(["wcag2a", "wcag2aa", "wcag21aa", "wcag22aa"]).analyze();
      accessibility.push({ scheme, width, violations: scan.violations });
      await page.screenshot({ path: `${evidence}/account-recovery-${width}-${scheme}.png` });
      await page.keyboard.press("Escape");
    }
  }
  assert.deepEqual(accessibility.flatMap((scan) => scan.violations), []);
  checks.push("Usage dialog has zero axe violations and no horizontal overflow in desktop and phone light and dark scans");
  await context.close();
  await capture(browser);
  return accessibility;
}

(async () => {
  fs.writeFileSync(result, JSON.stringify({ status: "failed", reason: "not completed" }));
  const browser = await chromium.launch();
  try {
    const accessibility = await run(browser);
    fs.writeFileSync(result, JSON.stringify({ status: "passed", checks, accessibility }, null, 2));
  } catch (error) {
    fs.writeFileSync(result, JSON.stringify({ status: "failed", checks, reason: String(error) }, null, 2));
    throw error;
  } finally {
    await mode("recovery-ok");
    await browser.close();
  }
})().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
