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
| `XOLOTL_CLICKHOUSE_URL` | `export.clickhouse.url`; sets up the export with its defaults when the file has no `[export.clickhouse]` |
| `XOLOTL_NATS_URL` | `export.nats.url`; sets up the export with its defaults when the file has no `[export.nats]` |

The CLI uses `XOLOTL_URL` and `XOLOTL_ADMIN_TOKEN` (see [CLI](cli.md)).

## `[server]`

```toml
[server]
listen = "0.0.0.0:8317"
max_body_bytes = 67108864      # 64 MiB, for OCR and batch uploads
upstream_timeout_secs = 600    # maximum idle time while reading an upstream response
# public_url = "https://xolotl.example.lan"
```

`listen` is the only address xolotl serves on. The LLM endpoints, the
passthroughs, `/mcp`, the T3 Code hubs endpoints (`/v0/management/...`), the
admin API and the web UI all use it. Port 8317 is only the default.

Set `public_url` to the URL where browsers reach xolotl. Then MCP OAuth logins
come back to xolotl and finish without a pasted URL.

## `[database]`

```toml
[database]
path = "xolotl.db"      # the container image uses /data/xolotl.db
retention_days = 90     # request log, MCP call log, sessions and tool counts
```

The database holds cooldowns, usage snapshots, the account index, the request
log, the MCP call log, sessions and tool counts. It holds no secrets. If you lose it, xolotl rebuilds
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

See [Telemetry](telemetry.md).

## `[export.clickhouse]`

Copies each session's requests and responses to a ClickHouse table. Off
without this section.

```toml
[export.clickhouse]
url = "http://clickhouse:8123"
database = "xolotl"
table = "llm_exchanges"
bodies = true            # false: request log fields only
```

See [ClickHouse export](clickhouse-export.md) for every setting and the table.

## `[export.nats]`

Publishes the same rows to a NATS JetStream stream, which ClickHouse loads.
Off without this section. Xolotl stops at startup when the stream is missing.

```toml
[export.nats]
url = "nats://nats:4222"
stream = "XOLOTL"
```

See [NATS export](nats-export.md) for every setting, the stream and the tables.

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

See [Web UI](web-ui.md).

## `[subscription]`

How xolotl uses subscription accounts. See [Subscription accounts](accounts.md).

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

## `[sessions]`

```toml
[sessions]
lock_tokens = 60000     # 0 disables the context lock
```

A session whose prompt reaches `lock_tokens` stays on its account and model, so
that its prompt cache is kept. Set it to `0` to turn this off and keep failover
for every request. See [Context lock](accounts.md#context-lock).

## `[gateway.providers.<name>]`

Provider accounts from the config file. A provider account is an API-key
upstream. The name is a model prefix (`<name>/<model>`) and a path prefix
(`/<name>/...`). You can also add provider accounts at runtime, without this
section; see [Provider accounts](accounts.md#provider-accounts). Providers from
the config file are read-only at runtime, except for their key.

```toml
[gateway.providers.vercel]
kind = "vercel"

[gateway.providers.mistral]
kind = "mistral"

[gateway.providers.llamacpp]
kind = "openai-compatible"
base_url = "http://llama.example.lan:8080"
no_auth = true

[gateway.providers.anthropic-team]
kind = "anthropic"
headers = { "anthropic-workspace-id" = "wrkspc_abc123" }
```

| Field | Meaning |
|---|---|
| `kind` | `anthropic`, `openai`, `vercel`, `mistral` or `openai-compatible` (default). See [Anthropic and OpenAI API accounts](accounts.md#anthropic-and-openai-api-accounts) |
| `base_url` | Required for `openai-compatible`; overrides the default for the others |
| `secret` | OpenBao secret with the `api_key` field. Default `providers/<name>` |
| `no_auth` | `true` for a server that needs no key |
| `headers` | Extra static headers sent upstream |
| `enabled` | `false` turns the provider off |
| `workload_identity` | `anthropic` and `openai` only: authenticate with workload identity federation instead of a key. See below |

### Workload identity

An `anthropic` or `openai` provider with `workload_identity` has no key. Xolotl
reads the JWT in `token_file`, exchanges it at the vendor's token endpoint for a
service account's access token, and sends that token as a bearer token. It
exchanges again 5 minutes before the token expires (at 80% of a shorter
lifetime), and after a 401. It reads the file again for every exchange, so a
token that your platform rotates on disk is picked up. One exchange runs at a
time for each provider.

```toml
[gateway.providers.anthropic-ci]
kind = "anthropic"
workload_identity = { token_file = "/var/run/secrets/anthropic.com/token", service_account_id = "svac_...", federation_rule_id = "fdrl_...", organization_id = "00000000-0000-0000-0000-000000000000" }

[gateway.providers.openai-ci]
kind = "openai"
workload_identity = { token_file = "/var/run/secrets/openai.com/token", service_account_id = "...", identity_provider_id = "..." }
```

| Field | Kind | Meaning |
|---|---|---|
| `token_file` | both | The file that holds the identity provider's JWT |
| `service_account_id` | both | The service account the token acts as |
| `federation_rule_id` | `anthropic` | The federation rule (`fdrl_...`) |
| `organization_id` | `anthropic` | The organization's UUID |
| `workspace_id` | `anthropic` | The workspace (`wrkspc_...`). Required when the rule is enabled for more than one |
| `identity_provider_id` | `openai` | The Workload Identity Provider's ID |
| `token_url` | both | The token endpoint. Default `https://api.anthropic.com/v1/oauth/token` or `https://auth.openai.com/oauth/token` |

You can set `workload_identity` only in the config file, because it makes the
server read a file. A failed exchange fails the request with a 502 that names
the reason. Some identity tokens (those with a `jti` claim) can be exchanged
only once; make sure your platform writes a new one before the access token
expires.

Without a `[gateway]` section, xolotl configures `vercel` and `mistral`. When
you add any provider to the file, list every config-file provider you want.

Names are letters, digits, `-` and `_`. These names are refused:
`claude`, `codex`, `grok`, `opencode-go` (subscription accounts), and `v0`,
`v1`, `admin`, `ui`, `assets`, `backend-api`, `healthz`, `readyz`, `mcp`
(built-in routes).

Store a provider's key on the web UI's **Accounts** screen: under
**Provider accounts**, select **Set key** (**Replace key** when one is stored)
and save it. With the CLI:

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
override `claude`. See [Clients](clients.md#wire-formats-and-translation).

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
the upstream fields.

To find a model in the registry, xolotl tries `provider/model`, then the model
under its provider family (Codex as OpenAI, Claude as Anthropic, Grok as xAI,
OpenCode Go Qwen as DashScope), then the bare model name. `litellm_key`
replaces this lookup.

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

See [MCP gateway](mcp-gateway.md).

## OpenBao paths

All paths are under `<mount>/<prefix>/`, by default `secret/xolotl/`.

| Path | Contents | Written by |
|---|---|---|
| `admin` | `token`: the bootstrap admin token | you |
| `oidc` | `client_secret`: the web UI's OpenID Connect client secret | you |
| `telemetry` | OTLP header values, keyed by header name | you |
| `clickhouse` | `password` and, optionally, `user` for the [ClickHouse export](clickhouse-export.md) | you |
| `nats` | `user` and `password`, or `creds` (a `.creds` file), for the [NATS export](nats-export.md) | you |
| `providers/<name>` | `api_key` of a config-file provider; a provider account added at runtime, with its `api_key` | `xolotl providers set-key`, `xolotl accounts add-provider` |
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
re-reads provider keys.
