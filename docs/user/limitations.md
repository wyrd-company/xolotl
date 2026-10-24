---
title: Known limitations
order: 11
product: xolotl
---

| Limitation | Tracked in |
|---|---|
| A rotated OAuth token cannot be recovered if its response is lost, or if xolotl crashes before an unsaved credential reaches OpenBao. Secrets persist only in OpenBao. Shared logins can invalidate refresh tokens. Unknown outcomes are logged, failed refreshes back off, and token-reuse rejections suggest checking for shared logins. See [token refresh](accounts#token-refresh). | [#4](https://github.com/wyrd-company/xolotl/issues/4) |
| The provider side (logins, refresh, usage, Claude Code and Codex CLI end to end, T3Code hubs) was only tested against mocks. OpenBao was tested against a real server. | [#5](https://github.com/wyrd-company/xolotl/issues/5) |
| Single instance only: refreshes are serialized in-process, and cooldowns and session affinity are per instance. MCP OAuth logins re-read the vault before refreshing and reconcile conflicting writes, but two instances refreshing the same login at the same moment can still spend one refresh token twice. | [#6](https://github.com/wyrd-company/xolotl/issues/6) |

Smaller points, not tracked separately:

- **Codex streams only.** Codex always gets `stream: true`, so a non-streaming `/v1/responses` request through the Codex pool gets an SSE response. Codex CLI always streams.
- **No WebSocket transport for Codex.** `GET /v1/responses` returns 426 and Codex falls back to HTTP.
- **Web UI: one role.** Every member of the admin group is a full admin. Sessions and sign-ins in progress live in memory, so a restart signs everyone out.
- **Providers come only from the config file.** Gateway providers can't be added at runtime; only their keys can be set (`xolotl providers set-key`).
- **No load balancing within a gateway provider.** Each has a single `base_url`. For several llama.cpp servers, configure one provider per server and use an alias with fallbacks.
- **Translation gaps.** `/v1/responses/compact` is native only. Translated `count_tokens` uses a UTF-8 byte estimate, not a model tokenizer; images and tokenizer differences can make it inaccurate. Hosted and server tools (web search, …) are dropped, not mapped. Responses `previous_response_id` is rejected. Structured output can't be sent to a Messages provider. Documents (PDFs) and other non-image attachments are dropped. Tool calls from a Chat provider reach the client whole, not argument by argument.
- **MCP gateway.** Upstream servers must speak Streamable HTTP (no stdio). Credentials are header values or one OAuth login per server instance (not per client key). Tools that ask for user input (elicitation, sampling) or return tasks are not supported. rmcp refuses authorization servers on private-network addresses unless both the MCP server and the authorization server are on loopback, so a self-hosted identity provider on the LAN can't be discovered. Tool lists are refreshed periodically rather than on `list_changed`. `/mcp` authenticates with client keys, not the OAuth resource-server profile of the MCP spec. Code mode has no approvals; use `deny_tools`.
- **Rate limits count requests and tokens, not spend.** There is no price list, so gateway providers can't be capped in money. Rate limit counters are per instance. See [Rate limits](policies-and-keys#rate-limits).
- **Hard-coded OpenCode Go wire formats.** Which models speak Messages or Responses is a fixed list taken from OpenCode Go's documentation. New models default to Chat until the list or `[catalog.wires]` is updated.

- **Model prices.** API-equivalent costs are list-price estimates, not subscription spend. They exclude requests with missing prices or usage. Tiered pricing, regional rates, discounts and other billing rules are not applied.
