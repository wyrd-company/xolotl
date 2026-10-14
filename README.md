# Xolotl Proxy Gateway

## Why

- Avoid having the actual secrets in agent devcontainers, where they may accidentally leak secrets
- Integrate with my OpenBao server, so secrets aren't sitting on disk or in ENV vars.
- Centralized so I can set it up in one place
- Metrics, Logs, and Traces so I can monitor/alert using OpenObserve
- Use multiple subscription accounts and track usage/limits
- Works with T3Code usage reporting via hubs
- Fast and lightweight
- One MCP gateway for agents, with per-key access to MCP servers and their credentials kept out of the devcontainer

## Implementation Notes

- Written in Rust
    - `xolotl` cli
    - Container image on GHCR
    - ci + cd, deploy on version tag `0.0.0` (no v)
- OpenBao/Vault integration
    - Upstream provider key storage
    - Downstream client key management (issue, rotate, revoke)
    - Access vault using AppRole
- OpenTelemetry
- LLM Subscription Proxy server <!-- Supporting the services I subscribe to now -->
    - Reference implementations: CLIProxyAPI & opencodex
    - Supports:
        - Codex oauth
        - Claude oath
        - OpenCode Go (API)
    - Simple CLI authentication device flows
    - Subscription usage/remaining
    - Multi-account load balancing/routing
    - Supports clients:
        - codex cli
        - claude code cli
- AI Gateway Server <!-- These are the services I use now -->
    - Reference implementations: bifrost, litellm
    - Provider support:
        - Vercel AI Gateway - all methods
        - Mistral AI
            - OCR
            - Batch
        - llama.cpp (OpenAI-compatible)
        - OpenCode Go
- MCP gateway
    - Upstream: Streamable HTTP MCP servers, several instances of one server with different credentials: header secrets or OAuth logins, kept in OpenBao
    - Downstream: one code-mode MCP server at `/mcp` (`search` + `execute`, QuickJS sandbox), MCP 2026-07-28 and older clients
    - Policies select MCP servers and tools
- T3Code Hubs compatibility (CLIProxyAPI)
    - `GET /v0/management/auth-files` to list accounts. It uses enabled Codex and Claude accounts.
    - `POST /v0/management/api-call` to fetch each account’s provider usage. The provider URLs are Codex’s usage endpoint and Anthropic’s OAuth usage endpoint. For Codex, it also fetches reset credits and can redeem one.
    - `POST /v0/management/reset-quota` after a reset credit is redeemed, to clear that account’s hub cooldown

- Learn from others implementations:
  - Already established providers for capabilities, we are just putting it together
    - Use best of class implementation as reference - copy what works, but not bugs. Make sensisble improvements when the opportunity presents itself.

## Usage

Design notes (architecture, routing, accounts, keys, releases, known limitations) are in [`docs/`](docs/README.md).

### 1. OpenBao

Xolotl reads and writes everything secret in a KV v2 mount under one prefix (default `secret/xolotl/`) and logs in with AppRole:

```sh
bao auth enable approle
bao policy write xolotl - <<'HCL'
path "secret/data/xolotl/*"     { capabilities = ["create", "read", "update", "delete"] }
path "secret/metadata/xolotl/*" { capabilities = ["list", "read", "delete"] }
path "secret/destroy/xolotl/*"  { capabilities = ["update"] }
HCL
bao write auth/approle/role/xolotl token_policies=xolotl token_ttl=1h token_max_ttl=24h
bao read -field=role_id auth/approle/role/xolotl/role-id
bao write -f -field=secret_id auth/approle/role/xolotl/secret-id

# Bootstrap admin token (used by the CLI). Later changes: `bao kv patch`, so the
# web UI's password hash in the same secret is kept.
bao kv put secret/xolotl/admin token="$(openssl rand -base64 32)"
# Optional: OTLP headers for OpenObserve
bao kv put secret/xolotl/telemetry Authorization="Basic ..."
```

