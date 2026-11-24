---
xolotl: minor
xolotl-docs: minor
---

Excluding a model in a policy no longer breaks the sessions already on it. The policy checks a model when a session first uses it: when a conversation of the session starts with it or switches to it. After a rule, its `models` switch or `deny_other_models` excludes the model, a session on it goes on with no error, on the same provider and, once it holds a context lock, the same account, including its subagents and the conversation after a compaction. A new session can't use the model, and a conversation that switches to another model can't switch back, also after a compaction, unless another conversation of its session, such as a subagent at work, still uses it. This was checked against Claude Code, Codex, Grok and OpenCode. Xolotl now also reads the session ID that the OpenCode client sends (`x-opencode-session-id`), so OpenCode conversations get their own session for the context lock too. A rotated key keeps its sessions; a key revoked and issued again starts with none. Removing a provider or an account from a policy still applies to every session at once. The Policies and client keys page explains it.
