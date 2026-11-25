// Run against the built UI with synthetic API responses.
const assert = require("node:assert/strict");
const { chromium } = require("playwright");
const { serveBuild } = require("./serve-build.cjs");

const serverName = "retired-catalog";
const toolName = "search-older-saved-records";
const toolKey = `${serverName}/${toolName}`;
const recentCalls = [{
  ts: 1_800_000_000, client_id: "services/archived-documents", server: serverName, tool: toolName,
  status: "tool_error", error: "Unknown document", latency_ms: 240, trace_id: null,
}];

async function main() {
  const { server, url } = await serveBuild();
  let browser;
  try {
    browser = await chromium.launch();
    const page = await browser.newPage({ viewport: { width: 390, height: 844 } });
    const usageRequests = [];
    await page.route("**/admin/**", (route) => {
      const { pathname, searchParams } = new URL(route.request().url());
      const group = searchParams.get("group_by");
      const period = searchParams.get("since_secs");
      if (pathname === "/admin/mcp/usage") usageRequests.push([group, period]);
      const data = {
        "/admin/session": { authenticated: true, configured: true, user: "Fixture operator" },
        "/admin/mcp/servers": [],
        "/admin/mcp/calls": recentCalls,
        "/admin/mcp/usage": period === "129600" ? [
          { key: group === "tool" ? toolKey : serverName, calls: 1, errors: 1, avg_latency_ms: 240 },
        ] : [],
      }[pathname];
      return route.fulfill({ status: data === undefined ? 404 : 200, json: data ?? {} });
    });
    await page.goto(url + "/#/mcp");
    await page.getByText(/No MCP servers yet/).waitFor();
    await page.getByText(/No tool calls in this period/).waitFor();
    await page.getByRole("cell", { name: toolKey, exact: true }).waitFor();
    assert.deepEqual(usageRequests, [["server", "86400"]]);

    const lookBack = page.getByLabel("Look back", { exact: true });
    await lookBack.fill("36hours");
    await lookBack.press("Enter");
    await page.getByRole("alert").filter({ hasText: "Enter a positive whole-number duration" }).waitFor();
    assert.deepEqual(usageRequests, [["server", "86400"]]);
    await lookBack.fill("36h");
    await lookBack.press("Enter");
    await page.getByRole("cell", { name: serverName, exact: true }).waitFor();

    const groupBy = page.getByRole("combobox", { name: "Group by" });
    await groupBy.focus();
    await page.keyboard.press("Space");
    const listbox = page.getByRole("listbox");
    const controls = await page.locator("#mcp-group-by").getAttribute("aria-controls");
    assert.ok(controls);
    assert.equal(controls, await listbox.getAttribute("id"));
    await page.waitForFunction(() => document.activeElement?.textContent === "Server");
    await page.keyboard.press("ArrowDown");
    await page.waitForFunction(() => document.activeElement?.textContent === "Tool");
    await page.keyboard.press("Enter");
    const totals = page.locator('[data-slot="card"]').filter({ has: page.getByRole("heading", { name: "Tool calls", exact: true }) });
    await totals.getByRole("cell", { name: toolKey, exact: true }).waitFor();
    assert.deepEqual(usageRequests, [["server", "86400"], ["server", "129600"], ["tool", "129600"]]);
    await page.evaluate(() => document.fonts.ready);
    const widths = await page.locator("main [data-slot=table-container]").evaluateAll((tables) =>
      tables.map((table) => ({ viewport: table.clientWidth, content: table.scrollWidth })));
    assert.equal(widths.length, 2);
    assert.ok(widths.every(({ viewport, content }) => content <= viewport), `Call table content is clipped: ${JSON.stringify(widths)}`);
    console.log("MCP query duration, keyboard grouping and narrow call tables passed.");
  } finally {
    if (browser) await browser.close();
    await new Promise((resolve) => server.close(resolve));
  }
}

main().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
