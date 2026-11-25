// CI regression: exercise the built React app with synthetic admin responses.
const assert = require("node:assert/strict");
const { chromium } = require("playwright");
const { serveBuild } = require("./serve-build.cjs");

async function main() {
  const { server, url } = await serveBuild();
  let browser;
  try {
    browser = await chromium.launch();
    const page = await browser.newPage();
    page.setDefaultTimeout(5000);
    const starts = [], polls = new Map(), errors = [];
    let importResult;
    page.on("pageerror", error => errors.push(error.message));
    await page.route("**/admin/**", route => {
      const request = route.request(), pathname = new URL(request.url()).pathname;
      const reply = (data, status = 200) => route.fulfill({ status, json: data });
      if (pathname.endsWith("/start")) {
        const provider = pathname.split("/")[3];
        const flow_id = `${provider}-${starts.length + 1}`;
        starts.push({ provider, flow_id, body: request.postDataJSON() });
        return reply({ flow_id, provider, needs_code: false, url: "https://example.test/device", user_code: flow_id, instructions: "Authorize" });
      }
      if (pathname.startsWith("/admin/auth/flows/")) {
        const id = pathname.split("/").at(-1), count = (polls.get(id) ?? 0) + 1;
        polls.set(id, count);
        // The first Codex flow fails after pending; Grok fails on its first poll.
        if ((id === "codex-1" && count > 1) || id === "grok-3")
          return reply({ error: { message: "Fixture status unavailable" } }, 503);
        return reply({ status: "pending" });
      }
      if (pathname === "/admin/accounts/import") return reply(importResult);
      const data = {
        "/admin/session": { authenticated: true, configured: true, user: "Fixture operator", csrf_token: "synthetic" },
        "/admin/account-kinds": ["codex", "grok"].map(kind => ({ kind, label: kind, description: "Subscription", placement: "pool", provider: kind, add: kind === "codex" ? ["device-login", "import"] : ["device-login"], default_base_url: null })),
        "/admin/accounts": [], "/admin/providers": [], "/admin/catalog/status": {},
      }[pathname];
      return reply(data ?? {});
    });
    await page.goto(url + "/#/accounts/add");
    const card = title => page.locator('[data-slot="card"]').filter({ has: page.locator('[data-slot="card-title"]').filter({ hasText: title }) });
    const codex = card("Codex (ChatGPT)"), grok = card("Grok Build (xAI)");
    await codex.getByRole("button", { name: "Start device login", exact: true }).click();
    await codex.getByRole("status").filter({ hasText: "Waiting for authorization" }).waitFor();
    await codex.getByRole("alert").filter({ hasText: "Fixture status unavailable" }).waitFor({ timeout: 10000 });
    assert.match(await codex.innerText(), /A successful login has not been confirmed/);
    assert.doesNotMatch(await codex.innerText(), /Waiting for authorization/);
    const afterError = polls.get("codex-1");
    assert.equal(afterError, 2, "A failed status poll was automatically retried");
    await page.waitForTimeout(6500);
    assert.equal(polls.get("codex-1"), afterError, "Polling continued after HTTP error");
    await codex.getByRole("button", { name: "Start new login", exact: true }).click();
    await codex.getByText("codex-2", { exact: true }).waitFor().catch(() => { throw Error("Start new login did not start a fresh device flow"); });
    await codex.getByRole("status").filter({ hasText: "Waiting for authorization" }).waitFor();
    assert.deepEqual(starts.map(s => [s.provider, s.body]), [["codex", { device: true }], ["codex", { device: true }]]);
    assert.equal(await codex.getByRole("alert").count(), 0);
    await codex.getByRole("button", { name: "Cancel", exact: true }).click();
    await codex.getByRole("button", { name: "Start device login", exact: true }).waitFor();
    assert.equal(starts.length, 2, "Cancel started a login");

    await grok.getByRole("button", { name: "Start device login", exact: true }).click();
    await grok.getByRole("alert").filter({ hasText: "Fixture status unavailable" }).waitFor();
    await grok.getByRole("button", { name: "Start new login", exact: true }).click();
    await grok.getByText("grok-4", { exact: true }).waitFor();
    assert.deepEqual(starts.slice(2).map(s => [s.provider, s.body]), [["grok", { device: true }], ["grok", { device: true }]]);
    await grok.getByRole("status").filter({ hasText: "Waiting for authorization" }).waitFor();
    await grok.getByRole("button", { name: "Cancel", exact: true }).click();

    const imports = card("Import a login");
    const submit = async result => {
      importResult = result;
      await page.getByLabel("Credential file", { exact: true }).setInputFiles({ name: "auth.json", mimeType: "application/json", buffer: Buffer.from("{}") });
      await imports.getByRole("button", { name: "Import", exact: true }).click();
      await imports.getByRole("status").filter({ hasText: `Account ID: ${result.id}` }).waitFor().catch(() => { throw Error("Import did not retain its saved outcome"); });
      assert.match(await imports.innerText(), new RegExp(`provider: ${result.provider}`));
      assert.match(await imports.innerText(), /The imported credentials are saved/);
      assert.equal(await page.getByLabel("Credential file", { exact: true }).inputValue(), "");
    };
    for (const [label, email, identity] of [["Work subscription", "work@example.test", "Work subscription"], [null, "work@example.test", "work@example.test"], [null, null, "codex-work"]]) {
      await submit({ id: "codex-work", provider: "codex", label, email, catalog_refresh: { provider: "codex", models: 0 } });
      await imports.getByText(`Imported ${identity}`, { exact: true }).waitFor();
      await imports.getByText("codex: 0 models found.", { exact: true }).waitFor();
    }
    await submit({ id: "codex-failed", provider: "codex", label: "Saved despite discovery failure", catalog_refresh: { provider: "codex", error: "Fixture discovery unavailable" } });
    await imports.getByRole("alert").filter({ hasText: "Fixture discovery unavailable" }).waitFor();
    await imports.getByRole("button", { name: "Refresh models", exact: true }).waitFor();
    assert.deepEqual(errors, []);
    console.log("Account login CI regression passed: failed polling stops retry/refetch; Codex and Grok restart directly; imports retain label/identity, saved ID/provider, zero-model and error outcomes.");
  } finally {
    if (browser) await browser.close();
    await new Promise(resolve => server.close(resolve));
  }
}
main().catch(error => { console.error(error); process.exitCode = 1; });
