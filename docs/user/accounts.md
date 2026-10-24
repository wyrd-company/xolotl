---
title: Subscription accounts
order: 4
product: xolotl
---

A subscription account is a login to a consumer subscription or an API-key plan.
Xolotl holds several accounts per provider, as a pool, and sends each request to
one of them.

| Provider | Account | How to add it |
|---|---|---|
| `codex` | ChatGPT subscription (Codex) | `xolotl auth login codex`, or import `~/.codex/auth.json` |
| `claude` | Claude subscription (Claude Code) | `xolotl auth login claude`, or import `~/.claude/.credentials.json` |
| `grok` | Grok Build (xAI) subscription | `xolotl auth login grok` |
| `opencode-go` | OpenCode Go API key | `xolotl accounts add-key` |

You can also add accounts on the web UI's **Add account** screen.

## Add an account

### Codex

```sh
xolotl auth login codex
```

The CLI prints a URL and a code. Open the URL, enter the code and approve.
Xolotl waits for the approval and saves the account.

Add `--browser` to use the browser flow instead: open the URL, approve, then
paste the URL the browser ends on back into the CLI.

### Claude

```sh
xolotl auth login claude
```

Open the URL on claude.ai and approve. The page shows a code in the form
`CODE#STATE`. Paste it into the CLI.

Claude subscription accounts serve only the Claude Code API path,
`/v1/messages`. Xolotl does not translate requests to them from other formats.

### Grok Build

```sh
xolotl auth login grok
```

Open the URL, enter the code and approve. You can't import Grok logins.

### OpenCode Go

```sh
echo "$OPENCODE_GO_KEY" | xolotl accounts add-key --label personal
```

If you leave out the pipe, the CLI asks for the key without echo.

Every request to OpenCode Go carries a session ID in `x-opencode-session`.
Xolotl takes it from the client's `x-opencode-session`, `x-claude-code-session-id`
or `session_id` header. Without one, it uses a fixed ID derived from the client
key, so all clients that share a key share one session. Send a session header
to keep conversations apart. The same ID keeps the session on one account.

The upstream user agent is `xolotl/<version> (<client user agent>)`.

### Import a CLI login

```sh
xolotl accounts import codex ~/.codex/auth.json
xolotl accounts import claude ~/.claude/.credentials.json --label work
```

> **Stop using that login on the local CLI after you import it.** Refresh
> tokens are single-use. When xolotl refreshes the token, the copy on your
> machine stops working, and if the local CLI refreshes first, the account in
> xolotl stops working.

Logging in to the same account again updates it. It does not add a duplicate.

## See your accounts

```sh
xolotl accounts list
xolotl accounts list --provider codex
```

The list shows each account's ID, provider, label, plan, priority, state and
usage windows (for example `5h 12%, 7d 40%`).

| CLI state | Web UI state | Meaning | What to do |
|---|---|---|---|
| `ok` | Available | The account can take requests | Nothing |
| `cool→<time>` | Cooling down | Cooling down until that time, usually because the quota is used up. The reason shows beside the state | Wait, or **Retry** (`xolotl accounts reset <id>`) |
| `re-login` | Re-login needed | An OAuth account whose refresh token the provider refused. It is out of rotation | Log in to the account again |
| `re-login` | Key rejected | An API-key account whose key the provider refused with a 401. It is out of rotation | Replace the key |
| `re-login` | Refused | An API-key account whose last recorded error is another 4xx | Read the last error. Replace the key, or **Retry** |
| `UNSAVED` | | A refreshed token could not be written to OpenBao. Xolotl serves from memory and retries every 15 seconds | Fix OpenBao. Do not restart xolotl until the state clears, or the token is lost |
| `disabled` | Disabled | You disabled it | `xolotl accounts enable <id>` |

`xolotl accounts usage <id>` fetches the usage windows from the provider now.

## Manage accounts

```sh
xolotl accounts priority <id> 10   # higher priority is used first
xolotl accounts disable <id>
xolotl accounts enable <id>
xolotl accounts reset <id>         # clear the cooldown and the re-login state
xolotl accounts remove <id>
```

## Recover an account

The web UI's overview lists accounts that need attention. Each entry has a
**Review account** link that opens the account's details. There:

- **OAuth accounts** (Codex, Claude, Grok) in **Re-login needed** have an
  inline login. Log in again to the same account; xolotl updates it in place.
- **API-key accounts** (OpenCode Go) show the last error and a
  **Replace key** form. The account keeps its ID, label, priority, enabled
  state and usage history.
- **Retry** clears the cooldown and the re-login state, so xolotl tries the
  account again with the same credential. It does not change the credential
  and does not reset the provider's quota. A disabled account stays disabled.
  `xolotl accounts reset <id>` does the same.

Adding an OpenCode Go key that another account already has updates that
account instead of adding a second one. A replacement key that another account
already has is refused.

