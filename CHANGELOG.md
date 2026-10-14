# Changelog

## 0.2.0

### Features

- Sign in to the web UI with OpenID Connect (tested with Pocket ID) instead of a password. Configure `[ui.oidc]` (issuer, client ID, redirect URL, admin group) and store the client secret in the vault secret `oidc`; only members of the admin group get a session. `xolotl ui set-password`, `PUT /admin/ui/password` and `POST /admin/session` are removed, and the `ui_password_hash` field of the `admin` secret is no longer read.
- Add an MCP gateway: upstream Streamable HTTP MCP servers from `[mcp.servers]` (several instances of one server with different credentials, header secrets in OpenBao) exposed to clients at `/mcp` as a code-mode MCP server with `search` and `execute` tools. Policies gain the `mcp` scope, `mcp_servers`, `allow_tools` and `deny_tools`. Includes an MCP call log, metrics, `xolotl mcp` commands and an MCP servers page in the web UI.
  
  Upstream servers can use OAuth logins (`auth = "oauth"`, `xolotl mcp login`): discovery, pre-registered clients, client metadata documents or dynamic registration, PKCE, token refresh, with a paste-back or `server.public_url` callback finish.
  
  MCP servers can also be added, changed and removed at runtime from the web UI or `xolotl mcp put` / `remove`; they are stored in OpenBao, and removal is refused while a policy lists the server.
- Add Grok Build (xAI) subscription accounts: `xolotl auth login grok` (device code), token refresh, a `grok` pool on the Responses wire with keepalive rewriting, and the Grok CLI's `/v1/models` shape.

### Fixes

- Fix concurrent AMD64 and ARM64 container builds by separating Cargo caches for each architecture.

## 0.1.0

### Features

- Add an admin web UI at `/`: accounts and logins, policies with a live preview of the names a key sees (including translated endpoints), client keys, provider keys and the model catalog, and usage. Sign in with one password, set with `xolotl ui set-password` and stored as an argon2id hash in OpenBao. New admin endpoints `GET /admin/status`, `GET /admin/providers`, `GET /admin/catalog/routes` and `POST /admin/policy-preview`, and `telemetry.dashboard_url` for the link to OpenObserve.
- First release of the Xolotl proxy gateway: Codex, Claude and OpenCode Go subscription pools with OAuth login, token refresh, load balancing, quota cooldowns and usage tracking; an AI gateway for Vercel AI Gateway, Mistral and OpenAI-compatible servers; policy-bound client keys in OpenBao with model precedence and aliases; T3Code hubs compatibility; and OpenTelemetry traces, logs and metrics.
- Translate between Anthropic Messages, OpenAI Responses and Chat Completions, so models are reachable on all three endpoints: Claude Code can use Codex and Chat-only models, and Codex CLI can use Anthropic models from Vercel or OpenCode Go. Claude subscription accounts stay on the Claude Code API path (`/v1/messages`) only. Requests, JSON and streamed responses, tool calls and results, reasoning, images, usage and errors are translated; `GET /v1/models` and `xolotl policies view` mark translated endpoints.
