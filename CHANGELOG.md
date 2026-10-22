# Changelog

## 0.5.0

### Features

- Expose cached model context and price metadata with overrides, Claude Code context hints and token estimates, API-equivalent usage costs, and an authenticated Codex-native model catalog.

### Fixes

- Show Claude weekly model limits and available Claude and Codex resets; hide internal usage buckets.
- Focus Overview on actionable failures, configured provider health and 24-hour traffic, with account fixes and errors-only Usage links.
- Add a keyboard-accessible repository and issue-reporting link to the web UI sidebar.
- Keep policy and MCP server save actions visible while scrolling, and keep button labels readable before hover and while disabled in both themes.

## 0.4.0

### Features

- Policies choose the accounts each subscription provider may use; a provider without an entry has none. Model rules replace `allow_models` and `deny_models`: one ordered `rules` list of allow and deny globs where the first match decides. Turn single models on or off with the new `models` table; rules win over those switches, and `deny_other_models` sets the rest. Limit requests and tokens per provider and per account. The policy editor lists accounts per provider, edits the rules in order or shows the effective models with overrides, can hide denied models, keeps precedence in the model list, and folds each section.

## 0.3.0

### Features

- Policies can set rate limits: requests and tokens per minute and per day, counted per key or shared by the policy. A request over a limit gets 429 with retry-after in the endpoint's error shape. Consumption shows in `xolotl keys list`, the admin API, the web UI and metrics.
- Narrow usage by client, account, provider or model: `GET /admin/usage` and `xolotl usage` take filters, and selecting a row on the web UI's Usage page narrows the view to it, with the filters kept in the link.
- Show Codex reset credits on the overview and account details.
- The policy editor shows a policy's models as one list: allow or deny each model (or all shown models), filter by provider, sort, rename models inline, add more names with fallbacks, and pick who serves a shared name. Each route shows its provider's account count or key state, and providers without usable accounts are greyed out. Policies gain `renames` (`provider/model` → the name keys see instead of the plain name).
- Add the xolotl emblem: SVG masters and icon derivatives in `brand/`, the emblem in the web UI's sidebar and sign-in card, a favicon, Apple touch icon and web manifest served at the root, and a UI palette that takes its ink and rust from the emblem.
- The web UI has a dark theme. It follows the system color scheme, and a sidebar toggle can pick light or dark.

### Fixes

- Refresh model lists in place and show per-provider discovery status.
- Diagnose uncertain OAuth refreshes, back off retries, and refresh Claude imports with unknown expiry before use.
- Fix OpenCode Go session forwarding, request refusals, and account error diagnostics
- Make account recovery provider-aware, replace API keys in place, and refresh models after recovery.
- Add user documentation, published to wyrd.foo, with a demo of issuing a client key.

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
