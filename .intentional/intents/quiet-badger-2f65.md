---
xolotl: minor
xolotl-docs: minor
---

Excluding a model in a policy no longer breaks the sessions already using it. The policy checks a model when a session first uses it. Once an answer from the model completes, the session keeps it: after a rule, its `models` switch or `deny_other_models` excludes the model, the session goes on using it in any way, with no error. That covers switching away and back, subagents, background requests and compactions, on the same provider and, once it holds a context lock, the same account. A session that never used the model can't use it. This was checked against Claude Code, Codex, Grok and OpenCode. Xolotl now also reads the session ID that the OpenCode client sends (`x-opencode-session-id`), so OpenCode conversations get their own session for the context lock too. A rotated key keeps its sessions; a key revoked and issued again starts with none. Removing a provider or an account from a policy still applies to every session at once. The Policies and client keys page explains it.
