---
xolotl: minor
---

Translate between Anthropic Messages, OpenAI Responses and Chat Completions, so every model is reachable on all three endpoints: Claude Code can use Codex and Chat-only models, and Codex CLI can use Claude. Requests, JSON and streamed responses, tool calls and results, reasoning, images, usage and errors are translated; `GET /v1/models` and `xolotl policies view` mark translated endpoints.
