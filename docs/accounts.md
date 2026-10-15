---
title: Subscription accounts
order: 4
docs: true
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

| State | Meaning | What to do |
|---|---|---|
| `ok` | Available | Nothing |
| `cool→<time>` | Cooling down until that time, usually because the quota is used up | Wait, or `xolotl accounts reset <id>` |
| `re-login` | The provider refused the refresh token. The account is out of rotation | Log in to the account again |
| `UNSAVED` | A refreshed token could not be written to OpenBao. Xolotl serves from memory and retries every 15 seconds | Fix OpenBao. Do not restart xolotl until the state clears, or the token is lost |
| `disabled` | You disabled it | `xolotl accounts enable <id>` |

`xolotl accounts usage <id>` fetches the usage windows from the provider now.

## Manage accounts

```sh
xolotl accounts priority <id> 10   # higher priority is used first
xolotl accounts disable <id>
xolotl accounts enable <id>
xolotl accounts reset <id>         # clear the cooldown and the re-login state
xolotl accounts remove <id>
```

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

When every account is cooling down, the client gets a 429 with `Retry-After`
set to the earliest reset.

## Token refresh

Xolotl refreshes OAuth tokens before they expire (`refresh_margin_secs`) and
once after a 401. The usage poll, every 5 minutes, also keeps idle accounts
fresh. Refreshed tokens are written back to OpenBao at once.

Run only one xolotl instance against the same accounts. Refreshes are
serialized inside one process only.

## Usage windows

Xolotl reads each account's usage windows from response headers and polls them
every `usage_poll_secs`. It uses them for the soft limit and `least-used`, and
shows them in `xolotl accounts list`, the web UI and the
`xolotl.account.usage_percent` metric. Grok accounts have no usage windows.

## T3 Code hubs

T3 Code can show your accounts' usage. Add a CLIProxyAPI usage source with the
URL of xolotl and a client key whose policy has the `management` scope. T3 Code
shows the Codex and Claude accounts. See [Clients](clients#t3-code).
