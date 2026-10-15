# Xolotl design notes

These pages describe how xolotl is built and why. For installing, configuring and running it, see the [user documentation](../overview.md).

| Page | Covers |
|---|---|
| [Architecture](architecture.md) | Components, storage split, background tasks, source layout |
| [Routing and policies](routing.md) | Model catalog, wire formats and translation between them, how a policy turns a model name into a target, passthroughs |
| [Subscription accounts](subscriptions.md) | Codex, Claude and OpenCode Go accounts: login, token refresh, account selection, failover, usage, T3Code hubs |
| [MCP gateway](mcp.md) | Upstream MCP servers, code mode (`search` / `execute`), the QuickJS sandbox, OAuth logins, MCP policies, call log |
| [Web UI](web-ui.md) | OpenID Connect sign-in, sessions and CSRF, how the UI is built and served |
| [Client keys](client-keys.md) | Key records in OpenBao, authentication, rotation and revocation |
| [Rate limits](limits.md) | Request and token limits on policies: windows, per-key or per-policy counting, 429 responses, consumption |
| [Releases](releases.md) | Intentional, CI, and the release workflow's tag guard |
| [Known limitations](../limitations.md) | What isn't done yet, with the issues that track it |
