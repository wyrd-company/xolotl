const assert = require("node:assert/strict");
const { writeFileSync, renameSync } = require("node:fs");
const { join } = require("node:path");
const { chromium } = require("playwright");
const { AxeBuilder } = require("@axe-core/playwright");

const out = process.env.XOLOTL_EVIDENCE_DIR ?? "/evidence";
const origin = "http://localhost:36317";
const report = { status: "failed", head: process.env.XOLOTL_TEST_HEAD, checks: [] };
const reportPath = join(out, "acceptance.json");
function writeReport() {
  writeFileSync(reportPath, JSON.stringify(report, null, 2) + "\n");
}

async function checkDialog(page) {
  const modal = page.getByRole("dialog");
  await modal.evaluate(node => Promise.all(node.getAnimations({ subtree: true }).map(animation => animation.finished)));
  const { violations } = await new AxeBuilder({ page }).include('[data-slot="dialog-content"]').analyze();
  assert.deepEqual(violations.map(({ id, nodes }) => ({ id, selectors: nodes.map(node => node.target) })), []);
  const box = await modal.boundingBox();
  assert.ok(box.x >= 0 && box.x + box.width <= page.viewportSize().width, "Dialog exceeds viewport width");
  assert.equal(await modal.evaluate(el => el.scrollWidth <= el.clientWidth), true, "Dialog content has horizontal overflow");
}