| Path (under the prefix) | Contents |
|---|---|
| `admin` | `token`: bootstrap admin token; `ui_password_hash`: web UI password (argon2id) |
| `telemetry` | OTLP header values |
| `providers/<name>` | `api_key` for gateway providers |
| `accounts/<provider>/<id>` | OAuth tokens / API key for subscription accounts (rewritten on refresh) |
| `mcp-servers/<server>` | MCP servers added from the web UI or `xolotl mcp put` |
| `mcp/<server>` | values for the `{field}` placeholders in an MCP server's headers; `client_secret` of a pre-registered OAuth client |
| `mcp/<server>/oauth` | an MCP server's OAuth client registration and tokens (rewritten on refresh) |
| `policies/<policy>` | client key policy |
| `keys/<policy>/<client>` | client key record, including `api_key`, for that client to read |

Runtime state (cooldowns, usage snapshots, request log) lives in SQLite. It contains no secrets and is rebuilt from OpenBao if lost.

### 2. Run

```sh
cp config.example.toml config.toml   # edit vault/telemetry/providers
docker run -d --name xolotl -p 8317:8317 \
  -v $PWD/config.toml:/etc/xolotl/config.toml:ro -v xolotl-data:/data \
  -e XOLOTL_VAULT_ROLE_ID=... -e XOLOTL_VAULT_SECRET_ID=... \
  ghcr.io/wyrd-company/xolotl:latest
```

`GET /healthz` is liveness and `GET /readyz` checks SQLite and OpenBao.

### 3. Administer in the browser

Set a password, then open `http://xolotl:8317/`:

```sh
xolotl ui set-password   # hashed locally; only the argon2id hash is stored in OpenBao
```

The web UI covers what the CLI does: accounts and logins, policies (with a live preview of the names a key sees), client keys, provider keys and the model catalog, and usage. See [Web UI](docs/web-ui.md).

### 4. Administer with the CLI

The CLI talks to the server's admin API (`XOLOTL_URL`, default `http://127.0.0.1:8317`, and `XOLOTL_ADMIN_TOKEN`).

```sh
# Subscription accounts
xolotl auth login codex            # device code flow (add --browser for the paste flow)
xolotl auth login claude           # opens claude.ai, paste back CODE#STATE
xolotl auth login grok             # Grok Build (xAI) device code flow
xolotl accounts add-key            # OpenCode Go API key (stdin)
xolotl accounts import codex ~/.codex/auth.json   # or claude ~/.claude/.credentials.json
xolotl accounts list               # state, plan, usage windows
xolotl accounts priority <id> 10   # higher is used first
xolotl accounts disable|enable|reset|remove <id>

# Gateway provider keys
echo "$VERCEL_KEY" | xolotl providers set-key vercel

# Models, policies and client keys
xolotl models --refresh            # what each provider serves
xolotl policies put examples/policies/devcontainer.toml
xolotl policies view devcontainer  # the names a key under this policy sees, and where they route
xolotl keys issue --policy devcontainer --name laptop --ttl 90d
xolotl keys list
xolotl keys rotate devcontainer/laptop --grace 1h
xolotl keys revoke devcontainer/laptop

xolotl usage --group-by client --since 7d

# MCP gateway
xolotl mcp servers                 # source, connection, tools, secret fields
xolotl mcp put linear.toml         # add or change a server stored in OpenBao (name + [mcp.servers] fields)
xolotl mcp remove linear           # refused while a policy lists it
xolotl mcp set-secret github-work token   # value on stdin, fills {token} in the headers
xolotl mcp login linear            # OAuth: open the URL, approve, paste the URL the browser ends on
xolotl mcp logout linear
xolotl mcp tools github-work
xolotl mcp refresh
xolotl mcp usage --group-by tool
```

Imported tokens keep rotating on refresh. Stop using the same login in the local CLI afterwards, or one side ends up with a dead refresh token.

### 5. Policies and client keys

Every client key belongs to a policy (see [`examples/policies/devcontainer.toml`](examples/policies/devcontainer.toml)):

