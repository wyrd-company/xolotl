---
title: Policies and client keys
order: 5
docs: true
---

Every client authenticates with its own client key (`xol_...`). Every key
belongs to a policy. The policy decides what the key can do.

## Write a policy

A policy is a TOML or JSON file:

```toml
name = "devcontainer"
description = "Coding agents in devcontainers"

providers = ["claude", "codex", "grok", "opencode-go", "vercel"]
deny_models = ["*-preview"]
precedence = ["claude", "codex", "grok", "opencode-go", "vercel"]

[winners]
"kimi-k3" = "vercel"

[renames]
"opencode-go/claude-haiku-5-5" = "haiku-go"

[aliases]
"fast" = ["codex/gpt-6-codex-mini", "vercel/openai/gpt-6-mini"]

[limits]
requests_per_minute = 60
tokens_per_day = 5000000
```

```sh
xolotl policies put devcontainer.toml
xolotl policies list
xolotl policies get devcontainer
xolotl policies delete devcontainer   # refused while keys use it
```

The web UI's policy editor has the same fields. Its model list shows every
model of the allowed providers, with allow and deny toggles, inline renames,
extra names with fallbacks, the winner of each shared name, and the state of
each provider's accounts. The toggles write exact `provider/model` entries to
`allow_models` and `deny_models`. The globs stay editable under
**Model patterns**.