async function main() {
  writeReport();
  const browser = await chromium.launch();
  try {
    const signin = await browser.newContext();
    const loginPage = await signin.newPage();
    await loginPage.goto(origin);
    await loginPage.getByRole("link", { name: /sign in/i }).click();
    await loginPage.getByRole("heading", { name: "Overview", exact: true }).waitFor();
    const storageState = await signin.storageState();
    await signin.close();

    const context = await browser.newContext({
      storageState,
      viewport: { width: 1280, height: 800 },
      colorScheme: "light",
      recordVideo: { dir: out, size: { width: 960, height: 600 } },
    });
    const page = await context.newPage();
    page.setDefaultTimeout(8000);
    const pageErrors = [];
    const writes = [];
    page.on("pageerror", error => pageErrors.push(error.message));
    page.on("request", request => {
      if (request.method() === "POST" && /\/admin\/providers\/[^/]+\/key$/.test(request.url())) {
        writes.push(new URL(request.url()).pathname);
      }
    });
    await page.goto(origin + "/#/accounts?kind=providers");
    const byButton = (name) => page.getByRole("button", { name, exact: true });
    const dialog = page.getByRole("dialog");
    const keyInput = dialog.getByLabel("API key", { exact: true });
    const save = dialog.getByRole("button", { name: "Save key", exact: true });
    const submit = async (provider, action, key) => {
      await byButton(`${action} for ${provider}`).click();
      assert.equal(await keyInput.getAttribute("type"), "password");
      assert.equal(await keyInput.inputValue(), "");
      assert.equal(await save.isDisabled(), true);
      await keyInput.fill(key);
      const response = page.waitForResponse(r =>
        r.request().method() === "POST" && new URL(r.url()).pathname === `/admin/providers/${provider}/key`,
      );
      await save.click();
      return { response };
    };
    const capture = (name) => page.screenshot({ path: join(out, name + ".png"), animations: "disabled" });
    const holdFrame = () => page.waitForTimeout(850); // Readable pauses in the documentation recording.
    const saved = (provider) => dialog.getByRole("status").filter({ hasText: `Key saved for ${provider}` });
    const done = () => dialog.getByRole("button", { name: "Done", exact: true }).click();

    const original = await submit("gateway", "Set key", "  fixture-original  ");
    await dialog.getByRole("status").filter({ hasText: "Saving key and checking model discovery" }).waitFor();
    assert.equal(await save.isDisabled(), true);
    assert.equal(await keyInput.isDisabled(), true);
    await page.keyboard.press("Enter");
    const originalResponse = await original.response;
    assert.equal(originalResponse.status(), 200);
    assert.deepEqual((await originalResponse.json()).catalog_refresh, { provider: "gateway", models: 1 });
    await saved("gateway").filter({ hasText: "1 model found." }).waitFor();
    assert.equal(await keyInput.count(), 0);
    await capture("provider-key-success");
    await holdFrame();
    await done();
    await page.locator('button[aria-label="Replace key for gateway"]:focus').waitFor();
    report.checks.push("Set key reports in-progress state, prevents duplicate submission, confirms the OpenBao write and shows the discovered model count. Done returns keyboard focus to the row.");

    const replacement = await submit("gateway", "Replace key", "fixture-replacement");
    const replacementResponse = await replacement.response;
    assert.equal(replacementResponse.status(), 200);
    const outcome = await replacementResponse.json();
    assert.equal(outcome.stored, true);
    assert.match(outcome.catalog_refresh.error, /503 Service Unavailable/);
    await saved("gateway").waitFor();
    const warning = dialog.getByRole("alert").filter({ hasText: "Model discovery failed" });
    await warning.filter({ hasText: outcome.catalog_refresh.error }).waitFor();
    await warning.filter({ hasText: "cached or static models remain available" }).waitFor();
    const discovery = dialog.getByRole("region", { name: "Provider discovery" });
    await discovery.getByText(/1 models.*cached or static models shown/).waitFor();
    await checkDialog(page);
    report.checks.push("The discovery warning dialog passes axe and fits at 1280px in light mode.");
    await capture("provider-key-discovery-error");
    await holdFrame();
    const refreshed = page.waitForResponse(r =>
      r.request().method() === "POST" && new URL(r.url()).pathname === "/admin/catalog/refresh",
    );
    await dialog.getByRole("button", { name: "Refresh models", exact: true }).click();
    const refresh = await refreshed;
    assert.equal(new URL(refresh.url()).searchParams.get("providers"), "gateway");
    assert.equal(refresh.status(), 200);
    assert.deepEqual((await refresh.json()).gateway.sort(), ["another-model", "replacement-model"]);
    await discovery.getByText(/2 models/).waitFor();
    assert.doesNotMatch(await discovery.innerText(), /503|Discovery failed/);
    await saved("gateway").waitFor();
    await holdFrame();
    await done();
    report.checks.push("Replace key reports the saved credential and the upstream 503 together, preserves the cached model, and recovers discovery with one provider-scoped refresh using the saved key.");

    const federated = await submit("federated", "Set key", "fixture-rejected");
    const rejection = await federated.response;
    assert.equal(rejection.status(), 400);
    const reason = (await rejection.json()).error.message;
    assert.match(reason, /uses workload identity/);
    await dialog.getByRole("alert").filter({ hasText: reason }).waitFor();
    await dialog.getByText("Update the provider's workload identity configuration instead.").waitFor();
    assert.equal(await saved("federated").count(), 0);
    await capture("provider-key-rejected");
    await holdFrame();
    await page.keyboard.press("Escape");
    await byButton("Set key for federated").click();
    assert.equal(await keyInput.inputValue(), "");
    assert.equal(await dialog.getByRole("alert").count(), 0);
    await page.keyboard.press("Escape");
    report.checks.push("Workload identity offers Set key and shows the API's rejection with configuration guidance. Reopening clears the previous key and error.");

    const disabled = await submit("disabled", "Set key", "fixture-disabled");
    const disabledResponse = await disabled.response;
    assert.equal(disabledResponse.status(), 200);
    assert.equal((await disabledResponse.json()).catalog_refresh, null);
    await saved("disabled").filter({ hasText: "No model discovery result" }).waitFor();
    await done();
    await page.getByRole("row").filter({ has: byButton("Replace key for disabled") }).getByText("disabled", { exact: true }).last().waitFor();
    report.checks.push("Disabled provider accepts the key, remains disabled and reports no discovery result.");

    const noAuth = await submit("local", "Set key", "fixture-no-auth");
    const noAuthResponse = await noAuth.response;
    assert.equal(noAuthResponse.status(), 200);
    assert.deepEqual((await noAuthResponse.json()).catalog_refresh, { provider: "local", models: 0 });
    await saved("local").filter({ hasText: "0 models found." }).waitFor();
    await done();
    await page.getByRole("row").filter({ has: byButton("Set key for local") }).getByText("none needed").waitFor();
    report.checks.push("No-auth provider accepts a key and explicitly reports zero discovered models. The upstream received no Authorization header.");

    assert.deepEqual(writes, ["gateway", "gateway", "federated", "disabled", "local"].map(p => `/admin/providers/${p}/key`));
    assert.deepEqual(pageErrors, []);
    const recording = page.video();
    await context.close();
    renameSync(await recording.path(), join(out, "provider-key-workflow.webm"));

    for (const width of [390, 1440]) {
      for (const theme of ["light", "dark"]) {
        const a11yContext = await browser.newContext({ storageState, viewport: { width, height: 844 }, colorScheme: theme });
        const a11y = await a11yContext.newPage();
        await a11y.goto(origin + "/#/accounts?kind=providers");
        await a11y.getByRole("button", { name: "Set key for federated", exact: true }).click();
        const modal = a11y.getByRole("dialog");
        await modal.getByLabel("API key").fill("fixture-rejected");
        await modal.getByRole("button", { name: "Save key", exact: true }).click();
        await modal.getByRole("alert").waitFor();
        await checkDialog(a11y);
        await a11y.screenshot({ path: join(out, `provider-key-${width}-${theme}.png`), animations: "disabled" });
        await a11yContext.close();
        report.checks.push(`Provider key rejection dialog passes axe and fits at ${width}px in ${theme} mode.`);
      }
    }
  } finally {
    await browser.close();
  }
  report.status = "passed";
  writeReport();
}

main().catch(error => {
  report.failure = error.message;
  writeReport();
  console.error(error);
  process.exitCode = 1;
});
