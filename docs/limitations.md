# Known limitations

| Limitation | Tracked in |
|---|---|
| No admin web UI; the CLI and `/admin` API are the only interface. A proposal exists. | [#1](https://github.com/wyrd-company/xolotl/issues/1) |
| No budgets or rate limits; policies only control models and names. | [#3](https://github.com/wyrd-company/xolotl/issues/3) |
| A refreshed OAuth token can still be lost: a lost token response after the provider rotated it, a crash during an OpenBao outage, or a login shared with a local CLI. Claude imports without `expiresAt` refresh only after a 401. | [#4](https://github.com/wyrd-company/xolotl/issues/4) |
| The provider side (logins, refresh, usage, Claude Code and Codex CLI end to end, T3Code hubs) was only tested against mocks. OpenBao was tested against a real server. | [#5](https://github.com/wyrd-company/xolotl/issues/5) |
| Single instance only: refreshes are serialized in-process, and cooldowns and session affinity are per instance. | [#6](https://github.com/wyrd-company/xolotl/issues/6) |

Smaller points, not tracked separately:

- **Codex streams only.** Codex always gets `stream: true`, so a non-streaming `/v1/responses` request through the Codex pool gets an SSE response. Codex CLI always streams.
- **No WebSocket transport for Codex.** `GET /v1/responses` returns 426 and Codex falls back to HTTP.
- **Providers come only from the config file.** Gateway providers can't be added at runtime; only their keys can be set (`xolotl providers set-key`).
- **No load balancing within a gateway provider.** Each has a single `base_url`. For several llama.cpp servers, configure one provider per server and use an alias with fallbacks.
- **Translation gaps.** `count_tokens` and `/v1/responses/compact` are not translated, so Claude Code's token counting fails on models that don't speak Messages. Hosted and server tools (web search, …) are dropped, not mapped. Responses `previous_response_id` is rejected. Structured output can't be sent to a Messages provider. Documents (PDFs) and other non-image attachments are dropped. Chat streams whose tool calls interleave are not supported.
- **Hard-coded OpenCode Go wire formats.** Which models speak Messages or Responses is a fixed list taken from OpenCode Go's documentation. New models default to Chat until the list or `[catalog.wires]` is updated.
