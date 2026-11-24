# Sessions and context

Every LLM request of a client key belongs to a **session**: one conversation. Xolotl records how full each session is, and once a session's prompt cache is worth keeping, it **locks** the session to the account and model that hold that cache. A session also **keeps** the models it has used after the key's policy excludes them. `src/context/` holds the identity, the accounting, the lock and the models a session keeps; the pool (`src/subscription/mod.rs`) and `route_json` (`src/server/mod.rs`) apply the lock, and `route_json` and the passthroughs apply the kept models.

## Session identity

`context::resolve` returns a key from the first of these it finds:

| Source | Read from |
|---|---|
| `claude-code` | `x-claude-code-session-id`, or `session_id` in the JSON of `metadata.user_id` |
| `codex` | `session_id`, `session-id` or `thread-id` |
| `grok` | `x-grok-session-id` or `x-grok-conv-id` |
| `opencode` | `x-opencode-session`, or `x-opencode-session-id`, which the OpenCode client sends to a provider configured in it |
| `prompt-cache-key` | `prompt_cache_key` in the body |
| `content` | A digest of the system prompt and the first user turn |

The key is a truncated SHA-256 of the source and the value, so the database holds neither a client's identifier nor its prompt. A session is keyed by client key plus session key: two keys never share one.

Sessions exist on the three wire formats, including through a passthrough. `count_tokens` is not a turn, other endpoints have no conversation, and the bootstrap admin token has no client key, so none of these has a session.

The `content` source treats two conversations with the same system prompt and first turn as one, and a client that rewrites its first turn starts a new one. A client that sends an identifier has neither limit.

## Accounting

Reading a request for its session changes nothing that is forwarded, and adds no upstream call: identity and the estimate read the parsed body, and the rest is SQLite.

A request's size is the upstream's reported input tokens, cached and uncached, when the response has them, and otherwise `metadata::estimate_tokens`. Once an upstream has reported a session's size, a later estimate does not replace it, because the estimate reads high.

`Accounting::finish` writes `session_key`, `context_tokens` and `context_window` on the request log row. A successful request also updates the `sessions` row: `current_tokens`, `peak_tokens`, `turns`, and the prompt and cached totals that give the cache hit ratio. The window is `catalog.metadata` override, then Codex's reported window, then the LiteLLM metadata (`ModelCatalog::context_window`). Sessions and tool counts are pruned with the request log, by `last_seen` and `last_used`.

Metrics carry provider and model, never the session key, so that their cardinality stays bounded.

## Context lock

A successful request whose session has reached `sessions.lock_tokens` (default 60 000, 0 disables) forms a lock on what served it: the requested name, the provider and model, and the account when there is one. Passthroughs to a provider account are counted but not locked, as nothing routes them.

While the lock holds, a request for the locked name goes to the locked target and account, ahead of alias order, session affinity, `soft_limit_percent` and the strategy. It breaks, with the reason recorded on the session, as a metric and in the log, when:

| Reason | When |
|---|---|
| `cooldown` | The account is cooling down, or this response cooled it (an exhausted quota; a 429 with `Retry-After`) |
| `account_lost` | The account was removed or disabled, needs a new login, or its credential is unavailable |
| `policy` | The key's policy no longer allows the account |
| `limit` | The account is at a limit of the key's policy, or the provider is and the alias has another target |
| `upstream_errors` | `subscription.max_attempts` consecutive failed requests on the lock |
| `target_unavailable` | The requested name no longer offers the locked target. A policy that excludes the locked model doesn't break the lock, as the session keeps the model ([Models a session keeps](#models-a-session-keeps)) |

After a break the request routes normally, and a later success past the threshold locks the session where it landed. A lock's generation changes each time one forms. Releasing a lock or counting a failure names the generation it read, so a stale request can't touch a newer lock, even on the same target. A policy that can use no account of the locked pool breaks the lock before routing, as that pool's 403 would stop an alias from moving on. Idle time and compaction release nothing: the lock goes with its session row at `retention_days`, and holding it costs only the account choice.

### A failure on the lock

A 5xx, a transport error or a 429 without a cooldown (on a gateway target, any 429) goes back to the client and the lock stays, because failing over would throw away the cache for what may be a short failure. A success ends the run. The `max_attempts`-th failure in a row breaks the lock and fails over in the same request, so that an account that fails every request cannot hold a session. The attempt on the lock is outside that request's budget of `max_attempts` accounts, so that this holds at `max_attempts = 1`.

This narrows failover for a session past the threshold: its client sees errors that it would not see otherwise. Options not chosen: holding with no limit, which strands a session on a broken account until an operator disables it; failing over as before, where one 529 costs the whole cache.

### Another name in a locked session

The target pin belongs to the requested name. A request for another name routes normally, neither breaks nor moves the lock, and does not change the session's size, peak or model. If its target is on the locked provider, it prefers the locked account; if that account is unavailable or fails, the request fails over as usual and the lock is untouched.

Options not chosen: a hard pin, which silently replaces the model the client named; treating another name as a break, which would break the lock on each background request.

### Assumptions

These client behaviors shaped the rules above and have not been measured:

