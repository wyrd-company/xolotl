---
title: CLI reference
order: 9
product: xolotl
---

`xolotl serve` runs the server. Every other command calls a running server's
admin API. `xolotl <command> --help` shows every option.

## Connection options

These options work on every command except `serve` and `check-config`.

| Option | Environment | Default | Meaning |
|---|---|---|---|
| `--url` | `XOLOTL_URL` | `http://127.0.0.1:8317` | Server URL |
| `--token` | `XOLOTL_ADMIN_TOKEN` | none | The bootstrap admin token, or a key whose policy has the `admin` scope |
| `--json` | | off | Print raw JSON |

Commands that read a secret (`providers set-key`, `accounts add-key`,
`mcp set-secret`) read it from stdin, or ask for it without echo when stdin is
a terminal.

Durations are a number with `s`, `m`, `h`, `d` or `w`, for example `90d`.

## Server

| Command | Does |
|---|---|
| `xolotl serve [--config <file>]` | Run the server |
| `xolotl check-config [--config <file>]` | Validate a config file and print it with credentials redacted |
| `xolotl reload` | Reload policies, client keys and accounts from OpenBao now |

## Accounts

| Command | Does |
|---|---|
| `xolotl auth login <codex\|claude\|grok> [--browser]` | Log in a subscription account. `--browser`: Codex browser flow instead of the device code |
| `xolotl accounts list [--provider <p>]` | Accounts with state, plan, priority and usage windows |
| `xolotl accounts import <codex\|claude> <file> [--label <l>]` | Import a CLI credential file |
| `xolotl accounts add-key [--provider opencode-go] [--label <l>]` | Add an API-key account |
| `xolotl accounts priority <id> <n>` | Set the priority. Higher is used first |
| `xolotl accounts enable\|disable <id>` | Put the account in or out of rotation |
| `xolotl accounts reset <id>` | Clear the cooldown and the re-login state |
| `xolotl accounts usage <id>` | Fetch the usage windows from the provider now |
| `xolotl accounts remove <id>` | Remove the account |

See [Subscription accounts](accounts).

## Gateway providers and models

| Command | Does |
|---|---|
| `xolotl providers set-key <name>` | Store a gateway provider's API key in OpenBao |
| `xolotl models [--refresh]` | The models each provider serves. `--refresh` discovers them again from every provider first |

## Policies and keys

| Command | Does |
|---|---|
| `xolotl policies list` | Policies with providers and scopes |
| `xolotl policies get <name>` | One policy as JSON |
| `xolotl policies put <file>` | Create or replace a policy from a TOML or JSON file |
| `xolotl policies view <name>` | The names a key sees, where they route, their endpoints, and the MCP tools |
| `xolotl policies delete <name>` | Delete a policy that no key uses |
| `xolotl keys issue --policy <p> --name <n> [--ttl <d>] [--description <s>]` | Issue a key into OpenBao at `keys/<p>/<n>` |
| `xolotl keys list` | Keys with policy, state, rotation, expiry and rate limit consumption |
| `xolotl keys rotate <p>/<n> [--grace <d>]` | Write a new key. The old key works for the grace period |
| `xolotl keys enable\|disable <p>/<n>` | Turn a key on or off |
| `xolotl keys revoke <p>/<n>` | Revoke the key and destroy its earlier versions |

See [Policies and client keys](policies-and-keys).

## MCP gateway

| Command | Does |
|---|---|
| `xolotl mcp servers` | Upstream servers: source, state, tool count, secret fields |
| `xolotl mcp put <file>` | Add or change a server stored in OpenBao |
| `xolotl mcp remove <server>` | Remove a server stored in OpenBao, with its secrets and login |
| `xolotl mcp tools <server>` | The server's tools, without the policy filter |
| `xolotl mcp refresh [<server>]` | Re-read tool lists |
| `xolotl mcp set-secret <server> <field>` | Set a field of the server's header secret |
| `xolotl mcp unset-secret <server> <field>` | Remove a field |
| `xolotl mcp login <server>` | OAuth sign-in |
| `xolotl mcp logout <server>` | Forget the server's OAuth tokens |
| `xolotl mcp usage [--group-by server\|tool\|client] [--since 24h]` | Tool calls, errors and average latency |

See [MCP gateway](mcp-gateway).

## Usage

| Command | Does |
|---|---|
| `xolotl usage [--group-by client\|account\|provider\|model] [--since 24h] [--client <c>] [--account <a>] [--provider <p>] [--model <m>]` | Requests, errors and tokens from the request log. The filters narrow the rows; `""` matches rows without a value |

`xolotl models` shows context, maximum output, input/output and cache prices, metadata source and age. `--refresh` refreshes metadata with model discovery; `--json --metadata` returns catalog route rows with a `metadata` object. `--json` keeps the provider-to-model-name map. Costs are API-equivalent USD list prices.
