# Subscription accounts

`src/subscription/` manages the pools for `codex` and `claude` (OAuth accounts from consumer subscriptions) and `opencode-go` (API-key accounts).

## Accounts

- **Storage:** credentials live in OpenBao at `accounts/<provider>/<id>`.
  - The id is `<provider>-` followed by the first 10 hex characters of a SHA-256 of the account's external identity (ChatGPT account id, email, or key). Logging in to the same account twice updates it instead of adding a duplicate.
  - SQLite holds an index row per account: label, email, plan, enabled, priority, cooldown, "re-login needed" flag, last error.
- **Adding accounts:**
  - **Codex, device code** (default): `xolotl auth login codex`. The server starts the flow and polls the token endpoint in the background; the CLI polls `/admin/auth/flows/<id>`.
  - **Codex, browser:** `--browser`. A PKCE flow where the user pastes the redirect URL back.
  - **Claude:** a PKCE flow with Anthropic's manual redirect. The user pastes back `CODE#STATE`.
  - **Import:** from `~/.codex/auth.json` or `~/.claude/.credentials.json`.
  - **OpenCode Go:** `xolotl accounts add-key`.

## Token refresh

`Subscriptions::credential` returns a usable credential, refreshing it first when needed.

- **Before expiry:** when the token expires within `refresh_margin_secs` (default 300). Every use goes through this check: requests, usage polls, model discovery and T3Code `api-call`. The usage poll (every 300 s) therefore keeps idle accounts fresh.
- **After a 401:** one forced refresh, then the request is retried on the same account. The refresh is skipped if the token has already changed since it was rejected, so a burst of 401s causes one refresh.
- **Serialized per account:** refresh tokens are single-use, so each account's credential sits behind its own async mutex. Two concurrent requests can't spend the same refresh token. This only holds within one process ([#6](https://github.com/wyrd-company/xolotl/issues/6)).
- **Failures:**
  - A dead grant (`invalid_grant`, `refresh_token_reused`, …) marks the account "re-login needed" and takes it out of rotation.
  - Anything else is transient: the current token keeps being used while still valid, and the refresh is retried on next use.
- **Codex refresh responses** without email, plan or account id keep the previous values, so a paid account isn't reset to "free".

### When the vault write fails

A refreshed credential is written back to OpenBao immediately. If that write fails, the old refresh token in OpenBao has already been spent, so the in-memory copy is the only valid one. Xolotl:

- keeps serving from memory;
- marks the account unsaved: `UNSAVED` in `xolotl accounts list`, `unsaved: true` in the admin API, the account's last error, and the `xolotl.vault.unsaved_credentials` gauge;
- retries the write every 15 s, and once more on graceful shutdown (logging an error if any remain);
- never lets the periodic account sync replace an in-memory credential with the vault copy.

`/readyz` doesn't fail on unsaved credentials: an orchestrator restart is exactly what would lose them. A crash during an OpenBao outage still loses the token ([#4](https://github.com/wyrd-company/xolotl/issues/4)).

## Selecting an account

`Subscriptions::select` builds the candidates from enabled accounts that are loaded, not marked "re-login needed", not cooling down, and not already tried for this request. Then:

1. **Session affinity:** if the request carries a session id (Codex `session_id`, Claude Code session id, OpenCode session header) and that session used an account in the last `session_affinity_secs` (3600), that account is reused. This keeps prompt caches warm and avoids replaying encrypted reasoning to another account. The exception is an account above the soft limit.
2. **Tier:** accounts below `soft_limit_percent` (95%) of every usage window first, then by highest `priority`.
3. **Strategy** within the best tier: `round-robin` (default), `fill-first` or `least-used`.

## Failover

`Subscriptions::execute` tries up to `max_attempts` (3) accounts. Each provider classifies an upstream failure as one of four kinds:

| Class | Examples | Effect |
|---|---|---|
| Exhausted | Codex `usage_limit_reached`; Claude 429 with a rejected 5h/7d unified limit; OpenCode Go 429 with `Retry-After` over 5 min | Cooldown until the reset time (or `default_cooldown_secs`); next account |
| Auth | 401, revoked OAuth | One forced refresh; then "re-login needed" and next account |
| Transient | 5xx, 529, overloaded, short 429 | Short cooldown on 429 with `Retry-After` (max 300 s); next account |
| Fatal | 400, context length exceeded, Claude overage-only limits | Returned to the client: another account would fail the same way |

Codex can report a quota error as the first events of a `200` SSE stream. Xolotl reads the start of the stream before sending anything to the client, so it can still fail over. When every account is cooling down, the client gets a 429 with `Retry-After` set to the earliest reset.

## Provider specifics

- **Codex:** requests go to the ChatGPT backend (`chatgpt.com/backend-api/codex/responses`) with `chatgpt-account-id`. The backend is stateless and streaming-only, so xolotl:
  - forces `store: false` and `stream: true` (except `/compact`);
  - removes `previous_response_id` and parameters the backend rejects;
  - wraps a plain-string `input` in a message.
- **Claude:** requests go to `api.anthropic.com` with the OAuth bearer token.
  - The OAuth and Claude Code betas are merged into `anthropic-beta`, and a `betas` body field is moved into the header.
  - Requests that don't come from Claude Code get Claude Code's identifying headers.
- **OpenCode Go:** the API key is sent as Bearer (plus `x-api-key` on Messages). Any path is accepted, which is what the `/opencode-go/<path>` passthrough relies on.

## Usage

Usage windows (percent used, reset time) come from two sources:
- **Response headers:** Codex `x-codex-*`, Claude `anthropic-ratelimit-unified-*`.
- **Polling** each account every `usage_poll_secs`: Codex `/wham/usage`, Claude `/api/oauth/usage`, OpenCode Go `/v1/usage`.

They are stored per account in SQLite, shown by `xolotl accounts list` / `usage`, exported as `xolotl.account.usage_percent`, and used by selection (soft limit, least-used).

## T3Code hubs

`src/server/management.rs` implements CLIProxyAPI's management API for keys with the `management` scope:

- **`GET /v0/management/auth-files`** lists Codex and Claude accounts in CLIProxyAPI's shape. `auth_index` is a stable hash of the account id.
- **`POST /v0/management/api-call`** makes a request with the account's token substituted for `$TOKEN$`. It refreshes and retries once on a 401. The URL's scheme, host and port must match the account's own provider endpoint, so a management key can't send tokens elsewhere.
- **`POST /v0/management/reset-quota`** clears the account's cooldown and re-polls its usage.
