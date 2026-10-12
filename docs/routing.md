# Routing and policies

A client sends a model name. Xolotl turns it into one or more **targets** (`provider/model`) using the key's policy and the model catalog, and sends the request to the first one, translating it when the target's provider doesn't speak the endpoint's wire format.

## Model catalog

`src/catalog.rs` keeps the list of models each provider serves.

- **Discovery** runs at startup and every `catalog.refresh_secs` (default 600 s), on `POST /admin/catalog/refresh` (`xolotl models --refresh`), and after an account login or import.
  - Codex, Claude and OpenCode Go are asked using one of their accounts. For Codex, `catalog.codex_client_version` is sent, because the backend filters models by client version.
  - Gateway providers are asked via `GET /v1/models`.
- **Static models** from `[catalog.static_models]` are added on top. Before the first discovery completes, the catalog holds only these, so routing works from the start.
- If discovery fails for a provider, its previous list is kept.

## Wire formats

There are three LLM wire formats: Anthropic Messages (`/v1/messages`), OpenAI Responses (`/v1/responses`) and OpenAI Chat (`/v1/chat/completions`). Each provider speaks some of them natively:

| Provider | Formats |
|---|---|
| `claude` | Messages |
| `codex` | Responses |
| `opencode-go` | per model: a fixed list speaks Messages, another Responses, the rest Chat |
| `vercel` kind | all three |
| other gateway providers | Chat |

`[catalog.wires]` overrides this for a provider (`llamacpp = ["chat", "messages"]`) or a single model (`"opencode-go/new-model" = ["messages"]`). Claude subscription accounts can't be overridden: a `claude` or `claude/<model>` key fails config validation.

`ModelCatalog::route` decides how a target is reached on an endpoint:

- **Native** when its provider speaks the endpoint's format. Native always wins: a provider that speaks several formats is never translated.
- **Translated** otherwise, from the best format the provider speaks: Responses, then Messages, then Chat. Responses and Messages carry reasoning between turns; Chat does not.
- **Not at all** for `/v1/messages/count_tokens` and `/v1/responses/compact`. Only the three main endpoints are translated.
- **Not at all** for Claude subscription accounts (`claude`) on any endpoint but their own: they serve only the Claude Code API path, `/v1/messages`. Anthropic models on other providers (Vercel, OpenCode Go, a `[catalog.wires]` override) are translated like any other. A policy that wants a Claude model on every endpoint can alias it to both, e.g. `["claude/claude-haiku-5-5", "opencode-go/claude-haiku-5-5"]`; targets that can't serve the endpoint are skipped.

`GET /v1/models` and `xolotl policies view` list, for every name, the endpoints its first target is served on natively (`endpoints`) and through translation (`translated_endpoints`).

Other `POST /v1/*` endpoints (embeddings, OCR, …) have no wire format. They are never checked, and only go to gateway providers, not subscription pools.

## Translation

`src/translate/` turns a request in the client's format into a format-neutral request (`ir.rs`), encodes that in the upstream's format, and turns the upstream response back into the client's format. Each format has one module (`messages/`, `responses/`, `chat/`): `mod.rs` decodes and encodes requests, `response.rs` decodes and encodes responses and streams. Streams are decoded into events (block start, delta, stop, finish, error) and re-encoded as they arrive.

