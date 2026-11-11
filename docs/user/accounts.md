---
title: Accounts
order: 4
product: xolotl
---

An account is a credential that xolotl uses to send requests upstream. Each
account has a kind. The kind decides where the account serves requests and how
you add it:

- **Pool accounts** are logins to a consumer subscription or an API-key plan.
  Xolotl holds several accounts per provider, as a pool, and sends each request
  to one of them.
- **Provider accounts** are API-key upstreams, such as the Anthropic and
  OpenAI APIs, Vercel AI Gateway, Mistral or a llama.cpp server. You give each one a name, and clients address
  it by that name: `<name>/<model>` and `/<name>/...`. A provider account is a
  provider of its own.

| Kind | Placement | Account | Web UI (**Add account**) | CLI |
|---|---|---|---|---|
| `codex` | pool `codex` | ChatGPT subscription (Codex) | **Codex (ChatGPT)**, or **Import a login** | `xolotl auth login codex` |
| `claude` | pool `claude` | Claude subscription (Claude Code) | **Claude (Pro / Max)**, or **Import a login** | `xolotl auth login claude` |
| `grok` | pool `grok` | Grok Build (xAI) subscription | **Grok Build (xAI)** | `xolotl auth login grok` |
| `opencode-go` | pool `opencode-go` | OpenCode Go API key | **OpenCode Go** | `xolotl accounts add-key` |
| `anthropic` | provider | Anthropic API key: personal or service account | **Provider account**, kind **Anthropic API** | `xolotl accounts add-provider <name> --kind anthropic` |
| `openai` | provider | OpenAI API key: project or service account | **Provider account**, kind **OpenAI API** | `xolotl accounts add-provider <name> --kind openai` |
| `vercel` | provider | Vercel AI Gateway API key | **Provider account**, kind **Vercel AI Gateway** | `xolotl accounts add-provider <name> --kind vercel` |
| `mistral` | provider | Mistral API key | **Provider account**, kind **Mistral** | `xolotl accounts add-provider <name> --kind mistral` |
| `openai-compatible` | provider | Any OpenAI-compatible server, such as llama.cpp | **Provider account**, kind **OpenAI-compatible** | `xolotl accounts add-provider <name> --kind openai-compatible --base-url <url>` |

