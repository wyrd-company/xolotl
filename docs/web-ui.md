# Web UI

`xolotl serve` serves an admin web UI at `/`. It is a client of the same `/admin` API the CLI uses, plus a few endpoints added for it.

## Screens

| Screen | Covers | API |
|---|---|---|
| Overview | OpenBao and SQLite health, pool availability and usage windows, 24 h totals, gateway providers, recent failed requests | `GET /admin/status`, `/admin/accounts`, `/admin/providers`, `/admin/usage` |
| Accounts | List by provider; refresh usage, enable/disable, priority, clear cooldown, remove; details | `/admin/accounts/...` |
| Add account | Codex device or browser login, Claude paste login, OpenCode Go key, import of a CLI credential file | `/admin/auth/...`, `POST /admin/accounts/import` |
| Policies | Providers with their account or key state, scopes, precedence, MCP access, rate limits with the policy's consumption. The model list shows every model of the allowed providers: allow/deny toggles (with allow/deny all for the filtered rows), filter by provider, sort, inline rename, more names with fallbacks, the winner of a shared name, and the provider's account state. The toggles write exact `provider/model` entries to `allow_models` / `deny_models`; the globs stay editable under Model patterns | `/admin/policies/...`, `POST /admin/policy-preview`, `GET /admin/accounts`, `GET /admin/providers` |
| Client keys | Issue, rotate with grace, enable/disable, revoke. Consumption against the policy's rate limits. Shows the vault path and the `bao` command and Vault policy the client needs, never the key | `/admin/keys/...` |
| MCP servers | Upstream MCP servers from the config file and OpenBao: add, edit and remove the latter; header secrets; OAuth sign-in; tools; recent tool calls | `/admin/mcp/...` |
| Providers & models | Gateway providers from the config file and their key state; set a key; the catalog with each model's endpoints | `GET /admin/providers`, `GET /admin/catalog/routes`, `POST /admin/catalog/refresh` |
| Usage | Request log grouped by client, account, provider or model. Selecting a row narrows the view to it and groups by the first of model, account, client and provider that is neither shown nor narrowed yet; filters show as removable chips and live in the hash route (`/#/usage?group=model&client=…`). Link to `telemetry.dashboard_url` | `GET /admin/usage?group_by=…&since_secs=…&client=…&account=…&provider=…&model=…` (an empty filter value matches rows without that key, such as the admin token) |

## Model discovery

Model lists and counts include a shared **Refresh models** control, the age of the
catalog, and each provider's model count or discovery error. The policy model list
uses its selected providers, narrowed to the provider filter when set. Account views use their provider pools: discovery
is shared across accounts of a provider, not performed for one account. A failed
discovery keeps cached and configured static models visible and labels the error.
A successful empty response is shown as **Provider returned no models**.

`GET /admin/catalog/status` returns each provider's `refreshed_at` (Unix seconds
of the last attempt, or null), `models` (including cached and static entries), and
`error` (or null). `POST /admin/catalog/refresh?providers=provider-a,provider-b`
refreshes those providers; omit the query to refresh all providers. Its response
remains the provider-to-model-list map used by the CLI.

Account imports, completed logins, reset, enable/disable, removal, and provider key
changes trigger discovery, as does admin reload. The UI checks discovery status
every 30 seconds and updates model lists after background discovery completes. Manual refresh updates
all open model queries immediately after completion.

## Sign-in