| Concern | Behaviour |
|---|---|
| Streaming | The client gets what it asked for. A streamed upstream answer is collected into one JSON body for a non-streaming client (Codex always streams); a JSON answer is replayed as a stream for a streaming client. |
| System prompts | Messages `system`, Responses `instructions` and system/developer messages, and Chat system/developer messages are all system text. Codex gets it as a leading developer message. Claude Code's `x-anthropic-billing-header` block is dropped. |
| Tools | Function tools map one to one. Responses `custom` tools (Codex's `apply_patch`) become a function with one string argument, `input`, with the grammar in the description; calls go back to the client as `custom_tool_call`. Responses namespace tools are flattened to `<namespace>__<name>` (as CLIProxyAPI does), and calls go back with `namespace` and `name`. Hosted and server tools (web search, file search, code execution, …) are dropped. Names longer than 64 characters or with other characters than `[A-Za-z0-9_-]` are shortened for OpenAI upstreams and restored in the response. |
| Parallel tool calls | Chat streams may interleave the argument deltas of parallel calls, so calls from a Chat provider are assembled per `index` and sent on whole when the choice finishes. |
| Tool results | Messages `tool_result`, Responses `function_call_output` and Chat `tool` messages map to each other. Images in a tool result go to Chat as a user message after the tool message. |
| Reasoning | Upstream reasoning is returned as thinking blocks, reasoning items or `reasoning_content`. Its replay state (Anthropic signature, OpenAI encrypted content) travels through the client's slot for the other kind, tagged `xolotl.openai.` or `xolotl.anthropic.`, so a later turn can replay it to the provider that issued it. Reasoning a provider didn't issue is dropped. |
| Reasoning effort | Chat `reasoning_effort`, Responses `reasoning.effort` and Messages `thinking` / `output_config.effort` map to each other. A Messages provider gets a `budget_tokens` thinking budget. Thinking is left off when the last assistant turn called tools without a signed thinking block (Claude rejects that), and when a tool is forced. |
| Images | Base64 and URL images, in messages and tool results. |
| Usage | Recorded from the upstream response before translation, so the request log and metrics are unchanged. The client gets it in its own shape; Chat streams include it when `stream_options.include_usage` is set. |
| Errors | Upstream errors keep their status and headers (`retry-after`) and get the client's error envelope. A 2xx JSON body that reports a failure (a `failed` Responses object, an error body) is an error too: 502 for a non-streaming client, an error event for a streaming one. A stream that fails or ends early ends with an error event. |
| Rejected | Responses `previous_response_id`, and structured output (`response_format` / `text.format`) towards Messages, get a 400. |

## Policies

A policy (`src/policy.rs`) is stored in OpenBao at `policies/<name>`, and every client key belongs to one. Fields are described in the project README; the example is [`examples/policies/devcontainer.toml`](../examples/policies/devcontainer.toml).

`Policy::resolve` builds the policy's **view**: client-visible name → list of targets.

1. For every catalog model, keep it if the provider is allowed and the model passes `allow_models` / `deny_models`. The globs match either `model` or `provider/model`, and deny wins.
2. Group the remaining models by plain name. When several providers serve the same name, one wins it:
   1. the provider in `winners[model]`, if it serves the model;
   2. otherwise, the earliest provider in `precedence`;
   3. otherwise, the earliest in the default order: `claude`, `codex`, `opencode-go`;
   4. otherwise, alphabetical.

   Providers left out of `precedence` rank after every provider in it. The losing providers don't get the plain name.
3. Add the aliases. Each one maps a name to one target or an ordered list of targets. An alias overrides a plain name of the same spelling. Targets on providers the policy doesn't allow are dropped.

`View::lookup` handles a requested name:
- a name in the view returns its targets;
- otherwise, `provider/model` returns that single target, if the provider exists and the policy allows the model. This form works even for a model that lost its plain name.

The resolved view for any policy can be inspected with `xolotl policies view <name>` (`GET /admin/policies/<name>/view`).

### Example

The catalog has `claude-haiku-5-5` on `claude` and on `opencode-go`. With the default order, a key sees `claude-haiku-5-5` → `claude/claude-haiku-5-5`. The OpenCode Go copy is still reachable as `opencode-go/claude-haiku-5-5`, or under a friendlier name through an alias:

```toml
[aliases]
haiku-go = "opencode-go/claude-haiku-5-5"
fast = ["codex/gpt-6-codex", "vercel/moonshotai/kimi-k3"]   # tried in order on 429/5xx
```

## LLM requests

For `/v1/messages`, `/v1/messages/count_tokens`, `/v1/responses`, `/v1/responses/compact`, `/v1/chat/completions` and other `POST /v1/*` (`route_json` in `src/server/mod.rs`):

1. Authorize with the `proxy` scope; parse the body and read `model`.
2. Look the name up in the view. If it's not found: 404.
3. Keep the targets the endpoint reaches, natively or through translation. If none (`count_tokens`, `compact`, or a Claude subscription account off `/v1/messages`): 400 naming the endpoints the model *is* served on.
4. Try each target in order. A 429 or 5xx moves on to the next target; the last target's response is returned. A translated target gets the request in its own format, and its response is translated back.
   - **Pool targets:** `model` is rewritten to the provider's name and the request goes to the pool ([Subscription accounts](subscriptions.md)).
   - **Gateway targets:** the provider's key is injected (Bearer, plus `x-api-key` on `/v1/messages`). For Mistral, a few OpenAI parameter spellings it rejects are rewritten.

Errors use the shape of the endpoint that was called: Anthropic-style on `/v1/messages`, OpenAI-style elsewhere. This holds for translated requests too.

`/backend-api/codex/responses` is accepted as an alias of `/v1/responses` for clients configured against the ChatGPT backend path. `GET /v1/responses` (Codex's WebSocket transport) returns 426, so Codex falls back to HTTP.

`GET /v1/models` lists the view's names with their native and translated endpoints, in Anthropic's list format when the request carries `anthropic-version` (Claude Code).

## Passthroughs

| Path | Behaviour |
|---|---|
| `/<provider>/<path>` | Any method, to a gateway provider's base URL. The client's credentials are stripped and the provider key is injected. JSON bodies are buffered so `model` can be checked against the policy; other bodies (multipart uploads) are streamed. |
| `/opencode-go/<path>` | Any method, to OpenCode Go through the account pool, so load balancing, failover and usage tracking still apply. The body is always buffered, because failover may resend it. |

Both require the policy to allow the provider.

Upstream redirects are never followed, on any route: a 3xx is relayed to the client as is, so provider credentials and account tokens never reach another origin.
