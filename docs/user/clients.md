---
title: Clients
order: 6
product: xolotl
---

Clients use xolotl like a provider API. They send their client key and a model
name the key's policy exposes. The examples use `http://xolotl:8317` as the
xolotl URL and `XOLOTL_API_KEY` for the client key (see
[Issue a client key](policies-and-keys#issue-a-client-key)).

Xolotl accepts the key in `Authorization` (Bearer or bare), `x-api-key` or
`x-management-key`.

## Claude Code

```sh
export ANTHROPIC_BASE_URL=http://xolotl:8317
export ANTHROPIC_AUTH_TOKEN=$XOLOTL_API_KEY      # or ANTHROPIC_API_KEY
```

Claude Code's model list comes from `GET /v1/models`, in Anthropic's format,
with model descriptions, context limits and API-equivalent list prices. Models
with a known input limit of at least one million tokens receive a `[1m]` hint.
Both the hinted and canonical names route and account as the same model. Alias
hints require every fallback target to meet the limit. A 200K–999K window stays
untagged so Claude Code can compact early. Unknown models receive no hint.
Claude Code discovery filters for names containing `claude` or `anthropic`;
select other names with `--model` or `/model <name>`.

For an exact custom-model window, set `CLAUDE_CODE_MAX_CONTEXT_TOKENS` in the
client session. Xolotl cannot set this environment value through the API. See
[Claude Code model configuration](https://code.claude.com/docs/en/model-config#correct-the-window-for-a-gateway-or-custom-model-id).
Upstream context-limit errors become `prompt is too long` so Claude Code can
compact and retry. Translated token counts are approximate and labeled as such.

## Codex CLI

In `~/.codex/config.toml`:

```toml
model_provider = "xolotl"

[model_providers.xolotl]
name = "xolotl"
base_url = "http://xolotl:8317/v1"
env_key = "XOLOTL_API_KEY"
wire_api = "responses"
model_catalog_url = "http://xolotl:8317/backend-api/codex/models"
```

The Codex-native catalog uses the same client key as inference. It is filtered
to the key's policy and includes aliases as independent entries. Native Codex
metadata passes through; other models use LiteLLM windows and configured
capabilities. Configure unknown models with `[catalog.metadata]`.

Instead of `env_key`, Codex can obtain the key from a helper:

```toml
[model_providers.xolotl.auth]
command = "secrets-helper xolotl-key"
```

The helper prints the client key. Codex sends provider authentication when it
fetches `model_catalog_url`; no public catalog is required. See the
[official configuration reference](https://learn.chatgpt.com/docs/config-file/config-reference).

## Grok CLI

The Grok CLI runs `auth_provider_command` and sends what it prints as the bearer
token. Point it at a command that prints the client key, and declare a model
that uses the Responses backend:

```toml
[auth]
auth_provider_command = "secrets-helper xolotl-key"   # prints $XOLOTL_API_KEY

[model."grok-build"]
base_url = "http://xolotl:8317/v1"
model = "grok-build"
api_backend = "responses"
```

## OpenAI-compatible SDKs

Use base URL `http://xolotl:8317/v1`, the client key as the API key, and any
model name the policy exposes.

```sh
curl http://xolotl:8317/v1/chat/completions \
  -H "Authorization: Bearer $XOLOTL_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"model": "fast", "messages": [{"role": "user", "content": "Hello"}]}'
```

## MCP clients

Add a Streamable HTTP server with URL `http://xolotl:8317/mcp` and header
`Authorization: Bearer $XOLOTL_API_KEY`. The key's policy needs the `mcp`
scope. For Claude Code:

```sh
claude mcp add --transport http xolotl http://xolotl:8317/mcp \
  --header "Authorization: Bearer $XOLOTL_API_KEY"
```

See [MCP gateway](mcp-gateway).

## T3 Code

Add a CLIProxyAPI usage source with URL `http://xolotl:8317` and a key whose
policy has the `management` scope. T3 Code then shows the usage of your Codex
and Claude accounts, and the Codex reset credits.

T3 Code redeems a Codex reset credit itself: it sends the redemption request to
Codex through xolotl's `api-call` endpoint, which adds the account's token and
only reaches the account's own provider. Then it calls `reset-quota`, and xolotl
clears that account's cooldown and last error and polls its usage again.
`reset-quota` alone sends nothing to the provider.

## Endpoints

| Request | Goes to |
|---|---|
| `POST /v1/messages`, `/v1/responses`, `/v1/chat/completions`, other `POST /v1/*` | The target the policy gives for `model` |
| `GET /backend-api/codex/models` | Policy-filtered Codex-native metadata |
| `GET /v1/models` | The policy's model names and their endpoints |
| `/<provider>/<path>` | Passthrough to a gateway provider, with its key |
| `/opencode-go/<path>` | Passthrough to OpenCode Go through the account pool |
| `/mcp` | The MCP gateway |
| `/v0/management/...` | T3 Code hubs |
| `/admin/...` | The CLI and the web UI |
| `/` | The web UI |

`/backend-api/codex/responses` is the same as `/v1/responses`, for clients set
up against the ChatGPT backend path.

### Passthroughs

A passthrough sends any method and path to the provider. Xolotl removes the
client's credentials and adds the provider's. Use them for endpoints that are
not chat, for example:

```sh
curl http://xolotl:8317/mistral/v1/ocr -H "Authorization: Bearer $XOLOTL_API_KEY" ...
curl http://xolotl:8317/vercel/v1/credits -H "Authorization: Bearer $XOLOTL_API_KEY"
```

The policy must allow the provider. When the body is JSON, the policy must also
allow its `model`.

## Wire formats and translation

There are three LLM formats: Anthropic Messages (`/v1/messages`), OpenAI
Responses (`/v1/responses`) and OpenAI Chat Completions
(`/v1/chat/completions`). Every model is reachable on all three. When the
provider does not speak the endpoint's format, xolotl translates the request and
the response, streams included.

| Provider | Speaks natively |
|---|---|
| `claude` | Messages |
| `codex`, `grok` | Responses |
| `opencode-go` | Messages, Responses or Chat, depending on the model |
| `vercel` kind | All three |
| Other gateway providers | Chat |

Change this with [`[catalog.wires]`](configuration#catalog).

Exceptions:

- **Claude subscription accounts** serve only `/v1/messages`. To use a Claude
  model on every endpoint, alias it to a second provider:
  `["claude/claude-haiku-5-5", "opencode-go/claude-haiku-5-5"]`. Xolotl skips
  targets that can't serve the endpoint.
- **`/v1/messages/count_tokens`** calls native Messages providers or returns a
  local UTF-8 byte estimate for translated models (`estimated: true` and
  `x-xolotl-token-count` header). Tools and system prompts are included; image
  and tokenizer costs are approximate. **`/v1/responses/compact`** is native only.
- A model that can't serve the endpoint gets a 400 that names the endpoints it
  is served on.

Translation drops what has no match in the other format: hosted tools such as
web search, and attachments that are not images. See
[Known limitations](limitations).

## Errors

Errors have the shape of the endpoint you called: Anthropic-style on
`/v1/messages`, OpenAI-style on the others.

| Status | Meaning |
|---|---|
| 401 | No key, or the key is unknown, expired, disabled or revoked |
| 403 | The key lacks the scope, or a passthrough's provider or model is not allowed |
| 404 | The policy exposes no such model name |
| 400 | The model is not served on this endpoint |
| 429 | Every account is cooling down, or the key is over a [rate limit](policies-and-keys#rate-limits). `Retry-After` gives the seconds to wait |
