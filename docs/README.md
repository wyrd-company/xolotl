# Xolotl design notes

These pages describe how xolotl is built and why. For installing, configuring and running it, see the [project README](../README.md).

| Page | Covers |
|---|---|
| [Architecture](architecture.md) | Components, storage split, background tasks, source layout |
| [Routing and policies](routing.md) | Model catalog, wire formats and translation between them, how a policy turns a model name into a target, passthroughs |
| [Subscription accounts](subscriptions.md) | Codex, Claude and OpenCode Go accounts: login, token refresh, account selection, failover, usage, T3Code hubs |
| [Client keys](client-keys.md) | Key records in OpenBao, authentication, rotation and revocation |
| [Releases](releases.md) | Intentional, CI, and the release workflow's tag guard |
| [Known limitations](limitations.md) | What isn't done yet, with the issues that track it |
