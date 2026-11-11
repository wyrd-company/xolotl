<p align="center"><img src="brand/xolotl-emblem.png" alt="xolotl" height="240"></p>

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

## Documentation

User documentation is on [wyrd.foo](https://wyrd.foo/docs/xolotl/). Its source is in [`docs/`](docs/):

| Page | Covers |
|---|---|
| [Overview](docs/user/overview.md) | What xolotl does and how it fits together |
| [Installation](docs/user/install.md) | Container image, release binary, Homebrew, building from source, OpenBao setup, running |
| [Configuration](docs/user/configuration.md) | The config file, environment variables, OpenBao paths |
| [Web UI](docs/user/web-ui.md) | Sign-in and the screens you administer xolotl from |
| [Subscription accounts](docs/user/accounts.md) | Codex, Claude, Grok and OpenCode Go accounts: login, import, states, failover |
| [Policies and client keys](docs/user/policies-and-keys.md) | Policies, model names, renames and aliases, rate limits, issuing and rotating keys, usage |
| [Clients](docs/user/clients.md) | Claude Code, Codex CLI, Grok CLI, SDKs, MCP clients, T3 Code; endpoints and translation |
| [MCP gateway](docs/user/mcp-gateway.md) | Upstream MCP servers, OAuth logins, code mode, MCP policies |
| [CLI reference](docs/user/cli.md) | Every `xolotl` command |
| [Telemetry](docs/user/telemetry.md) | OTLP export, logs, metrics |
| [ClickHouse export](docs/user/clickhouse-export.md) | Session requests and responses in a ClickHouse table for analysis |
| [NATS export](docs/user/nats-export.md) | The same rows through NATS JetStream, so they survive ClickHouse maintenance |
| [Known limitations](docs/user/limitations.md) | What isn't done yet |

Design notes (architecture, routing, accounts, keys, MCP, web UI, releases) are in [`docs/design/`](docs/design/README.md).

## Quick start

```sh
# OpenBao: an AppRole for xolotl and the bootstrap admin token (see docs/user/install.md)
bao kv put secret/xolotl/admin token="$(openssl rand -base64 32)"

# Run
cp config.example.toml config.toml   # edit [vault] at least
docker run -d --name xolotl -p 8317:8317 \
  -v $PWD/config.toml:/etc/xolotl/config.toml:ro -v xolotl-data:/data \
  -e XOLOTL_VAULT_ROLE_ID=... -e XOLOTL_VAULT_SECRET_ID=... \
  ghcr.io/wyrd-company/xolotl:latest

# Administer in the web UI at http://xolotl:8317/ once [ui.oidc] is set
# (see docs/user/web-ui.md), or with the xolotl binary from a release archive:
export XOLOTL_ADMIN_TOKEN=$(bao kv get -field=token secret/xolotl/admin)
xolotl auth login codex
xolotl policies put examples/policies/devcontainer.toml
xolotl keys issue --policy devcontainer --name laptop --ttl 90d

# Use
export ANTHROPIC_BASE_URL=http://xolotl:8317
export ANTHROPIC_AUTH_TOKEN=$(bao kv get -field=api_key secret/xolotl/keys/devcontainer/laptop)
claude
```

## Contributing

### Development

```sh
cargo test
cargo clippy --all-targets -- -D warnings

# Web UI (Vite + React + Tailwind + shadcn/ui), in ui/
pnpm install
pnpm dev     # proxies /admin to a xolotl on 127.0.0.1:8317 (XOLOTL_URL)
pnpm build   # ui/dist: embedded into release builds, read from disk by debug builds
```

### Documentation

`docs/user/` is the wyrd.foo v2 docs tree. User pages are its `**/*.md` files, each with `product: xolotl` in its front matter; `docs/user/docs.yml` is the catalog entry and lists every asset the pages and the entry use. The v2 publisher publishes everything under `docs/user/` and nothing else in `docs/`. A page links to another page by its file name (`[Clients](clients.md#heading)`), so the link also works on GitHub. A page links only to other pages, to assets that `docs.yml` lists, and to full URLs: nothing else in the repository is on the site. An intent links only to full URLs, because the changelogs show its text away from the pages. `cargo test --test docs_links` checks every link in the pages, the changelogs, the pending intents and this README, and CI runs it. `.github/workflows/publish-docs.yml` publishes to wyrd.foo when a docs unit tag (`xolotl-docs@<version>`) is pushed, and on a manual run. A code unit release publishes from `release.yml`, after the GitHub Release; a docs tag pushed with a code tag on the same commit leaves its publication to that run. A push to `main` does not publish. A docs-only fix is an `xolotl-docs` intent: it releases the docs unit on its own.

Record the demo again after a change it shows (needs Docker, `jq` and [VHS](https://github.com/charmbracelet/vhs)):

```sh
cargo build && vhs docs/demo.tape   # writes docs/user/assets/demo.gif
```

### Releases

Versions are managed with [Intentional](https://github.com/wyrd-company/intentional) (`cargo install intentional-cli --version 0.4.1 --locked`). Tags are plain versions (`0.1.0`, no `v`); `0.0.0` is the baseline.

Each user-visible change ships with an intent:

```sh
intentional add --release-unit xolotl:minor --message "Describe the change."
```

To release, on a branch from `main`:

```sh
intentional status && intentional check
intentional plan > release-plan.json  # committed with the release; the tag step reads it
intentional apply                     # version projections, changelogs, consumes intents
cargo update -w --offline             # keep Cargo.lock in step
git add -A && git commit -m "chore: release $(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)"
```

Raise a PR with only that commit. After it merges, on `main` at that commit:

```sh
intentional tag --plan release-plan.json
git push origin --follow-tags main    # the code tag and the docs tag together
```

Intentional is checked only at release time: the release workflow runs `intentional check` and only deploys a tag that is an Intentional release record (annotated, `baseline: false`, matching `Cargo.toml` and `CHANGELOG.md`): it tests, then publishes a GitHub release with the Linux x86_64, Linux arm64 and macOS arm64 binaries, the Homebrew formula, `ghcr.io/<repo>:<version>` (amd64 + arm64) and the wyrd.foo docs. Baseline records are skipped and any other tag is refused.
