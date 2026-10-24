---
title: Configuration
order: 3
product: xolotl
---

Xolotl reads one TOML file, given with `--config` or `XOLOTL_CONFIG`. Every
section is optional. Unknown keys are refused. `config.example.toml` in the
repository shows every section with its defaults.

Check a file before you use it:

```sh
xolotl check-config --config config.toml
```

The file holds no secrets. Secrets go in OpenBao (see
[OpenBao paths](#openbao-paths)).

## Environment variables

Environment variables override the file.

| Variable | Overrides |
|---|---|
| `XOLOTL_CONFIG` | The config file path |
| `XOLOTL_LISTEN` | `server.listen` |
| `XOLOTL_DATABASE` | `database.path` |
| `BAO_ADDR` or `VAULT_ADDR` | `vault.address` |
| `BAO_NAMESPACE` or `VAULT_NAMESPACE` | `vault.namespace` |
| `BAO_CACERT` or `VAULT_CACERT` | `vault.ca_cert` |
| `XOLOTL_VAULT_ROLE_ID` | `vault.approle.role_id` |
| `XOLOTL_VAULT_SECRET_ID` | `vault.approle.secret_id` |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | `telemetry.otlp_endpoint` |
| `XOLOTL_LOG_FORMAT` | `telemetry.log_format` |

The CLI uses `XOLOTL_URL` and `XOLOTL_ADMIN_TOKEN` (see [CLI](cli)).

## `[server]`

```toml
[server]
listen = "0.0.0.0:8317"
max_body_bytes = 67108864      # 64 MiB, for OCR and batch uploads
upstream_timeout_secs = 600    # maximum idle time while reading an upstream response
# public_url = "https://xolotl.example.lan"
```

Set `public_url` to the URL where browsers reach xolotl. Then MCP OAuth logins
come back to xolotl and finish without a pasted URL.

## `[database]`

```toml
[database]
path = "xolotl.db"      # the container image uses /data/xolotl.db
retention_days = 90     # request log and MCP call log
```

The database holds cooldowns, usage snapshots, the account index, the request
log and the MCP call log. It holds no secrets. If you lose it, xolotl rebuilds
the account index from OpenBao.

## `[vault]`

```toml
[vault]
address = "http://127.0.0.1:8200"
mount = "secret"        # KV v2 mount
prefix = "xolotl"       # every secret is under secret/xolotl/...
# namespace = "team"
# ca_cert = "/etc/xolotl/bao-ca.pem"

[vault.approle]
mount = "approle"
# role_id = "..."       # prefer the environment or a file
role_id_file = "/run/secrets/xolotl_role_id"
secret_id_file = "/run/secrets/xolotl_secret_id"
```

## `[telemetry]`

```toml
[telemetry]
# otlp_endpoint = "http://openobserve:5080/api/default"
service_name = "xolotl"
log_format = "pretty"   # or "json" (the container image sets json)
log_filter = "info"     # RUST_LOG-style filter
metrics_interval_secs = 30
# dashboard_url = "https://openobserve.example.lan"   # linked from the web UI

[telemetry.otlp_headers]
# Authorization = "Basic ..."   # prefer the OpenBao secret `telemetry`
```

See [Telemetry](telemetry).

## `[ui.oidc]`

Web UI sign-in with OpenID Connect. Without this section nobody can sign in to
the web UI. The CLI and client keys still work.

```toml
[ui.oidc]
issuer = "https://id.example.lan"
client_id = "00000000-0000-0000-0000-000000000000"
redirect_url = "https://xolotl.example.lan/admin/session/callback"
admin_group = "xolotl-admins"
```

See [Web UI](web-ui).

## `[subscription]`

How xolotl uses subscription accounts. See [Subscription accounts](accounts).

```toml
[subscription]
strategy = "round-robin"   # round-robin | fill-first | least-used
usage_poll_secs = 300      # 0 disables usage polling
max_attempts = 3           # accounts tried for one request
default_cooldown_secs = 60 # after a 429 with no known reset time
refresh_margin_secs = 300  # refresh OAuth tokens this long before they expire
soft_limit_percent = 95.0  # accounts above this in any window are used last
session_affinity_secs = 3600   # 0 disables session affinity
```

## `[gateway.providers.<name>]`

Gateway providers are API-key upstreams. The name is a model prefix
(`<name>/<model>`) and a path prefix (`/<name>/...`).

```toml
[gateway.providers.vercel]
kind = "vercel"

[gateway.providers.mistral]
kind = "mistral"

[gateway.providers.llamacpp]
kind = "openai-compatible"
base_url = "http://llama.example.lan:8080"
no_auth = true
```

| Field | Meaning |
|---|---|
| `kind` | `vercel`, `mistral` or `openai-compatible` (default) |
| `base_url` | Required for `openai-compatible`; overrides the default for the others |
| `secret` | OpenBao secret with the `api_key` field. Default `providers/<name>` |
| `no_auth` | `true` for a server that needs no key |
| `headers` | Extra static headers sent upstream |
| `enabled` | `false` turns the provider off |

Without a `[gateway]` section, xolotl configures `vercel` and `mistral`. When
you add any provider, list every provider you want.

Names are letters, digits, `-` and `_`. These names are refused:
`claude`, `codex`, `grok`, `opencode-go` (subscription accounts), and `v0`,
`v1`, `admin`, `ui`, `assets`, `backend-api`, `healthz`, `readyz`, `mcp`
(built-in routes).

Store a provider's key with:

```sh
echo "$VERCEL_KEY" | xolotl providers set-key vercel
```

## `[catalog]`

The model catalog lists the models each provider serves.

```toml
[catalog]
refresh_secs = 600
codex_client_version = "0.160.0"   # Codex filters its models by client version
grok_client_version = "1.0.52"     # xAI refuses old Grok CLI versions with 426

[catalog.static_models]
llamacpp = ["qwen3-coder"]         # added to what discovery finds

[catalog.wires]
llamacpp = ["chat", "messages"]    # formats the provider speaks natively
# "opencode-go/new-model" = ["messages"]
```

`static_models` adds models to the discovered ones. It also gives routing names
before the first discovery completes, and it covers providers that have no
model list.

`wires` sets the formats a provider, or one `provider/model`, speaks natively:
`messages`, `responses` or `chat`. Xolotl translates other formats. You can't
override `claude`. See [Clients](clients#wire-formats-and-translation).

### Model metadata

`catalog.metadata_url` defaults to LiteLLM's public model registry. Set it to an
empty string to disable network refresh. Refresh uses `catalog.refresh_secs`
and `xolotl models --refresh`. The last validated snapshot is cached in SQLite;
failed fetches keep its values and age. Models need no metadata to route.

Map or override an individual model with its full `provider/model` key:

```toml
[catalog.metadata."llamacpp/sample-model"]
litellm_key = "openai/sample-model"  # optional explicit registry key
max_input_tokens = 128000
max_output_tokens = 8000
input_cost_per_token = 0.000002
output_cost_per_token = 0.000008
cache_read_input_token_cost = 0.0000002
cache_creation_input_token_cost = 0.0000025
tools = true                       # Codex catalog shell tools
images = false
reasoning_levels = ["low", "high"]
```

Only supplied values replace registry values. Unmatched models show unknown
limits and prices. Prices are USD per token, displayed per million tokens.
Codex catalog synthesis defaults to text input, no shell tools, and no reasoning
levels. Declare supported capabilities explicitly. Native Codex entries retain
the upstream fields. See [Model metadata design](https://github.com/wyrd-company/xolotl/blob/main/docs/design/model-metadata.md).

## `[mcp]`

```toml
[mcp]
execute_timeout_secs = 120      # wall clock for one execute call
memory_limit_mb = 64            # JavaScript heap for one execute call
max_tool_calls = 100            # tool calls in one execute call
max_output_bytes = 100000
max_concurrent_executions = 8
tools_refresh_secs = 300
allowed_origins = []            # browser origins allowed to call /mcp

[mcp.oauth]
# redirect_uri = "http://127.0.0.1:33418/callback"
# client_metadata_url = "https://example.com/xolotl-client.json"
client_name = "xolotl"

[mcp.servers.github-work]
url = "https://api.githubcopilot.com/mcp/"
description = "GitHub, work account"
headers = { Authorization = "Bearer {token}" }
# timeout_secs = 60
```

See [MCP gateway](mcp-gateway).

## OpenBao paths

All paths are under `<mount>/<prefix>/`, by default `secret/xolotl/`.

| Path | Contents | Written by |
|---|---|---|
| `admin` | `token`: the bootstrap admin token | you |
| `oidc` | `client_secret`: the web UI's OpenID Connect client secret | you |
| `telemetry` | OTLP header values, keyed by header name | you |
| `providers/<name>` | `api_key` of a gateway provider | `xolotl providers set-key` |
| `accounts/<provider>/<id>` | OAuth tokens or API key of a subscription account | xolotl, at login and on refresh |
| `policies/<policy>` | a client key policy | `xolotl policies put` |
| `keys/<policy>/<client>` | a client key record, with `api_key` | `xolotl keys` |
| `mcp-servers/<server>` | an MCP server added at runtime | `xolotl mcp put` |
| `mcp/<server>` | values for the `{field}` placeholders in an MCP server's headers; `client_secret` of a pre-registered OAuth client | `xolotl mcp set-secret` |
| `mcp/<server>/oauth` | an MCP server's OAuth registration and tokens | xolotl, at login and on refresh |

Write the first three yourself with `bao kv put`:

```sh
bao kv put secret/xolotl/admin token="$(openssl rand -base64 32)"
bao kv put secret/xolotl/oidc client_secret="..."
bao kv put secret/xolotl/telemetry Authorization="Basic ..."
```

Xolotl reloads policies, keys, accounts and MCP servers from OpenBao every
60 seconds. `xolotl reload` reloads policies, keys and accounts at once, and
re-reads gateway provider keys.