- `providers`: which providers the key may use (`claude`, `codex`, `grok`, `opencode-go`, gateway provider names, or `*`).
- `allow_models` / `deny_models`: globs matched against `model` or `provider/model`; deny wins.
- `precedence` and `winners`: who owns a model name served by several providers. The winner serves the plain name (`claude-haiku-5-5`), the others drop it. Without a `precedence` list the order is `claude`, `codex`, `grok`, `opencode-go`, then gateway providers alphabetically.
- `aliases`: extra names, each a `provider/model` or a list tried in order on 429/5xx. Use them to bring back a model that lost its plain name.
- `scopes`: `proxy` (default), `management` (T3Code hubs), `mcp` (MCP gateway), `admin`.
- `mcp_servers`, `allow_tools` / `deny_tools`: the MCP servers (or `*`) a key may use through `/mcp`, and globs on `tool` or `server/tool` (deny wins). See [MCP gateway](docs/mcp.md).

The explicit `provider/model` form always works for any model the policy allows, even one that lost its plain name. `GET /v1/models` lists the plain names and aliases.

Keys are issued into the vault and never printed. A client reads its own key with a Vault policy scoped to its record:

```hcl
path "secret/data/xolotl/keys/devcontainer/laptop" { capabilities = ["read"] }
```

```sh
export XOLOTL_API_KEY=$(bao kv get -field=api_key secret/xolotl/keys/devcontainer/laptop)
```

Revoking writes a marker as the latest version and destroys the earlier versions. Writes use check-and-set, so an instance with stale data can't overwrite a key changed elsewhere. Unknown keys trigger a reload (at most every 10 s), so keys issued by another instance work quickly.

### 6. Point clients at it

**Claude Code**

```sh
export ANTHROPIC_BASE_URL=http://xolotl:8317
export ANTHROPIC_AUTH_TOKEN=$XOLOTL_API_KEY      # or ANTHROPIC_API_KEY
```

**Codex CLI** (`~/.codex/config.toml`)

```toml
model_provider = "xolotl"

[model_providers.xolotl]
name = "xolotl"
base_url = "http://xolotl:8317/v1"
env_key = "XOLOTL_API_KEY"
wire_api = "responses"
```

**Grok CLI** (Grok Build): the CLI runs `auth_provider_command` and sends what it prints as the bearer token, so point it at a command that prints the client key. Declare a model on xolotl with the Responses backend:

```toml
[auth]
auth_provider_command = "secrets-helper xolotl-key"   # prints $XOLOTL_API_KEY

[model."grok-build"]
base_url = "http://xolotl:8317/v1"
model = "grok-build"
api_backend = "responses"
```

`GET /v1/models` answers in the Grok CLI's shape when the user agent contains `grok-shell`. Xolotl forwards the CLI's own `x-grok-client-version`; other clients reaching Grok send `catalog.grok_client_version`.

**MCP clients**: add a Streamable HTTP server with URL `http://xolotl:8317/mcp` and header `Authorization: Bearer $XOLOTL_API_KEY`, using a key whose policy has the `mcp` scope. For Claude Code:

```sh
claude mcp add --transport http xolotl http://xolotl:8317/mcp --header "Authorization: Bearer $XOLOTL_API_KEY"
```

The client sees two tools: `search` (TypeScript declarations of the tools the key may call) and `execute` (JavaScript that calls them). Upstream servers are configured under `[mcp.servers]` in the config file; see [MCP gateway](docs/mcp.md).

**T3Code hubs**: add a CLIProxyAPI usage source with URL `http://xolotl:8317` and a key whose policy has the `management` scope.

**Anything else** (OpenAI-compatible SDKs): base URL `http://xolotl:8317/v1` and any model name the key's policy exposes.

### Routing

