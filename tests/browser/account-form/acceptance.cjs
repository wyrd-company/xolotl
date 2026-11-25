const { chromium } = require("/runner/node_modules/playwright");
const fs = require("node:fs");
const out = "/evidence",
  origin = "http://localhost:35317";
const checks = [];
(async () => {
  fs.writeFileSync(
    `${out}/acceptance-result.json`,
    JSON.stringify({ status: "failed", reason: "not completed" }),
  );
  const browser = await chromium.launch({ headless: true });
  const context = await browser.newContext({
    storageState: `${out}/session.json`,
    viewport: { width: 1440, height: 1100 },
  });
  const page = await context.newPage();
  const imports = [],
    priorities = [];
  page.on("request", (r) => {
    if (r.method() === "POST" && r.url().endsWith("/admin/accounts/import"))
      imports.push(r.postDataJSON());
    if (r.method() === "POST" && r.url().endsWith("/priority"))
      priorities.push(r.postDataJSON());
  });
  await page.goto(`${origin}/#/accounts/add`);
  const section = page
    .locator('[data-slot="card"]')
    .filter({ has: page.getByText("Import a login", { exact: true }) });
  const label = page.getByLabel("Label (optional)", { exact: true });
  const file = page.getByLabel("Credential file", { exact: true });
  const submit = section.getByRole("button", { name: "Import", exact: true });
  await label.fill("Work subscription");
  await file.setInputFiles({
    name: "invalid.json",
    mimeType: "application/json",
    buffer: Buffer.from("not json"),
  });
  await submit.click();
  await section
    .getByRole("alert")
    .filter({ hasText: "That file is not JSON." })
    .waitFor();
  if ((await label.inputValue()) !== "Work subscription" || imports.length)
    throw Error("Invalid JSON did not preserve label or sent request");
  await page.screenshot({ path: `${out}/import-invalid.png`, fullPage: true });
  checks.push(
    "Invalid JSON rejected locally; label and file retained; no import request",
  );
  await file.setInputFiles({
    name: "missing-tokens.json",
    mimeType: "application/json",
    buffer: Buffer.from("{}"),
  });
  await submit.click();
  await section
    .getByRole("alert")
    .filter({ hasText: "tokens.access_token" })
    .waitFor();
  if ((await label.inputValue()) !== "Work subscription")
    throw Error("API failure lost label");
  checks.push("Invalid credentials API error shown inline; label retained");
  const credentials = (id) =>
    Buffer.from(
      JSON.stringify({
        tokens: { access_token: `synthetic-${id}`, account_id: id },
      }),
    );
  await label.fill("  Work subscription  ");
  await file.setInputFiles({
    name: "auth.json",
    mimeType: "application/json",
    buffer: credentials("work"),
  });
  const saved = page.waitForResponse(
    (r) =>
      r.request().method() === "POST" &&
      r.url().endsWith("/admin/accounts/import") &&
      r.status() === 200,
  );
  await submit.click();
  await saved;
  await page.waitForFunction(
    () => document.querySelector("#import-label").value === "",
  );
  if (imports.at(-1).label !== "Work subscription")
    throw Error("Label was not transported exactly");
  if (await file.inputValue())
    throw Error("Successful import did not reset file");
  await label.fill("  ");
  await file.setInputFiles({
    name: "auth-personal.json",
    mimeType: "application/json",
    buffer: credentials("personal"),
  });
  const saved2 = page.waitForResponse(
    (r) =>
      r.request().method() === "POST" &&
      r.url().endsWith("/admin/accounts/import") &&
      r.status() === 200,
  );
  await submit.click();
  await saved2;
  if (imports.at(-1).label !== null) throw Error("Blank label was not omitted");
  checks.push(
    "Successful imports persisted trimmed label and blank null label; file and label reset",
  );
  await page.getByRole("link", { name: "Back to accounts" }).click();
  await page
    .getByRole("button", { name: "Details for Work subscription" })
    .click();
  const dialog = page.getByRole("dialog"),
    priority = dialog.getByLabel("Priority (higher is used first)");
  const save = dialog.getByRole("button", {
    name: "Save priority",
    exact: true,
  });
  // Repeated runs may begin at the first value; establish a different value.
  if ((await priority.inputValue()) === "9223372036854775807") {
    await priority.fill("0");
    await Promise.all([
      page.waitForResponse(
        (r) =>
          r.request().method() === "POST" &&
          r.url().endsWith("/priority") &&
          r.status() === 200,
      ),
      save.click(),
    ]);
  }
  for (const value of [
    "9223372036854775807",
    "-9223372036854775808",
    "9007199254740993",
    "-9007199254740993",
  ]) {
    await priority.fill(value);
    const saved = page.waitForResponse(
      (r) =>
        r.request().method() === "POST" &&
        r.url().endsWith("/priority") &&
        r.status() === 200,
    );
    await save.click();
    const response = await saved;
    const body = await response.json();
    if (
      body.priority_decimal !== value ||
      priorities.at(-1).priority !== value ||
      typeof priorities.at(-1).priority !== "string"
    )
      throw Error("Priority precision or string transport lost");
    await page.reload();
    await priority.waitFor();
    if ((await priority.inputValue()) !== value)
      throw Error(`Priority did not survive reload: ${value}`);
    await page.keyboard.press("Escape");
    await page
      .getByRole("row")
      .filter({ hasText: "Work subscription" })
      .getByRole("cell", { name: value, exact: true })
      .waitFor();
    await page
      .getByRole("button", { name: "Details for Work subscription" })
      .click();
  }
  checks.push(
    "Signed int64 extrema and ±9007199254740993 saved as strings and reloaded exactly in table and dialog",
  );
  const before = priorities.length;
  for (const value of [
    "9223372036854775808",
    "-9223372036854775809",
    "1.5",
    "1e3",
    "",
  ]) {
    await priority.fill(value);
    await save.click();
    await dialog
      .getByRole("alert")
      .filter({ hasText: "Enter a whole number" })
      .waitFor();
    if (
      (await priority.inputValue()) !== value ||
      (await priority.getAttribute("aria-invalid")) !== "true"
    )
      throw Error("Validation lost value or error association");
  }
  if (priorities.length !== before) throw Error("Invalid priority reached API");
  checks.push(
    "Overflow, underflow, fractions, exponent and blank input rejected; no mutation; entered text retained",
  );
  await priority.fill("9223372036854775808");
  await priority.blur();
  await page.screenshot({
    path: `${out}/priority-invalid.png`,
    fullPage: true,
  });
  await priority.fill("17");
  await page.route("**/admin/accounts/*/priority", (r) =>
    r.fulfill({
      status: 503,
      contentType: "application/json",
      body: JSON.stringify({ error: { message: "Fixture store unavailable" } }),
    }),
  );
  await save.click();
  await dialog
    .getByRole("alert")
    .filter({ hasText: "Fixture store unavailable" })
    .waitFor();
  if ((await priority.inputValue()) !== "17")
    throw Error("Server failure lost entered priority");
  await page.unroute("**/admin/accounts/*/priority");
  const retry = page.waitForResponse(
    (r) =>
      r.request().method() === "POST" &&
      r.url().endsWith("/priority") &&
      r.status() === 200,
  );
  await save.click();
  await retry;
  await page.reload();
  await priority.waitFor();
  if ((await priority.inputValue()) !== "17") throw Error("Save retry failed");
  checks.push(
    "Failed priority save shows underlying error and preserves value; retry succeeds",
  );
  await priority.fill("9223372036854775807");
  const finalSave = page.waitForResponse(
    (r) =>
      r.request().method() === "POST" &&
      r.url().endsWith("/priority") &&
      r.status() === 200,
  );
  await save.click();
  await finalSave;
  await page.reload();
  await priority.waitFor();
  await page.screenshot({ path: `${out}/priority-saved.png`, fullPage: true });
  await page.keyboard.press("Escape");
  await page.screenshot({ path: `${out}/accounts-saved.png`, fullPage: true });
  await page.getByRole("link", { name: "Add account", exact: true }).click();
  await label.fill("Work subscription");
  await page.screenshot({ path: `${out}/import-label.png`, fullPage: true });
  for (const scheme of ["light", "dark"]) {
    await page.setViewportSize({ width: 390, height: 844 });
    await page.emulateMedia({ colorScheme: scheme });
    await page.screenshot({
      path: `${out}/import-phone-${scheme}.png`,
      fullPage: true,
    });
    if (
      await page.evaluate(
        () => document.documentElement.scrollWidth > window.innerWidth,
      )
    )
      throw Error("Add account page overflows phone");
    await page.getByRole("link", { name: "Back to accounts" }).click();
    await page
      .getByRole("button", { name: "Details for Work subscription" })
      .click();
    await priority.fill("9223372036854775808");
    await priority.press("Tab");
    await dialog
      .getByRole("alert")
      .filter({ hasText: "Enter a whole number" })
      .waitFor();
    await page.waitForTimeout(500);
    const box = await dialog.boundingBox();
    if (box.x < 0 || box.x + box.width > 390)
      throw Error("Priority dialog exceeds phone viewport");
    await page.screenshot({
      path: `${out}/priority-phone-${scheme}.png`,
      fullPage: true,
    });
    await page.keyboard.press("Escape");
    await page.getByRole("link", { name: "Add account", exact: true }).click();
    await label.fill("Work subscription");
  }
  checks.push(
    "Add account form and priority validation checked at desktop and phone widths in light and dark; keyboard blur validates priority",
  );
  await browser.close();
  fs.writeFileSync(
    `${out}/acceptance-result.json`,
    JSON.stringify({ status: "passed", checks }, null, 2),
  );
  console.log(JSON.stringify({ status: "passed", checks }, null, 2));
})().catch((e) => {
  fs.writeFileSync(
    `${out}/acceptance-result.json`,
    JSON.stringify({ status: "failed", checks, error: String(e) }, null, 2),
  );
  console.error(e);
  process.exit(1);
});
