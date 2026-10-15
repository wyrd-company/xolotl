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

[aliases]
"haiku-go" = "opencode-go/claude-haiku-5-5"
"fast" = ["codex/gpt-6-codex-mini", "vercel/openai/gpt-6-mini"]
```

```sh
xolotl policies put devcontainer.toml
xolotl policies list
xolotl policies get devcontainer
xolotl policies delete devcontainer   # refused while keys use it
```

The web UI's policy editor has the same fields and a live preview.

| Field | Meaning |
|---|---|
| `name` | Lowercase letters, digits, `-` and `_`, at most 63 characters |
| `description` | Free text |
| `providers` | Providers the key can use: `claude`, `codex`, `grok`, `opencode-go`, gateway provider names, or `*` for all |
| `allow_models` | Globs on `model` or `provider/model`. Empty allows every model of the allowed providers |
| `deny_models` | Globs that remove models. Deny wins over allow |
| `precedence` | Provider order for a model name that several providers serve |
| `winners` | Per-model provider choice, before `precedence` |
| `aliases` | Extra names. Each maps to one `provider/model`, or to a list tried in order |
| `scopes` | What the key can do. Default `["proxy"]` |
| `mcp_servers` | MCP servers the key can use through `/mcp`, or `*` |
| `allow_tools`, `deny_tools` | Globs on `tool` or `server/tool`. Deny wins. See [MCP gateway](mcp-gateway#policies) |

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
3. Aliases are added. An alias with the same spelling as a plain name replaces
   it.

The `provider/model` form always works for a model the policy allows, also for
a model that lost its plain name.

An alias with a list is a fallback chain: on a 429 or 5xx, xolotl tries the next
target.

### Example

The catalog has `claude-haiku-5-5` on `claude` and on `opencode-go`. With the
default order, a key sees `claude-haiku-5-5` on `claude`. The OpenCode Go copy
is still `opencode-go/claude-haiku-5-5`, or `haiku-go` with the alias above.

### See what a key sees

```sh
xolotl policies view devcontainer
```

This lists every name, where it routes, and the endpoints it is served on,
natively and through translation. With MCP servers, it also lists the tools a
key can call. A client gets the same names from `GET /v1/models`.

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

Durations are numbers with `s`, `m`, `h`, `d` or `w`, for example `30m` or `90d`.

## Usage by client

```sh
xolotl usage --group-by client --since 7d
```

Group by `client`, `account`, `provider` or `model`. The table shows requests,
errors, and input, output and cached tokens.