- **OpenID Connect.** The UI signs in through an OpenID Connect provider (built against Pocket ID). Xolotl is a confidential client and uses the authorization code flow with PKCE, all on the server; the browser never holds a provider token. `[ui.oidc]` in the config file names the issuer, client ID, redirect URL and admin group. The client secret is the vault secret `oidc` (field `client_secret`). Without `[ui.oidc]`, nobody can sign in to the UI.
- **Flow.** `GET /admin/session/login` discovers the provider, keeps the `state`, nonce, PKCE verifier and provider metadata in memory for 10 minutes, sets the `xolotl_oidc` cookie to the `state` (`HttpOnly`, `SameSite=Lax` because the callback is a navigation from the provider's site, scoped to the callback path), and redirects to the provider. `GET /admin/session/callback` completes only in the browser that started the sign-in, once per `state`. It exchanges the code, checks the ID token (an asymmetric signature from the provider's keys — RSA, ECDSA or EdDSA, never HMAC — issuer, audience, expiry, nonce), and redirects to `/`. A failed sign-in redirects to `/?signin=<reason>` with one of a fixed set of reasons (`denied`, `expired`, `failed`, `forbidden`, `unavailable`); the details go to the log.
- **Admin group.** Only a user whose `groups` claim contains `admin_group` gets a session. Xolotl requests the `groups` scope and checks the claim itself, so loosening the client's allowed groups in the provider does not widen admin access.
- **Session.** The callback sets `xolotl_session`: `HttpOnly`, `SameSite=Strict`, `Path=/`, 12 hours. Both cookies are `Secure` when `redirect_url` is `https`. The session does not depend on the provider's tokens, which are not kept. `GET /admin/session` returns the CSRF token and the user's name (`preferred_username`, or the subject). `DELETE /admin/session` signs out of xolotl only; the provider session stays.
- **CSRF.** A session authorizes a request only with its CSRF token in `x-csrf-token`, on every method. Browsers never add that header to a cross-site request, and the cookie is `SameSite=Strict` as well.
- **Scope.** A session is an admin principal for `/admin` routes only; it never authorizes the LLM or management endpoints. A request that presents a key is judged by the key alone.
- **Limits.** At most 256 sign-ins in progress are kept; a new one drops the oldest. Starting a sign-in is unauthenticated, so memory stays bounded, and a burst of requests can't block sign-in for the next 10 minutes. Each start makes two requests to the provider (discovery and its keys); provider responses are limited to 1 MiB. One browser has one sign-in in progress: starting a second (another tab) makes the first fail with `expired`, and the second still completes.
- **State.** Sessions and sign-ins in progress live in memory: a restart signs everyone out, and instances don't share them.

### Decisions

- **No password sign-in.** When the provider is down, the CLI with the bootstrap admin token does everything the UI does. A second browser sign-in would be one more auth surface to keep safe for no new capability.
- **Xolotl checks the group.** The provider can also limit a client to groups, but that setting lives outside xolotl and is easy to change without thinking of admin access. Checking `groups` in xolotl keeps the admin boundary in xolotl's own config.
- **Discovery per sign-in.** Sign-ins are rare (sessions last 12 hours), so xolotl discovers the provider on each one instead of caching metadata and keys. That keeps signing-key rotation correct without a cache to expire, and the server starts when the provider is down.
- **Local sign-out.** Signing out of the UI does not end the provider session, which other applications share.

## Build and serving

`ui/` is a Vite + React + TypeScript app with Tailwind CSS v4 and shadcn/ui components (in `ui/src/components/ui`, written from shadcn's source because its registry is not reachable from every build environment). Data comes from TanStack Query; routes are hash routes (`/#/accounts`).

- `pnpm build` writes `ui/dist`. Release builds embed it with `rust-embed`; debug builds read it from disk, so a rebuilt UI shows up after a reload. A binary built without `ui/dist` serves a page saying so.
- The app claims `/`, `/assets/*` and the icon files browsers look for at the root (`favicon.ico`, `favicon.svg`, `apple-touch-icon.png`, `site.webmanifest` and the manifest's icons, from `ui/public`). Every screen is in the URL fragment, so no other path is taken from the `/<provider>/...` passthrough; `assets` (like `ui`) is rejected as a gateway provider name.
- The UI has a light and a dark theme. It follows the browser's color scheme unless a theme is picked in the sidebar (system, light or dark); the choice is kept in `localStorage`. Theme-dependent colors in `ui/src/index.css` use `light-dark()` pairs; the sidebar is dark in both themes.
- `index.html` is served with `no-cache` and a strict Content-Security-Policy (same-origin scripts, styles, fonts and requests; no framing). Hashed files under `/assets/` are cached for a year; the root icon files for a day. Fonts (IBM Plex) are bundled, so the UI loads nothing from other hosts.
- The emblem and its icons are described in `brand/README.md`.
- The Docker image builds the UI in a Node stage; CI runs `pnpm test` (Vitest on `ui/src` and `ui/test`) and `pnpm build` (which type-checks) in its own job, and the release workflow builds the UI before the binary.