Credentials go straight to OpenBao. Logging in to an account that is already
in xolotl updates it. It does not add a duplicate. Provider accounts can also
come from the config file (see
[`[gateway.providers.<name>]`](configuration.md#gatewayprovidersname)).

## Add an account

Open **Accounts** in the [web UI](web-ui.md) and select **Add account**. The
screen has one section per provider.

### Codex

1. Under **Codex (ChatGPT)**, select **Start device login**.
2. Open the sign-in page it links, sign in to ChatGPT and enter the code it
   shows. You can do this on any device.
3. The screen updates when you finish. The account is saved.

**Use browser sign-in instead** starts the browser flow: approve, then paste
the `http://localhost:1455/…` URL the browser ends on into the form, and select
**Finish login**.

With the CLI:

```sh
xolotl auth login codex             # device code
xolotl auth login codex --browser   # browser flow: paste the URL the browser ends on
```

### Claude

1. Under **Claude (Pro / Max)**, select **Start Claude login**.
2. Select **Open claude.ai authorization** and approve.
3. claude.ai shows a code in the form `CODE#STATE`. Paste it into
   **Authorization code** and select **Finish login**.

With the CLI, `xolotl auth login claude` prints the URL and asks for the code.

Claude subscription accounts serve only the Claude Code API path,
`/v1/messages`. Xolotl does not translate requests to them from other formats.

### Grok Build

1. Under **Grok Build (xAI)**, select **Start device login**.
2. Open the page it links, sign in to your xAI account and confirm the code.
3. The screen updates when you finish.

With the CLI: `xolotl auth login grok`. You can't import Grok logins.

### OpenCode Go

1. Under **OpenCode Go**, enter a **Label** (optional) and the **API key**
   from the OpenCode console.
2. Select **Add key**.

Add several keys to spread load and limits.

With the CLI:

```sh
echo "$OPENCODE_GO_KEY" | xolotl accounts add-key --label personal
```

If you leave out the pipe, the CLI asks for the key without echo.

#### Service-account keys

You can add an OpenCode
[service-account key](https://opencode.ai/docs/console/api) the same way as
a personal key: under **OpenCode Go**, or with `xolotl accounts add-key`. Both
are static `oc_sk_...` bearer keys, and xolotl uses them the same way.

- A key can have an expiration. Xolotl can't renew an expired key. Create a new
  key in the OpenCode console and enter it in **Replace key** on the account's
  details page in the web UI, so that the account keeps its history.
- OpenCode says that
  [service accounts are not covered by Go Teams](https://opencode.ai/docs/console/go).
  Check which plan pays for a service account's usage before you rely on it.
- Xolotl has not been tested with a real service-account key.

Every request to OpenCode Go carries a session ID in `x-opencode-session`.
Xolotl takes it from the client's `x-opencode-session`, `x-claude-code-session-id`
or `session_id` header. Without one, it uses a fixed ID derived from the client
key, so all clients that share a key share one session. Send a session header
to keep conversations apart. The same ID keeps the session on one account.

The upstream user agent is `xolotl/<version> (<client user agent>)`.

### Provider accounts

```sh
echo "$MISTRAL_KEY" | xolotl accounts add-provider mistral-team --kind mistral
xolotl accounts add-provider llama --kind openai-compatible \
  --base-url http://llama.example.lan:8080 --no-auth
```

| Option | Meaning |
|---|---|
| `--kind` | `anthropic`, `openai`, `vercel`, `mistral` or `openai-compatible` |
| `--base-url` | Required for `openai-compatible`. Overrides the default URL of the others |
| `--no-auth` | The server needs no key. Without it, the CLI reads the key from stdin |
| `--header Name=value` | An extra static header sent upstream. Repeat it for more |

The name is a model prefix and a path prefix, so the
[same rules](configuration.md#gatewayprovidersname) as for the config file apply.
A name that the config file uses is refused. Xolotl stores the account, with
its key and headers, in OpenBao at `providers/<name>`.

If a key is already stored at `providers/<name>`, for example because you moved
the provider out of the config file, adding the account without a key keeps
that key.

```sh
echo "$NEW_KEY" | xolotl providers set-key mistral-team   # replace the key
xolotl accounts remove-provider mistral-team              # remove the account and its key
```

A provider account that a policy names in `providers`, `precedence` or
`winners` can't be removed: change the policy first. You change and remove
config-file providers in the config file.

### Anthropic and OpenAI API accounts

These accounts use pay-as-you-go API keys from the Claude Console or the OpenAI
Platform, not a subscription. They serve the same endpoints as the subscription
accounts, so a client can use either one:

| Kind | Served natively | Served through translation |
|---|---|---|
| `anthropic` | `/v1/messages`, `/v1/messages/count_tokens` | `/v1/responses`, `/v1/chat/completions` |
| `openai` | `/v1/responses`, `/v1/chat/completions` | `/v1/messages` |

Use a service account key for a shared or automated workload:

- **Anthropic:** a service account key (Claude Console, **API keys**, linked
  account set to a service account) or a personal key. A key that is not
  scoped to one workspace needs the workspace in a header:
  `--header anthropic-workspace-id=wrkspc_...`.
- **OpenAI:** a project service account key (`sk-svcacct-...`) or a project
  key. To pick the organization or project of a key that has several, add
  `--header OpenAI-Organization=org-...` or `--header OpenAI-Project=proj_...`.

```sh
echo "$ANTHROPIC_KEY" | xolotl accounts add-provider anthropic-team --kind anthropic
echo "$OPENAI_KEY" | xolotl accounts add-provider openai-team --kind openai \
  --header OpenAI-Project=proj_abc123
```

On the web UI, pick the kind **Anthropic API** or **OpenAI API** under
**Provider account**, and put the headers in **Extra headers**.

Xolotl sends the Anthropic key in `x-api-key`, adds `anthropic-version` when the
client sends none, and drops the Claude subscription's OAuth beta flag, so a
Claude Code request works without changes. Request bodies go upstream as the
client sent them, so prompt cache breakpoints (`cache_control`) and OpenAI's
`prompt_cache_key` keep working.

A model that a subscription and an API account both serve is named by the
subscription by default (see [precedence](policies-and-keys.md)). The API
account's copy stays reachable as `<name>/<model>`. To use the API account when
the subscription is at its limit, alias the model to both:

```toml
[aliases]
"claude-opus-5-5" = ["claude/claude-opus-5-5", "anthropic-team/claude-opus-5-5"]
```

A 429 or 5xx from the subscription moves the request to the API account. An API
account bills per token: allow it only in the policies that should spend money.

#### Workload identity federation

An Anthropic or OpenAI account in the config file can authenticate as a service
account with workload identity federation instead of a stored key. Xolotl reads
a JWT from your identity provider (for example a Kubernetes projected service
account token) from a file and exchanges it for a short-lived access token. It
exchanges again before the token expires. See
[`workload_identity`](configuration.md#workload-identity).

Set up the identity provider and the service account first:

- **Anthropic:** a federation issuer, a federation rule and a service account in
  the Claude Console. See Anthropic's Workload Identity Federation guide.
- **OpenAI:** a Workload Identity Provider and a service account mapping in the
  OpenAI Platform's organization security settings. See OpenAI's workload
  identity federation guide. X.509 certificate federation is not supported.

### Import a CLI login

You can move a login from the Codex CLI (`~/.codex/auth.json`) or Claude Code
(`~/.claude/.credentials.json`) into xolotl.

1. Under **Import a login**, pick the **Provider**.
2. Choose the **Credential file** and select **Import**.

With the CLI:

```sh
xolotl accounts import codex ~/.codex/auth.json
xolotl accounts import claude ~/.claude/.credentials.json --label work
```

> **Stop using that login on the local CLI after you import it.** Refresh
> tokens are single-use. When xolotl refreshes the token, the copy on your
> machine stops working, and if the local CLI refreshes first, the account in
> xolotl stops working.

## See your accounts

The **Accounts** screen lists the accounts by provider, with their plan, state,
usage windows, priority and last use. **Refresh usage** fetches the usage
windows from the provider now. The details button at the end of a row shows
the label, cooldown reason, last error, token expiry, reset credits and the
date it was added.

With the CLI:

```sh
xolotl accounts list
xolotl accounts list --provider codex
xolotl accounts usage <id>    # fetch the usage windows now
```

The CLI list shows each pool account's ID, provider, label, plan, priority,
state and usage windows (for example `5h 12%, 7d 40%`). Without `--provider`, a
second table shows the provider accounts: name, kind, where each is defined
(`config` or `vault`) and whether its key is stored.

| CLI state | Web UI state | Meaning | What to do |
|---|---|---|---|
| `ok` | Available | The account can take requests | Nothing |
| `cool→<time>` | Cooling down | Cooling down until that time, usually because the quota is used up. The reason shows beside the state | Wait, or **Retry** (`xolotl accounts reset <id>`) |
| `re-login` | Re-login needed | An OAuth account whose refresh token the provider refused. It is out of rotation | Log in to the account again |
| `re-login` | Key rejected | An API-key account whose key the provider refused with a 401. It is out of rotation | Replace the key |
| `re-login` | Refused | An API-key account whose last recorded error is another 4xx | Read the last error. Replace the key, or **Retry** |
| `UNSAVED` | | A refreshed token could not be written to OpenBao. Xolotl serves from memory and retries every 15 seconds | Fix OpenBao. Do not restart xolotl until the state clears, or the token is lost |
| `disabled` | Disabled | You disabled it | **Enable** (`xolotl accounts enable <id>`) |

## Manage accounts

On the **Accounts** screen:

- **Disable** and **Enable** on a row take the account out of rotation and put
  it back.
- In the account's details, set **Priority (higher is used first)**, select
  **Retry** to clear a cooldown or re-login state, or **Remove** the account.

With the CLI:

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

Set these in [`[subscription]`](configuration.md#subscription). A session's
[context lock](#context-lock) comes before all of them.

## When an account fails

Xolotl tries up to `max_attempts` (3) accounts for one request.

| Upstream answer | What xolotl does |
|---|---|
| Quota used up (for example Codex `usage_limit_reached`, a Claude 5-hour or 7-day limit) | Cools the account down until its reset time and tries the next account |
| 401 or revoked login | Refreshes the token once and retries. If that fails, marks the account `re-login` and tries the next one |
| 5xx, overloaded, short 429 | Tries the next account, except on a session's [context lock](#context-lock) |
| 400, context length exceeded | Returns the error to the client, because another account fails the same way |
| OpenCode Go 403 | Returns the error to the client. The account stays available, because a 403 refuses the request, not the key |

When every account is cooling down, the client gets a 429 with `Retry-After`
set to the earliest reset.

The account's last error shows why the upstream refused, with credentials
removed. A successful request clears it.

## Context lock

A long conversation is cheap only while the provider's prompt cache holds it, and
that cache belongs to one account and one model. So a session whose prompt
reaches `lock_tokens` (60 000 tokens) stays on the account and model that served
it, ahead of alias order, `soft_limit_percent` and the strategy above. The session
is identified from what the client sends, or else from its system prompt and
first message.

Xolotl breaks the lock, moves the request on and records why, when:

| Reason | What happened |
|---|---|
| `cooldown` | The account is cooling down |
| `account_lost` | The account was removed or disabled, needs a new login, or its credential is unavailable |
| `policy` | The key's policy no longer allows the account |
| `limit` | The account or provider is at a limit of the key's policy |
| `upstream_errors` | `max_attempts` (3) requests in a row failed on the account |
| `target_unavailable` | The model name no longer offers the locked target |

**A locked session does not fail over on a short failure.** A 5xx, a transport
error or a 429 without `Retry-After` from the locked account goes back to the
client, which sees an error it would not see without the lock. A success ends the
count. The third failure in a row (`max_attempts`) breaks the lock and fails over
in that same request, which then still has its `max_attempts` tries of other
accounts. To keep failover on every request, turn the lock off with
`lock_tokens = 0` in [`[sessions]`](configuration.md#sessions).

The lock belongs to the model name the client asked for. A request in the same
session for another name, such as a smaller model for a background task, routes
normally and leaves the lock alone. It still prefers the locked account when its
model is on the same provider.

After a break, the session locks again wherever its next success lands. See
locks, failures and breaks on the web UI's **Usage** screen, at
`GET /admin/sessions`, and in the `xolotl.session.locks` metric.

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
[Models](web-ui.md#models).

## T3 Code hubs

T3 Code can show your accounts' usage. Add a CLIProxyAPI usage source with the
base URL of xolotl, the same URL your other clients use, and a client key
whose policy has the `management` scope. T3 Code shows the Codex and Claude
accounts, and the Codex reset credits.

To redeem a Codex reset credit, T3 Code sends the redemption request to Codex
through xolotl, which adds the account's token. Then T3 Code asks xolotl to
reset the account's quota: xolotl clears the account's cooldown and polls its
usage again. See [Clients](clients.md#t3-code).
