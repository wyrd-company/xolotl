// relationships:
//   tests: ui/src/lib/account-state.ts
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { accountState, accountPath } from '../src/lib/account-state.ts';

test('recovery labels distinguish API key rejection from OAuth login', () => {
  const account = { enabled: true, auth_failed: true, provider: 'opencode-go', last_error: 'upstream rejected credentials (401)' };
  assert.equal(accountState(account).label, 'Key rejected');
  assert.equal(accountState({ ...account, last_error: 'upstream rejected credentials (403)' }).label, 'Refused');
  for (const provider of ['codex', 'claude', 'grok']) assert.equal(accountState({ ...account, provider }).label, 'Re-login needed');
  assert.equal(accountState({ ...account, enabled: false }).label, 'Disabled');
  assert.equal(accountState({ ...account, auth_failed: false, cooldown_until: Date.now() / 1000 + 60 }).label, 'Cooling down');
  assert.equal(accountState({ ...account, auth_failed: false, cooldown_until: 1, available: true }).label, 'Available');
});

test('attention links identify and encode the existing account', () => {
  assert.equal(accountPath('example&other=account'), '/accounts?account=example%26other%3Daccount');
});
