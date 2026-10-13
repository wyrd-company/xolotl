# Architecture

Xolotl is a single Rust binary (`xolotl`). `xolotl serve` runs the HTTP server; every other subcommand is a CLI client of the server's `/admin` API.

```
clients (Claude Code, Codex CLI, SDKs, T3Code hubs, xolotl CLI)
        │  client key (Authorization / x-api-key)
        ▼
  axum router ── authorize (keys + policy) ──┬── subscription pools: codex, claude, opencode-go
        │                                     └── gateway providers: vercel, mistral, openai-compatible
        ├── OpenBao (KV v2, AppRole): every secret
        ├── SQLite: runtime state, no secrets
        └── OpenTelemetry (OTLP/HTTP): traces, logs, metrics
```

## Two kinds of upstream

- **Subscription pools** (`claude`, `codex`, `opencode-go`) hold several accounts per provider. Codex and Claude accounts use OAuth tokens; OpenCode Go accounts use API keys. A request goes to one account chosen by the pool, and fails over to another on quota, auth or transient errors. See [Subscription accounts](subscriptions.md).
- **Gateway providers** are configured in the config file under `[gateway.providers.<name>]` with a `kind`: `vercel`, `mistral` or `openai-compatible` (used for llama.cpp). Each has one API key in OpenBao at `providers/<name>`, or `no_auth = true`. Provider names become model prefixes and path prefixes, so names like `v1`, `admin` or `claude` are rejected.

## Storage

| Store | Holds | Why |
|---|---|---|
| OpenBao, KV v2 under `secret/xolotl/` | admin token, telemetry headers, provider keys, account credentials, policies, client key records | Secrets stay off disk and out of env vars. Writes that can race use check-and-set. |
| SQLite (`database.path`) | accounts index, cooldowns, usage snapshots, request log (pruned after `retention_days`, default 90) | Cheap, frequently changing state. Holds no secrets and is rebuilt from OpenBao if lost. |

`src/vault.rs` is the OpenBao client: AppRole login, re-login on 403, KV v2 get/put/list/delete, versioned reads, check-and-set writes and version destroy. It implements the `SecretStore` trait (`src/secrets.rs`); tests use the in-memory `MemoryStore`.

## Request path

1. `authorize` takes the key from `Authorization` (Bearer or bare), `x-api-key` or `x-management-key`, and resolves it to a principal: client id plus policy. The bootstrap admin token resolves to an unrestricted policy.
2. The route checks the scope it needs (`proxy`, `management` or `admin`).
3. LLM endpoints resolve `model` through the policy and dispatch to a pool or a gateway provider, translating the request when the provider doesn't speak the endpoint's format ([Routing](routing.md)). Passthroughs check only the provider (and `model`, when the body is JSON).
4. Responses are relayed as they arrive. `TapStream` (`src/proxy.rs`) reads token usage from the upstream JSON or SSE body on the way through, and `Accounting` writes the request log row and metrics when the response finishes. A translated response is translated after this tap, so usage always comes from what the upstream sent.

## Background tasks

Started by `server::spawn_background`:

| Every | Task |
|---|---|
| 60 s | Reload client keys and policies from OpenBao; sync accounts (picks up added and removed accounts; keeps the in-memory credentials of accounts it already holds) |
| `catalog.refresh_secs` (600 s) | Re-discover models from every provider |
| 15 s | Retry OpenBao writes of refreshed OAuth credentials that failed ([details](subscriptions.md#when-the-vault-write-fails)) |
| `subscription.usage_poll_secs` (300 s) | Poll each enabled account's usage; this also keeps OAuth tokens fresh |
| 24 h | Prune the request log |

On graceful shutdown the server makes one last attempt to save unsaved credentials.

## Telemetry

`src/telemetry.rs` sets up OTLP/HTTP export of traces, logs and metrics when `telemetry.otlp_endpoint` is set; header values come from OpenBao at `telemetry`. Metrics are listed in the project README.

## Source layout

| Path | Contents |
|---|---|
| `src/main.rs`, `src/cli.rs` | CLI parsing; `serve` and the admin API client |
| `src/config.rs` | TOML config, defaults, validation |
| `src/server/mod.rs` | Router, auth, LLM routing, passthroughs, background tasks |
| `src/server/admin.rs` | `/admin` API used by the CLI |
| `src/server/management.rs` | T3Code hubs endpoints (`/v0/management/*`) |
| `src/policy.rs` | Policies and model-name resolution |
| `src/catalog.rs` | Model catalog, wire formats and routes (native or translated) |
| `src/keys.rs` | Client keys and policy storage |
| `src/subscription/` | Account pools: `mod.rs` (selection, failover, refresh), `codex.rs`, `claude.rs`, `opencode_go.rs`, `oauth.rs` (PKCE, token errors) |
| `src/gateway.rs` | Gateway providers |
| `src/proxy.rs` | Header filtering, streaming relay, token usage, error shapes |
| `src/translate/` | Translation between Messages, Responses and Chat: requests, responses and streams |
| `src/db.rs`, `migrations/` | SQLite |
| `src/vault.rs`, `src/secrets.rs` | OpenBao client and the `SecretStore` trait |
| `src/telemetry.rs` | OpenTelemetry setup and metrics |
