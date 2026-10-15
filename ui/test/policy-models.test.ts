import assert from "node:assert/strict";
import { test } from "vitest";

import { globMatch, providerHealth, rulesAllow, setAllowed, sortRows, usable, type Candidate } from "../src/lib/policy-models.ts";

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

test("rulesAllow: deny wins, an empty allow list allows all", () => {
  const m = { provider: "opencode-go", model: "kimi-k3" };
  assert.ok(rulesAllow({ allow: [], deny: [] }, m));
  assert.ok(!rulesAllow({ allow: ["kimi-*"], deny: ["opencode-go/*"] }, m));
  assert.ok(rulesAllow({ allow: ["opencode-go/*"], deny: [] }, m));
  assert.ok(!rulesAllow({ allow: ["claude-*"], deny: [] }, m));
});

test("turning a model off denies it and never empties the allow list", () => {
  const off = setAllowed({ allow: [], deny: [] }, [row("codex", "gpt-6")], false);
  assert.deepEqual(off, { allow: [], deny: ["codex/gpt-6"] });

  // The only allow entry stays, so the list doesn't become "allow all".
  const only = setAllowed({ allow: ["codex/gpt-6"], deny: [] }, [row("codex", "gpt-6")], false);
  assert.deepEqual(only, { allow: ["codex/gpt-6"], deny: ["codex/gpt-6"] });

  // One of several exact entries is just removed.
  const many = setAllowed({ allow: ["codex/gpt-6", "claude-*"], deny: [] }, [row("codex", "gpt-6")], false);
  assert.deepEqual(many, { allow: ["claude-*"], deny: [] });

  // Still matched by an allow glob: deny it.
  const glob = setAllowed({ allow: ["codex/*", "codex/gpt-6"], deny: [] }, [row("codex", "gpt-6")], false);
  assert.deepEqual(glob, { allow: ["codex/*"], deny: ["codex/gpt-6"] });
});

test("turning a model on removes its deny entry and adds it to a non-empty allow list", () => {
  const denied = row("codex", "gpt-6", false);
  assert.deepEqual(setAllowed({ allow: [], deny: ["codex/gpt-6", "*-preview"] }, [denied], true), { allow: [], deny: ["*-preview"] });
  assert.deepEqual(setAllowed({ allow: ["claude-*"], deny: [] }, [denied], true), { allow: ["claude-*", "codex/gpt-6"], deny: [] });
  assert.deepEqual(setAllowed({ allow: ["codex/*"], deny: ["codex/gpt-6"] }, [denied], true), { allow: ["codex/*"], deny: [] });
});

test("rows already in the wanted state are left alone", () => {
  const rules = { allow: ["claude-*"], deny: ["x"] };
  assert.deepEqual(setAllowed(rules, [row("codex", "gpt-6"), row("claude", "claude-opus-5")], true), rules);
  assert.deepEqual(setAllowed(rules, [row("codex", "gpt-6", false)], false), rules);
});

test("bulk changes apply to every row", () => {
  const rows = [row("codex", "a"), row("codex", "b"), row("claude", "c", false)];
  assert.deepEqual(setAllowed({ allow: [], deny: ["claude/c"] }, rows, false), { allow: [], deny: ["claude/c", "codex/a", "codex/b"] });
  assert.deepEqual(setAllowed({ allow: [], deny: ["claude/c"] }, rows, true), { allow: [], deny: [] });
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
    { provider: "codex", available: true },
    { provider: "codex", available: false },
    { provider: "opencode-go", available: false },
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
});