| Assumption | Falsified by |
|---|---|
| A client retries a 5xx, so a short failure on a lock costs a retry and not a failed turn | `request_log`: a row with `status` 5xx and a `session_key` with no later row for that `session_key` |
| Claude Code sends background requests on a cheaper model under its session id, in an interactive session (none came in the measured print mode) | `request_log`: no `session_key` from a `claude-code` session with more than one `model` |

## Models a session keeps

A model exclusion is a maintenance control, not a security boundary. It stops new uptake of a model, and a running session never breaks because of it. The operator's ruling (2026-11-24): "Once a session has used a model, it can use that model in any capacity. Even go back."

So a model is checked against the key's policy when a session first uses it. Once an answer from it completes, the session keeps it. A later exclusion by a rule, its `models` switch or `deny_other_models` doesn't stop the session's requests to it. That holds from any of its conversations, after switching away and back, across compactions, and for background requests and subagents. A session that never used the model can't start with it.

`session_models` holds the models each session has used, per client key issuance: the key's id and the time it was issued. A key issued again under a revoked key's name starts with nothing. A rotation keeps the issuance and its sessions. Revoking a key and issuing it again therefore cuts what its sessions keep.

### Lookup and use

`route_json`, counting tokens and the passthroughs read the session's models before the policy (`context::models`), and look the name up in a view resolved as if the policy still allowed them (`Policy::resolve_keeping`). A kept model gets back the plain name or rename the exclusion took from it, ahead of another provider that serves the same name, and its `provider/model` form works again. Its lock, if it has one, holds.

Every request of a session the client names reads them. A `content` session is xolotl's guess from the system prompt and first user turn. There, only a request that carries an earlier assistant message or a Responses output item reads them, so a script that repeats a one-shot request keeps nothing.

A request records its model once its answer completes (`SessionModels::uses`). The answer completes when it is a 2xx response and either:

- a stream reaches its final event with no error event. The final event is `message_stop`, `response.completed` or `response.incomplete`, a Chat `finish_reason`, or `[DONE]`. An error event is Messages or Responses `error`, or a Chat chunk with an `error`.
- a JSON body is not an error.

A body is a stream when its `Content-Type` says so, or when it doesn't start as JSON. A provider can answer 200 and then send only an error: that records nothing. The end of the answer is held until the use is written: a stream from its final event on, or a JSON body's last chunk. So the client can't see the answer complete before its next request would see it. A client that leaves before the end records nothing. Counting tokens records nothing.

- A request records its target only if the policy allows the model by name, or the session keeps it already. A target only an alias reaches is never recorded, so the session can't reach it later under a name the policy refuses. Aliases ignore model rules, so an exclusion never stops them anyway.
- A model the policy never allowed never gets into `session_models`, so it stays refused in every session.
- A provider that `providers` no longer allows is not kept, and neither is an account. Those apply to every session at once, and an account's lock breaks with `policy`.
- A model the catalog no longer lists loses its plain name whatever the session keeps. Xolotl can't keep a model working that its provider stops serving.
- When the models can't be read, the policy decides alone. When a use can't be written, the session doesn't keep that model.

Rows go at `retention_days` after their last use, like the sessions.

### Clients

Measured on 2026-11-24 with each client in a clean container against a mock provider, over several turns, a model switch and a compaction. `src/server/tests/context/models/clients.rs` replays what xolotl reads from each.

| Client | Session | Other requests in the session | After a compaction |
|---|---|---|---|
| Claude Code 2.1.330 | `x-claude-code-session-id` | A subagent: same session id, `x-claude-code-agent-id` | A new first user turn |
| Codex 0.171.0 | `session-id`, `thread-id`; `prompt_cache_key` is the thread | None seen | A new first user turn, no earlier output |
| Grok 1.0.92 | `x-grok-session-id`, `x-grok-conv-id` | Turn summaries and title refreshes, on the conversation's model | The same first user turn, no earlier output |
| OpenCode 1.19.6 | `x-opencode-session-id`, `x-session-id` | Title generation on the small model | A new first user turn |

Every client keeps its session id across turns, model switches and compactions, so the session is what keeps a model. A rule tied to one conversation, or to requests that carry earlier output, would refuse each of them after a compaction, and Claude Code's subagents.

Grok also sends one request at startup with empty session headers, on its default model rather than the selected one. It is a `content` one-shot, so it never keeps a model, and excluding Grok's default model refuses it.

The measurements ran without a real model, so no client called tools on its own, and Claude Code's subagent was started by a scripted tool call.

Options not chosen:

| Option | Why not |
|---|---|
| Keep only the model the session's conversation is on, so that switching back is refused | Superseded by the operator's ruling. It also needed client-specific guesses: which requests are background ones, and which conversation a compaction continues |
| Keep a model only for the conversation that used it, or only for requests that carry earlier output | Every measured client starts a new conversation, or leaves no earlier output, at a compaction, and Claude Code subagents start new ones: the running session would break |
| Record a model on its 2xx headers | A provider that answers 200 and then only an error event would make a model the session never got an answer from its own |

## Tool usage

`TapStream` reads tool calls from the upstream response on the way through: Messages `tool_use` and `server_tool_use` blocks, Responses `function_call` and `custom_tool_call` items, and Chat `tool_calls`. Only successful responses count. `tool_usage` holds a count and a last use per client key and tool name; `GET /admin/tools` joins it with `mcp_call_log`, which already records `/mcp` calls. Names are as the upstream sent them, so a name shortened for a translated request stays shortened.
