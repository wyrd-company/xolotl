# Routing and policies

A client sends a model name. Xolotl turns it into one or more **targets** (`provider/model`) using the key's policy and the model catalog, keeps the targets that speak the endpoint's wire format, and sends the request to the first one.

## Model catalog

`src/catalog.rs` keeps the list of models each provider serves.

- **Discovery** runs at startup and every `catalog.refresh_secs` (default 600 s), on `POST /admin/catalog/refresh` (`xolotl models --refresh`), and after an account login or import.
  - Codex, Claude and OpenCode Go are asked using one of their accounts. For Codex, `catalog.codex_client_version` is sent, because the backend filters models by client version.
  - Gateway providers are asked via `GET /v1/models`.
- **Static models** from `[catalog.static_models]` are added on top. Before the first discovery completes, the catalog holds only these, so routing works from the start.
- If discovery fails for a provider, its previous list is kept.

## Wire formats

There are three LLM wire formats: Anthropic Messages (`/v1/messages`), OpenAI Responses (`/v1/responses`) and OpenAI Chat (`/v1/chat/completions`). Xolotl does **not** translate between them ([#2](https://github.com/wyrd-company/xolotl/issues/2)), so a model is only reachable on endpoints its provider speaks:

| Provider | Formats |
|---|---|
| `claude` | Messages |
| `codex` | Responses |
| `opencode-go` | per model: a fixed list speaks Messages, another Responses, the rest Chat |
| `vercel` kind | all three |
| other gateway providers | Chat |

`[catalog.wires]` overrides this for a provider (`llamacpp = ["chat", "messages"]`) or a single model (`"opencode-go/new-model" = ["messages"]`).

Other `POST /v1/*` endpoints (embeddings, OCR, …) have no wire format. They are never checked, and only go to gateway providers, not subscription pools.

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
3. Keep the targets whose format matches the endpoint. If none match: 400 naming the endpoint the model *is* served on.
4. Try each target in order. A 429 or 5xx moves on to the next target; the last target's response is returned as is.
   - **Pool targets:** `model` is rewritten to the provider's name and the request goes to the pool ([Subscription accounts](subscriptions.md)).
   - **Gateway targets:** the provider's key is injected (Bearer, plus `x-api-key` on `/v1/messages`). For Mistral, a few OpenAI parameter spellings it rejects are rewritten.

Errors use the shape of the endpoint that was called: Anthropic-style on `/v1/messages`, OpenAI-style elsewhere.

`/backend-api/codex/responses` is accepted as an alias of `/v1/responses` for clients configured against the ChatGPT backend path. `GET /v1/responses` (Codex's WebSocket transport) returns 426, so Codex falls back to HTTP.

`GET /v1/models` lists the view's names. When the request carries `anthropic-version` (Claude Code), only names reachable over Messages are listed, in Anthropic's list format.

## Passthroughs

| Path | Behaviour |
|---|---|
| `/<provider>/<path>` | Any method, to a gateway provider's base URL. The client's credentials are stripped and the provider key is injected. JSON bodies are buffered so `model` can be checked against the policy; other bodies (multipart uploads) are streamed. |
| `/opencode-go/<path>` | Any method, to OpenCode Go through the account pool, so load balancing, failover and usage tracking still apply. The body is always buffered, because failover may resend it. |

Both require the policy to allow the provider.

Upstream redirects are never followed, on any route: a 3xx is relayed to the client as is, so provider credentials and account tokens never reach another origin.
