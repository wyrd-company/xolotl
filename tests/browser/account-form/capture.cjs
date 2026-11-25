// Generate the user-doc captures against the disposable, signed-in fixture.
const { chromium } = require("/runner/node_modules/playwright");
const fs = require("node:fs");
(async () => {
  const browser = await chromium.launch({ headless: true });
  const context = await browser.newContext({
    storageState: "/evidence/session.json",
    viewport: { width: 1100, height: 900 },
    recordVideo: {
      dir: "/evidence/videos",
      size: { width: 1100, height: 900 },
    },
  });
  const page = await context.newPage();
  await page.goto("http://localhost:35317/#/accounts/add");
  const label = page.getByLabel("Label (optional)", { exact: true });
  await label.fill("Work subscription");
  const card = page.locator('[data-slot="card"]').filter({ has: label });
  await card.scrollIntoViewIfNeeded();
  await page
    .getByLabel("Credential file", { exact: true })
    .setInputFiles({
      name: "auth.json",
      mimeType: "application/json",
      buffer: Buffer.from(
        JSON.stringify({
          tokens: { access_token: "synthetic-work", account_id: "work" },
        }),
      ),
    });
  await page.waitForTimeout(700);
  await card.screenshot({ path: "/evidence/account-import.png" });
  const saved = page.waitForResponse(
    (r) =>
      r.request().method() === "POST" &&
      r.url().endsWith("/admin/accounts/import") &&
      r.status() === 200,
  );
  await card.getByRole("button", { name: "Import", exact: true }).click();
  await saved;
  await page.getByRole("link", { name: "Back to accounts" }).click();
  await page
    .getByRole("button", { name: "Details for Work subscription" })
    .click();
  const dialog = page.getByRole("dialog"),
    priority = dialog.getByLabel("Priority (higher is used first)");
  await priority.fill("9223372036854775807");
  await priority.scrollIntoViewIfNeeded();
  // Existing fixture runs may already have this value; save another first.
  if (
    await dialog.getByRole("button", { name: "Save priority" }).isDisabled()
  ) {
    await priority.fill("0");
    const reset = page.waitForResponse(
      (r) =>
        r.url().endsWith("/priority") &&
        r.request().method() === "POST" &&
        r.status() === 200,
    );
    await dialog.getByRole("button", { name: "Save priority" }).click();
    await reset;
    await priority.fill("9223372036854775807");
  }
  const prioritySaved = page.waitForResponse(
    (r) =>
      r.request().method() === "POST" &&
      r.url().endsWith("/priority") &&
      r.status() === 200,
  );
  await dialog.getByRole("button", { name: "Save priority" }).click();
  await prioritySaved;
  await page.waitForTimeout(700);
  await dialog.screenshot({ path: "/evidence/account-priority.png" });
  await priority.fill("9223372036854775808");
  await priority.blur();
  await page.waitForTimeout(900);
  await dialog.screenshot({ path: "/evidence/account-priority-error.png" });
  await context.close();
  const video = await page.video().path();
  fs.copyFileSync(video, "/evidence/account-forms.webm");
  await browser.close();
})().catch((e) => {
  console.error(e);
  process.exit(1);
});
