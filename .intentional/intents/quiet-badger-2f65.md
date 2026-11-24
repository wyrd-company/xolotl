---
xolotl: minor
xolotl-docs: minor
---

Excluding a model in a policy no longer breaks the conversations already on it. The policy checks a model when a session first uses it: when the session starts with it or switches to it. After a rule, its `models` switch or `deny_other_models` excludes the model, a conversation on it goes on with no error, on the same provider and, once it holds a context lock, the same account. A new session can't use the model, and a conversation that switches to another model can't switch back. Background requests and subagents that a client sends on other models under the same session are not a switch. Removing a provider or an account from a policy still applies to every session at once. The Policies and client keys page explains it.
