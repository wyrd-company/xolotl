---
xolotl: minor
---

Translate between Anthropic Messages, OpenAI Responses and Chat Completions, so models are reachable on all three endpoints: Claude Code can use Codex and Chat-only models, and Codex CLI can use Anthropic models from Vercel or OpenCode Go. Claude subscription accounts stay on the Claude Code API path (`/v1/messages`) only. Requests, JSON and streamed responses, tool calls and results, reasoning, images, usage and errors are translated; `GET /v1/models` and `xolotl policies view` mark translated endpoints.
