// Run against a standard-container fixture with policies routes, broken and blocked.
// routes: codex + claude fixture-model, alias fast; broken/blocked: invalid stored
// fields, and blocked/client is an active key. Authenticate with OIDC first and
// pass SESSION_FILE. This script performs product mutations only through controls.
const { chromium } = require(process.env.PLAYWRIGHT_MODULE || 'playwright');
const { writeFileSync, mkdirSync } = require('node:fs');
const assert = require('node:assert/strict');
const base = process.env.XOLOTL_URL || 'http://localhost:41317';
const output = process.env.EVIDENCE_DIR || '/evidence';
const result = { status: 'failed', checks: [], errors: [] };
mkdirSync(output, { recursive: true });
const check = name => result.checks.push(name);
const fault = (route, message) => route.fulfill({ status: 503, contentType: 'application/json', body: JSON.stringify({ error: { message } }) });
(async () => {
  const browser = await chromium.launch({ headless: true });
  let page;
  try {
    page = await browser.newPage({ storageState: process.env.SESSION_FILE || `${output}/session.json`, viewport: { width: 1440, height: 1000 } });
    page.on('pageerror', error => result.errors.push(error.message));
    await page.goto(`${base}/#/policies/routes`);
    const routes = page.getByRole('table', { name: 'Resolved model routes' });
    await routes.waitFor();
    const alias = routes.getByRole('row').filter({ hasText: 'fast' });
    assert.deepEqual(await alias.locator('ol li').allTextContents(), ['codex/fixture-model', 'claude/fixture-model']);
    for (const text of ['/v1/responses', '/v1/messages', '/v1/chat/completions']) assert((await alias.innerText()).includes(text));
    assert.equal(await page.getByText('No accounts', { exact: true }).count() >= 2, true);
    await page.screenshot({ path: `${output}/routes.png`, fullPage: true });
    await page.locator('[data-slot=card]').filter({ has: page.getByRole('button', { name: 'Resolved routes', exact: true }) }).screenshot({ path: `${output}/policy-routes.png` });
    check('Resolved alias, ordered fallback targets, native/translated endpoints and empty pools');

    let statusFailed = true;
    await page.route('**/admin/status', route => statusFailed ? fault(route, 'fixture status read denied') : route.continue());
    await page.reload();
    const statusError = page.getByRole('alert').filter({ hasText: 'Could not check for unloaded policies' });
    await statusError.getByText('fixture status read denied').waitFor({ timeout: 20000 });
    assert(await routes.isVisible());
    await page.screenshot({ path: `${output}/status-failed.png`, fullPage: true });
    statusFailed = false;
    await statusError.getByRole('button', { name: 'Retry', exact: true }).click();
    await statusError.waitFor({ state: 'detached' });
    await page.unroute('**/admin/status');
    check('Status failure shows cause and retry while loaded policy remains inspectable');

    let accountsFailed = true;
    await page.route('**/admin/accounts', route => accountsFailed ? fault(route, 'fixture accounts unavailable') : route.continue());
    await page.route('**/admin/policy-preview', async route => {
      if (!accountsFailed) return route.continue();
      const response = await route.fetch();
      const data = await response.json();
      data.accounts = null;
      await route.fulfill({ response, json: data });
    });
    await page.reload();
    await page.getByText('Could not read the accounts', { exact: true }).waitFor();
    await page.getByText('fixture accounts unavailable', { exact: true }).waitFor({ timeout: 20000 });
    assert(await routes.isVisible());
    await page.screenshot({ path: `${output}/accounts-failed.png`, fullPage: true });
    accountsFailed = false;
    await page.getByRole('button', { name: 'Retry accounts', exact: true }).click();
    await page.getByText('Could not read the accounts', { exact: true }).waitFor({ state: 'detached' });
    await page.getByRole('alert').filter({ hasText: 'Provider accounts could not be read' }).getByRole('button', { name: 'Retry', exact: true }).click();
    await page.getByText('fixture accounts unavailable', { exact: true }).waitFor({ state: 'detached' });
    await page.unroute('**/admin/accounts');
    await page.unroute('**/admin/policy-preview');
    check('Account read failures remain distinct from empty pools and retry successfully');

    let previewFailed = true;
    await page.route('**/admin/policy-preview', route => previewFailed ? fault(route, 'fixture preview unavailable') : route.continue());
    await page.reload();
    const previewError = page.getByRole('alert').filter({ hasText: 'Could not resolve policy' });
    await previewError.waitFor();
    assert((await previewError.innerText()).includes('fixture preview unavailable'));
    assert.equal(await routes.count(), 0);
    previewFailed = false;
    await previewError.getByRole('button', { name: 'Retry', exact: true }).click();
    await routes.waitFor();
    await page.unroute('**/admin/policy-preview');
    check('Preview failure shows cause/retry and withholds stale routing rows');

    let keysFailed = true;
    await page.route('**/admin/keys', route => keysFailed ? fault(route, 'fixture key list unavailable') : route.continue());
    await page.reload();
    const keyError = page.getByRole('alert').filter({ hasText: 'Could not check which policies have keys' });
    await keyError.waitFor({ timeout: 20000 });
    assert(await page.getByRole('button', { name: 'Delete unloaded policy broken', exact: true }).isDisabled());
    assert(await page.getByRole('button', { name: 'Delete', exact: true }).isDisabled());
    keysFailed = false;
    await keyError.getByRole('button', { name: 'Retry', exact: true }).click();
    await keyError.waitFor({ state: 'detached' });
    assert(await page.getByRole('button', { name: 'Delete unloaded policy blocked', exact: true }).isDisabled());
    await page.unroute('**/admin/keys');
    check('Unavailable key list disables deletion; known key use disables unloaded-policy deletion');

    // Simulate a stale client key list; the real server must still reject deletion.
    await page.route('**/admin/keys', async route => {
      const response = await route.fetch();
      await route.fulfill({ response, json: (await response.json()).filter(key => key.policy !== 'blocked') });
    });
    await page.reload();
    await page.getByRole('button', { name: 'Delete unloaded policy blocked', exact: true }).click();
    await page.getByRole('alertdialog').getByRole('button', { name: 'Delete policy', exact: true }).click();
    await page.getByLabel('Policies that did not load').getByText('policy blocked still has keys; revoke them first', { exact: true }).waitFor();
    await page.getByRole('button', { name: 'Delete unloaded policy blocked', exact: true }).waitFor();
    await page.screenshot({ path: `${output}/delete-rejected.png`, fullPage: true });
    await page.unroute('**/admin/keys');
    check('Real server rejects unloaded-policy deletion despite stale key list; cause stays visible');

    const broken = page.getByRole('button', { name: 'Delete unloaded policy broken', exact: true });
    await broken.click();
    await page.getByRole('alertdialog').getByRole('button', { name: 'Cancel', exact: true }).click();
    await broken.waitFor();
    await broken.click();
    await page.getByRole('alertdialog').getByRole('button', { name: 'Delete policy', exact: true }).click();
    await broken.waitFor({ state: 'detached' });
    await page.reload();
    await routes.waitFor();
    assert.equal(await broken.count(), 0);
    check('Unloaded-policy deletion supports cancel, deletes through UI, and survives refresh');

    await page.getByRole('link', { name: 'New policy', exact: true }).click();
    await page.getByLabel('Name', { exact: true }).fill('browser-hub');
    await page.getByRole('checkbox', { name: 'proxy', exact: true }).uncheck();
    await page.getByRole('checkbox', { name: 'management (T3Code hubs)', exact: true }).check();
    await page.getByRole('button', { name: 'Create policy', exact: true }).click();
    await page.waitForURL('**/#/policies/browser-hub');
    await page.getByText('No subscription providers allowed.', { exact: true }).waitFor();
    assert.equal(await page.getByRole('alert').filter({ hasText: 'Could not resolve policy' }).count(), 0);
    check('Management-only policy saves without provider/MCP/key-policy entries');

    await page.getByRole('link', { name: 'New policy', exact: true }).click();
    await page.getByLabel('Name', { exact: true }).fill('browser-warned');
    await page.getByRole('checkbox', { name: 'codex', exact: true }).check();
    const saved = page.waitForResponse(response => response.url().endsWith('/admin/policies/browser-warned') && response.request().method() === 'PUT');
    await page.getByRole('button', { name: 'Create policy', exact: true }).click();
    const warnings = (await (await saved).json()).warnings;
    assert(warnings.length > 0);
    await page.waitForURL('**/#/policies/browser-warned');
    const banner = page.getByRole('status', { name: 'Saved policy warnings' });
    await banner.waitFor();
    for (const warning of warnings) assert((await banner.innerText()).includes(warning));
    await page.getByLabel('Description', { exact: true }).fill('Changed through browser');
    await page.getByRole('button', { name: 'Save policy', exact: true }).click();
    await page.getByText('Saved policy browser-warned', { exact: true }).waitFor();
    for (const warning of warnings) assert((await banner.innerText()).includes(warning));
    await page.screenshot({ path: `${output}/saved-warnings.png`, fullPage: true });
    check('All real server warnings survive create navigation and an existing-policy save');

    assert.deepEqual(result.errors, []);
    result.status = 'passed';
  } catch (error) {
    if (page) {
      await page.screenshot({ path: `${output}/failure.png`, fullPage: true });
      writeFileSync(`${output}/failure.txt`, await page.locator('body').innerText());
    }
    throw error;
  } finally {
    await browser.close();
    writeFileSync(`${output}/acceptance.json`, JSON.stringify(result, null, 2));
  }
})().catch(error => { result.failure = error.stack; writeFileSync(`${output}/acceptance.json`, JSON.stringify(result, null, 2)); console.error(error); process.exit(1); });