| Request | Goes to |
|---|---|
| `POST /v1/messages`, `/v1/responses`, `/v1/chat/completions`, other `POST /v1/*` | the target the key's policy resolves `model` to (alias, plain name or `provider/model`) |
| `GET /v1/models` | the policy's names, each with its native and translated endpoints; in Anthropic's shape when the request carries `anthropic-version` (Claude Code) |
| `/opencode-go/<path>` (any method) | OpenCode Go API through the account pool (load balancing, failover, usage) |
| `/<provider>/<path>` (any method) | raw passthrough with the gateway provider's key, e.g. `POST /mistral/v1/ocr`, `/mistral/v1/batch/jobs`, `/mistral/v1/files`, `GET /vercel/v1/credits`, `/vercel/v1/generation?id=` |
| `/mcp` | MCP gateway (code mode over the upstream MCP servers the key's policy allows) |
| `/v0/management/{auth-files,api-call,reset-quota}` | T3Code hubs (CLIProxyAPI-compatible) |
| `/admin/...` | CLI and web UI |
| `/`, `/assets/...` | web UI |

Passthroughs strip the client's credentials and inject the provider's. Models are reachable on all three LLM endpoints: a provider that speaks the endpoint's format gets the request as is, any other gets it translated between Anthropic Messages, OpenAI Responses and Chat Completions (Claude speaks Messages, Codex and Grok Responses, OpenCode Go per model, Vercel all three, others Chat; override with `[catalog.wires]`). So Claude Code can use Codex models, and Codex CLI can use Anthropic models served by Vercel or OpenCode Go. Claude subscription accounts are the exception: they serve only the Claude Code API path (`/v1/messages`) and are never translated. `count_tokens` and `/v1/responses/compact` are not translated either. Such requests get a 400 naming the endpoints the model is served on.

Account pools refresh OAuth tokens before they expire and once more on a 401. Refresh tokens are single-use, so a rotated credential that can't be written to OpenBao keeps serving from memory, is retried every 15 s and on shutdown, and shows as `UNSAVED` in `xolotl accounts list` (metric `xolotl.vault.unsaved_credentials`). Run a single instance: refreshes are serialized per account in-process only. An account whose quota is exhausted (429 / `usage_limit_reached`, including when it arrives inside a 200 stream) cools down until its reset time and the request moves to the next account. Strategy is `round-robin`, `fill-first` or `least-used`. A session (Codex `session_id`, Claude Code session id, Grok `x-grok-session-id` or `x-grok-conv-id`) stays on one account to keep prompt caches warm. Usage windows are polled from each provider and also read from response headers.

`api-call` only reaches the account's own provider hosts, so a management key can't send account tokens to arbitrary URLs.

### Telemetry

Set `telemetry.otlp_endpoint` (OpenObserve: `http://openobserve:5080/api/<org>`) to export traces, logs and metrics over OTLP/HTTP:

- `xolotl.requests`, `xolotl.request.duration`, `xolotl.tokens` (by provider, model, client, status)
- `xolotl.account.usage_percent` (by account and window), `xolotl.account.available`
- `xolotl.upstream.errors`, `xolotl.oauth.refreshes`
- `xolotl.mcp.tool_calls` (by server, tool, status), `xolotl.mcp.tool_call.duration`, `xolotl.mcp.executions`

### Development

```sh
cargo test
cargo clippy --all-targets -- -D warnings

# Web UI (Vite + React + Tailwind + shadcn/ui), in ui/
pnpm install
pnpm dev     # proxies /admin to a xolotl on 127.0.0.1:8317 (XOLOTL_URL)
pnpm build   # ui/dist: embedded into release builds, read from disk by debug builds
```

### Releases

Versions are managed with [Intentional](https://github.com/wyrd-company/intentional) (`cargo install intentional-cli --locked`). Tags are plain versions (`0.1.0`, no `v`); `0.0.0` is the baseline.

Each user-visible change ships with an intent:

```sh
intentional add --release-unit xolotl:minor --message "Describe the change."
```

To release, on `main`:

```sh
intentional status && intentional check
intentional plan > release-plan.json
intentional apply                     # Cargo.toml, CHANGELOG.md, consumes intents
cargo update -w --offline             # keep Cargo.lock in step
git add -A && git commit -m "chore: release $(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)"
intentional tag --plan release-plan.json
git push origin main --follow-tags
```

Intentional is checked only at release time: the release workflow runs `intentional check` and only deploys a tag that is an Intentional release record (annotated, `baseline: false`, matching `Cargo.toml` and `CHANGELOG.md`): it tests, publishes `ghcr.io/<repo>:<version>` (amd64 + arm64) and a GitHub release with a Linux binary. Baseline records are skipped and any other tag is refused.