| Field | Meaning |
|---|---|
| `name` | Lowercase letters, digits, `-` and `_`, at most 63 characters |
| `description` | Free text |
| `providers` | Providers the key can use: `claude`, `codex`, `grok`, `opencode-go`, gateway provider names, or `*` for all |
| `allow_models` | Globs on `model` or `provider/model`. Empty allows every model of the allowed providers |
| `deny_models` | Globs that remove models. Deny wins over allow |
| `precedence` | Provider order for a model name that several providers serve |
| `winners` | Per-model provider choice, before `precedence` |
| `renames` | `provider/model` → the name keys see instead of its plain name |
| `aliases` | Extra names. Each maps to one `provider/model`, or to a list tried in order |
| `scopes` | What the key can do. Default `["proxy"]` |
| `mcp_servers` | MCP servers the key can use through `/mcp`, or `*` |
| `allow_tools`, `deny_tools` | Globs on `tool` or `server/tool`. Deny wins. See [MCP gateway](mcp-gateway#policies) |
| `limits` | Request and token limits on the LLM endpoints. See [Rate limits](#rate-limits) |

### Scopes

| Scope | Allows |
|---|---|
| `proxy` | The LLM endpoints (`/v1/...`) and the passthroughs |
| `mcp` | The MCP gateway at `/mcp` |
| `management` | The T3 Code hubs endpoints (`/v0/management/...`) |
| `admin` | The admin API, the same as the bootstrap admin token |

A policy that allows MCP servers needs no `providers`.

## Model names

A client sends a model name. The policy turns it into one or more targets
(`provider/model`):

1. Xolotl keeps each model in the catalog whose provider is allowed and that
   passes `allow_models` and `deny_models`.
2. When several providers serve the same name, one provider wins the plain
   name: the one in `winners`, else the first in `precedence`, else the first
   in the default order `claude`, `codex`, `grok`, `opencode-go`, else
   alphabetical. The other providers do not get the plain name.
3. Renames are added. A renamed model does not take part in step 2, so its
   plain name goes to the next provider that serves it. A rename does not
   allow a model: a denied model stays hidden.
4. Aliases are added. An alias or a rename with the same spelling as a plain
   name replaces it. An alias and a rename can't have the same name, and two
   models can't be renamed to the same name.

The `provider/model` form always works for a model the policy allows, also for
a model that lost its plain name.

An alias with a list is a fallback chain: on a 429 or 5xx, xolotl tries the next
target.

### Example

The catalog has `claude-haiku-5-5` on `claude` and on `opencode-go`. With the
default order, a key sees `claude-haiku-5-5` on `claude`. The OpenCode Go copy
is still `opencode-go/claude-haiku-5-5`, or `haiku-go` with the rename above.
`fast` tries a Codex model first and a Vercel model when Codex answers with a
429 or 5xx.

### See what a key sees

```sh
xolotl policies view devcontainer
```

This lists every name, where it routes, and the endpoints it is served on,
natively and through translation. With MCP servers, it also lists the tools a
key can call. A client gets the same names from `GET /v1/models`.

## Rate limits

A policy can limit how much its keys use the LLM endpoints:

```toml
[limits]
per = "key"                  # or "policy": all keys of the policy share one allowance
requests_per_minute = 60
requests_per_day = 5000
tokens_per_minute = 200000
tokens_per_day = 5000000
```

Every field is optional. A missing limit is no limit. A limit must be at least
1; to block a key, disable it.

| Limit | Counts |
|---|---|
| `requests_per_minute`, `requests_per_day` | Requests to `/v1/...`, `/backend-api/codex/...`, the passthroughs and `/opencode-go/...`. An alias that falls back to its next target counts once |
| `tokens_per_minute`, `tokens_per_day` | Input plus output tokens, as the upstream reports them. Input includes cache reads and writes |

- **Windows** slide: a minute is the last 60 seconds, a day the last 24 hours.
- **Per key or per policy.** With `per = "key"` (default) each key has its own
  allowance. With `per = "policy"` all keys of the policy share one.
- **Tokens are counted when the response finishes.** A token limit is checked
  before each request, so the request that goes over the limit still
  completes. The next one is refused.
- **Refused requests do not count**, so a client that retries gets through as
  soon as the window allows.
- **Not limited:** `GET /v1/models`, `/mcp`, the management and admin APIs, and
  the bootstrap admin token.
- **No spend limits.** Xolotl counts requests and tokens, not money.

A request over a limit gets a 429 with `Retry-After`, the seconds until the
limit admits a request again. The error names the limit, and says whether it
counts the key or the policy.

To count tokens in a streamed Chat Completions request, xolotl asks the
upstream for its usage chunk (`stream_options.include_usage`). The client then
gets one extra chunk with empty `choices`, also if it did not ask for it.

Counters are kept in memory and rebuilt from the request log of the last
24 hours when xolotl starts. Each instance counts only its own requests.

`xolotl keys list` shows each key's consumption per window, and the web UI
shows it on the **Policies** and **Client keys** screens. The metrics are
`xolotl.limit.consumption` and `xolotl.limit.rejections` (see
[Telemetry](telemetry)).

## Issue a client key

```sh
xolotl keys issue --policy devcontainer --name laptop --ttl 90d \
  --description "Laptop devcontainer"
```

The key is written to OpenBao at `keys/<policy>/<name>`, field `api_key`. It is
never printed. The client reads it with its own OpenBao policy, scoped to that
one path:

```hcl
path "secret/data/xolotl/keys/devcontainer/laptop" { capabilities = ["read"] }
```

```sh
export XOLOTL_API_KEY=$(bao kv get -field=api_key secret/xolotl/keys/devcontainer/laptop)
```

The web UI's **Client keys** screen shows the path, the `bao` command and the
OpenBao policy for each key.

A key issued on one instance works on another at once: an unknown key makes
xolotl reload keys from OpenBao, at most every 10 seconds.

## Manage keys

```sh
xolotl keys list
xolotl keys rotate devcontainer/laptop --grace 1h   # the old key works for 1 more hour
xolotl keys disable devcontainer/laptop
xolotl keys enable devcontainer/laptop
xolotl keys revoke devcontainer/laptop
```

- **Rotate** writes a new key to the same path. Without `--grace`, the old key
  stops at once.
- **Revoke** writes a revocation marker and destroys every earlier version of
  the record, so nobody can recover the key from history. You can then issue a
  new key with the same name.
- **Issue** refuses a name that has a live key. Rotate it instead.

The `USAGE` column of `xolotl keys list` shows requests per minute, requests per
day, tokens per minute and tokens per day. A window the policy limits shows
`used/limit`, for example `7/60`. With `per = "policy"`, the limited windows
show the total of all keys of the policy, and the column ends with `(policy)`.

Durations are numbers with `s`, `m`, `h`, `d` or `w`, for example `30m` or `90d`.

## Usage by client

```sh
xolotl usage --group-by client --since 7d
```

Group by `client`, `account`, `provider` or `model`. The table shows requests,
errors, and input, output and cached tokens.

Narrow the rows with `--client`, `--account`, `--provider` or `--model`:

```sh
xolotl usage --group-by model --client devcontainer/laptop
xolotl usage --group-by client --account ""   # gateway requests (no account)
```

An empty value matches rows that have no value there: `--client ""` is the
bootstrap admin token, `--account ""` gateway requests, `--model ""` requests
without a model.

On the web UI's **Usage** screen, select a row to narrow the view to it. The
view then groups by the next of model, account, client and provider that is not
shown or narrowed yet. Each filter shows as a chip that you can remove, and the
URL keeps the filters.
