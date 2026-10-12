# Xolotl Proxy Gateway

## Why

- Avoid having the actual secrets in agent devcontainers, where they may accidentally leak secrets
- Integrate with my OpenBao server, so secrets aren't sitting on disk or in ENV vars.
- Centralized so I can set it up in one place
- Metrics, Logs, and Traces so I can monitor/alert using OpenObserve
- Use multiple subscription accounts and track usage/limits
- Works with T3Code usage reporting via hubs
- Fast and lightweight

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
- T3Code Hubs compatibility (CLIProxyAPI)
    - `GET /v0/management/auth-files` to list accounts. It uses enabled Codex and Claude accounts.
    - `POST /v0/management/api-call` to fetch each account’s provider usage. The provider URLs are Codex’s usage endpoint and Anthropic’s OAuth usage endpoint. For Codex, it also fetches reset credits and can redeem one.
    - `POST /v0/management/reset-quota` after a reset credit is redeemed, to clear that account’s hub cooldown

- Learn from others implementations:
  - Already established providers for capabilities, we are just putting it together
    - Use best of class implementation as reference - copy what works, but not bugs. Make sensisble improvements when the opportunity presents itself.