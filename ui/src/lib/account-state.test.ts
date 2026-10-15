// ---
// relationships:
//   tests: account-state
// ---
import { expect, test } from "vitest";
import { accountState, accountPath } from "./account-state";
import type { Account } from "./api";

test("recovery labels distinguish API key rejection from OAuth login", () => {
  const account: Account = {
    id: "example-account",
    provider: "opencode-go",
    label: "Example",
    email: null,
    external_id: null,
    plan: null,
    enabled: true,
    priority: 0,
    cooldown_until: null,
    cooldown_reason: null,
    auth_failed: true,
    last_error: "upstream rejected credentials (401)",
    last_used_at: null,
    created_at: 0,
    auth_index: "example",
    available: false,
    expires_at: null,
    unsaved: false,
    usage: null,
  };
  expect(accountState(account).label).toBe("Key rejected");
  expect(accountState({ ...account, last_error: "upstream rejected credentials (403)" }).label).toBe("Refused");
  for (const provider of ["codex", "claude", "grok"] as const) expect(accountState({ ...account, provider }).label).toBe("Re-login needed");
  expect(accountState({ ...account, enabled: false }).label).toBe("Disabled");
  expect(accountState({ ...account, auth_failed: false, cooldown_until: Date.now() / 1000 + 60 }).label).toBe("Cooling down");
  expect(accountState({ ...account, auth_failed: false, cooldown_until: 1, available: true }).label).toBe("Available");
});

test("attention links identify and encode the existing account", () => {
  expect(accountPath("example&other=account")).toBe("/accounts?account=example%26other%3Daccount");
});