Adding a key, replacing a key and **Retry** also refresh the provider's model
list. **Refresh models**, next to **Refresh usage**, does it on demand and
shows the model count or the discovery error. A discovery error keeps the
previous model list, and does not undo the saved key or the retry.

## How xolotl picks an account

For each request, xolotl uses the enabled accounts that are not cooling down and
do not need a new login. Then:

1. **Session affinity.** A request with a session ID (Codex, Claude Code and
   Grok send one) goes to the account the session used before, for up to
   `session_affinity_secs` (one hour). This keeps prompt caches warm. It
   applies only while that account is below `soft_limit_percent`.
2. **Soft limit.** Accounts below `soft_limit_percent` (95%) in every usage
   window come first.
3. **Priority.** Higher priority first.
4. **Strategy.** Among equal accounts: `round-robin` (default), `fill-first`
   (use one account until it is limited) or `least-used`. `least-used` takes
   each account's highest usage across all its windows and picks the account
   where that is lowest. For example, an account at 10% in its 5-hour window
   and 99% in its 7-day window loses to an account at 40% in both.

Set these in [`[subscription]`](configuration#subscription).

## When an account fails

Xolotl tries up to `max_attempts` (3) accounts for one request.

| Upstream answer | What xolotl does |
|---|---|
| Quota used up (for example Codex `usage_limit_reached`, a Claude 5-hour or 7-day limit) | Cools the account down until its reset time and tries the next account |
| 401 or revoked login | Refreshes the token once and retries. If that fails, marks the account `re-login` and tries the next one |
| 5xx, overloaded, short 429 | Tries the next account |
| 400, context length exceeded | Returns the error to the client, because another account fails the same way |
| OpenCode Go 403 | Returns the error to the client. The account stays available, because a 403 refuses the request, not the key |

When every account is cooling down, the client gets a 429 with `Retry-After`
set to the earliest reset.

The account's last error shows why the upstream refused, with credentials
removed. A successful request clears it.

## Token refresh

Xolotl refreshes OAuth tokens before they expire (`refresh_margin_secs`) and
once after a 401. The usage poll, every 5 minutes, also keeps idle accounts
fresh. Refreshed tokens are written back to OpenBao at once.

When a refresh fails:

- **The provider refuses the refresh token** (for example `invalid_grant` or
  `refresh_token_reused`): the account goes to `re-login`. Log in again. A
  `refresh_token_reused` error usually means that another client, such as a
  local CLI with the same login, used the token first.
- **Other failures**: the account keeps its current access token while it
  works, and xolotl tries again later. The wait doubles after each failure,
  from 5 seconds to at most 5 minutes. A successful refresh or a new login
  ends the wait.
- **Unknown outcome**: the request was sent but no usable answer came back, so
  the provider may have used the refresh token. Xolotl logs an error, sets the
  account's last error, and counts `xolotl.oauth.refreshes` with
  `result=uncertain`. If the provider then refuses the refresh token, log in
  again.

A Claude import with a refresh token but no `expiresAt` refreshes on first use.

Run only one xolotl instance against the same accounts. Refreshes are
serialized inside one process only.

## Usage windows

Xolotl reads each account's usage windows from response headers and polls them
every `usage_poll_secs`. It uses them for the soft limit and `least-used`, and
shows them in `xolotl accounts list`, the web UI and the
`xolotl.account.usage_percent` metric. Grok accounts have no usage windows.

Claude's weekly model-family windows, including Fable, are shown separately
from the account-wide limits. The internal `iguana_necktie` bucket is hidden.
Exhausting a model-family window does not put the whole account on cooldown.

### Available resets

Each Codex usage poll also counts the account's available reset credits.
Claude usage polls request the available reset grants and count those that
are usable, unpaused and unexpired, when the provider identifies a next grant.

The web UI's overview shows a total for each Claude and Codex pool. The accounts
list and account details show each account's count. An unknown count is shown as
unknown, not zero; pool totals are marked partial when some accounts are unknown.
Counts come from the last poll, so a reset can expire or be used elsewhere in the
meantime. Claude accounts without an eligible reset offer show an unknown count.

Xolotl does not redeem credits itself. T3 Code can redeem a Codex credit (see
[T3 Code hubs](#t3-code-hubs)).

## Models

After you add, import, reset, enable, disable or remove an account, xolotl asks
the provider for its model list again. `xolotl models --refresh` and the web
UI's **Refresh models** control do it at once. See
[Providers and models](web-ui#models).

## T3 Code hubs

T3 Code can show your accounts' usage. Add a CLIProxyAPI usage source with the
URL of xolotl and a client key whose policy has the `management` scope. T3 Code
shows the Codex and Claude accounts, and the Codex reset credits.

To redeem a Codex reset credit, T3 Code sends the redemption request to Codex
through xolotl, which adds the account's token. Then T3 Code asks xolotl to
reset the account's quota: xolotl clears the account's cooldown and polls its
usage again. See [Clients](clients#t3-code).
