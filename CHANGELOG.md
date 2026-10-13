# Changelog

## 0.1.0

### Features

- Add an admin web UI at `/`: accounts and logins, policies with a live preview of the names a key sees (including translated endpoints), client keys, provider keys and the model catalog, and usage. Sign in with one password, set with `xolotl ui set-password` and stored as an argon2id hash in OpenBao. New admin endpoints `GET /admin/status`, `GET /admin/providers`, `GET /admin/catalog/routes` and `POST /admin/policy-preview`, and `telemetry.dashboard_url` for the link to OpenObserve.
- First release of the Xolotl proxy gateway: Codex, Claude and OpenCode Go subscription pools with OAuth login, token refresh, load balancing, quota cooldowns and usage tracking; an AI gateway for Vercel AI Gateway, Mistral and OpenAI-compatible servers; policy-bound client keys in OpenBao with model precedence and aliases; T3Code hubs compatibility; and OpenTelemetry traces, logs and metrics.
- Translate between Anthropic Messages, OpenAI Responses and Chat Completions, so models are reachable on all three endpoints: Claude Code can use Codex and Chat-only models, and Codex CLI can use Anthropic models from Vercel or OpenCode Go. Claude subscription accounts stay on the Claude Code API path (`/v1/messages`) only. Requests, JSON and streamed responses, tool calls and results, reasoning, images, usage and errors are translated; `GET /v1/models` and `xolotl policies view` mark translated endpoints.
