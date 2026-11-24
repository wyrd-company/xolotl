---
xolotl: minor
xolotl-docs: minor
---

Excluding a model in a policy no longer breaks the conversations already on it. The policy checks a model when a conversation first uses it: when the conversation starts with it or switches to it. After a rule, its `models` switch or `deny_other_models` excludes the model, a conversation on it goes on with no error, on the same provider and, once it holds a context lock, the same account. A new conversation can't use the model, in a new session or in the same one, and a conversation that switches to another model can't switch back. Background requests and subagents that a client sends under the same session are other conversations: they don't switch it, and a new subagent can't start on the excluded model. A compaction that replaces the conversation's first message starts a new conversation. A rotated key keeps its conversations; a key revoked and issued again starts with none. Removing a provider or an account from a policy still applies to every session at once. The Policies and client keys page explains it.
