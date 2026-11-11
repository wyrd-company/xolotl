---
xolotl: minor
xolotl-docs: minor
---

Anthropic and OpenAI API accounts: the new provider kinds `anthropic` and `openai` take an API key (personal, project or service account) and serve the same endpoints as Claude and Codex subscription accounts, so a client can use either. An Anthropic account serves `/v1/messages` and `/v1/messages/count_tokens` natively, with the key in `x-api-key` and the subscription OAuth beta flag dropped, so Claude Code requests and their cache breakpoints reach the API unchanged; an OpenAI account serves `/v1/responses` and `/v1/chat/completions` natively. Alias a model to a subscription and an API account to move to the API account when the subscription is at its limit. In the config file, an Anthropic or OpenAI account can authenticate with workload identity federation (`workload_identity`) instead of a key: xolotl exchanges a JWT from a file for a service account access token and renews it before it expires. The Accounts page documents both.
