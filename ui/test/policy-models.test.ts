import assert from "node:assert/strict";
import { test } from "vitest";

import {
  accountProvider,
  aliasesReaching,
  allowsAccount,
  globMatch,
  providerHealth,
  decide,
  matching,
  setToggles,
  sortRows,
  usable,
  type Candidate,
} from "../src/lib/policy-models.ts";

const row = (provider: string, model: string, allowed = true, name: string | null = model): Candidate => ({
  provider,
  model,
  allowed,
  rule: null,
  name: allowed ? name : null,
});

test("globMatch follows the server's globs", () => {
  assert.ok(globMatch("opencode-go/*", "opencode-go/kimi-k3"));
  assert.ok(globMatch("*-preview", "gpt-6-preview"));
  assert.ok(globMatch("a*b*c", "axxbyyc"));
  assert.ok(!globMatch("a*b*c", "axxc"));
  assert.ok(!globMatch("ab*ba", "aba"), "first and last parts can't overlap");
  assert.ok(!globMatch("kimi", "kimi-k3"));
});

test("decide: deny patterns, then allow patterns, then toggles, then the default", () => {
  const m = { provider: "opencode-go", model: "kimi-k3" };
  const rules = (allow: string[], deny: string[], models: Record<string, boolean> = {}) => ({ allow, deny, models });
  assert.deepEqual(decide(rules([], []), m), { allowed: true });
  assert.deepEqual(decide(rules(["claude-*"], []), m), { allowed: false }, "allow patterns: off by default");
  assert.deepEqual(decide(rules(["claude-*"], [], { "opencode-go/kimi-k3": true }), m), { allowed: true }, "toggled on");
  assert.deepEqual(decide(rules([], [], { "opencode-go/kimi-k3": false }), m), { allowed: false }, "toggled off");
  assert.deepEqual(decide(rules(["kimi-*"], [], { "opencode-go/kimi-k3": false }), m), {
    allowed: true,
    pattern: { kind: "allow", pattern: "kimi-*" },
  });
  assert.deepEqual(decide(rules(["kimi-*"], ["opencode-go/*"]), m), { allowed: false, pattern: { kind: "deny", pattern: "opencode-go/*" } });
});

test("setToggles writes toggles only", () => {
  const rows = [row("codex", "a"), row("codex", "b")];
  assert.deepEqual(setToggles({ "claude/c": true }, rows, false), { "claude/c": true, "codex/a": false, "codex/b": false });
  assert.deepEqual(
    matching("codex/*", [...rows, row("claude", "c")]).map((c) => c.model),
    ["a", "b"],
  );
  assert.deepEqual(matching("", rows), []);
});

test("default sort puts allowed models first, then by name", () => {
  const rows = [row("codex", "b"), row("vercel", "a", false), row("claude", "c"), row("claude", "a", true, "zz")];
  const order = (by: Parameters<typeof sortRows>[1]) => sortRows(rows, by).map((c) => `${c.provider}/${c.model}`);
  assert.deepEqual(order("allowed"), ["codex/b", "claude/c", "claude/a", "vercel/a"]);
  assert.deepEqual(order("name"), ["vercel/a", "codex/b", "claude/c", "claude/a"]);
  assert.deepEqual(order("provider"), ["claude/c", "claude/a", "codex/b", "vercel/a"]);
});

test("provider health counts available accounts and gateway keys", () => {
  const pools = ["codex", "opencode-go"];
  const accounts = [
    { id: "codex-a1", provider: "codex", available: true },
    { id: "codex-b2", provider: "codex", available: false },
    { id: "opencode-go-c3", provider: "opencode-go", available: false },
  ];
  const gateway = [
    { name: "vercel", has_key: false, no_auth: false },
    { name: "llama", has_key: false, no_auth: true },
  ];
  const h = (p: string) => providerHealth(p, pools, accounts, gateway);
  assert.deepEqual(h("codex"), { kind: "pool", total: 2, available: 1 });
  assert.deepEqual(h("opencode-go"), { kind: "pool", total: 1, available: 0 });
  assert.deepEqual(h("vercel"), { kind: "gateway", ready: false });
  assert.deepEqual(h("llama"), { kind: "gateway", ready: true });
  assert.deepEqual(h("nope"), { kind: "unknown" });
  assert.deepEqual(
    ["codex", "opencode-go", "vercel", "llama"].map((p) => usable(h(p))),
    [true, false, false, true],
  );
  // Only the accounts the policy uses count.
  assert.deepEqual(providerHealth("codex", pools, accounts, gateway, { codex: ["codex-b2"] }), { kind: "pool", total: 1, available: 0 });
  assert.deepEqual(providerHealth("codex", pools, accounts, gateway, { codex: ["*"] }), { kind: "pool", total: 2, available: 1 });
});

test("policy accounts: entries, * and account ids", () => {
  assert.equal(allowsAccount({}, "codex", "codex-a1"), false, "no entry: no account");
  assert.equal(allowsAccount({ codex: ["codex-a1"] }, "codex", "codex-b2"), false);
  assert.equal(allowsAccount({ codex: ["*"] }, "codex", "codex-new"), true);
  const pools = ["codex", "opencode-go"];
  assert.equal(accountProvider("opencode-go-c3", pools), "opencode-go");
  assert.equal(accountProvider("codex-", pools), undefined);
  assert.equal(accountProvider("llama-a1", pools), undefined);
});

test("aliasesReaching finds first targets and fallbacks", () => {
  const aliases = [
    { name: "short", targets: ["codex/a"] },
    { name: "fast", targets: ["codex/b", "codex/a"] },
    { name: "other", targets: ["codex/b"] },
    { name: "near", targets: ["codex/aa"] },
  ];
  assert.deepEqual(aliasesReaching(aliases, "codex/a"), [
    { index: 0, name: "short", fallback: false },
    { index: 1, name: "fast", fallback: true },
  ]);
});
