# Sessions and context

Every LLM request of a client key belongs to a **session**: one conversation. Xolotl records how full each session is, and once a session's prompt cache is worth keeping, it **locks** the session to the account and model that hold that cache. A session also **keeps** the models its conversations are on after the key's policy excludes them. `src/context/` holds the identity, the accounting, the lock and the conversations; the pool (`src/subscription/mod.rs`) and `route_json` (`src/server/mod.rs`) apply the lock, and `route_json` and the passthroughs apply the kept models.

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
| `target_unavailable` | The requested name no longer offers the locked target. A policy that excludes the locked model doesn't break the lock while the session keeps the model ([Models a session keeps](#models-a-session-keeps)) |

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
| A background request on another model never opens with its conversation's first user turn ([Clients](#clients): none did) | The exported exchanges of one `session_key`: a request on another model with the conversation's first user turn, followed by the conversation on its own model again |

## Models a session keeps

A model exclusion is a maintenance control, not a security boundary: it stops new uptake of a model, and a running session never breaks because of it (decided by the operator on 2026-11-24). So a model is checked against the key's policy when a session first uses it: when a conversation of the session starts with it, or switches to it. Once a request on it succeeds, the session keeps it while one of its conversations is on it. A later exclusion by a rule, its `models` switch or `deny_other_models` doesn't stop the session's requests to it, from that conversation or from a new one, such as a subagent or what follows a compaction. Another session can't start with it. Once a conversation of the session switches away from it, the session can't switch back, also after a compaction rewrote the conversation's opening, unless another conversation has used the model since (decided by the operator on 2026-11-24).

### Conversations

A session carries more than one conversation: the main one, subagents, and background requests on other models ([Clients](#clients)). By model name alone, a background request between two turns on a model looks like a switch away and back. So each conversation (`context::conversation`) records the target it is on, told apart within its session by its **opening**: a digest of the first user turn, without the cache markers that clients move as a conversation grows. Neither the model nor the system prompt is part of it: Claude Code names its model in the system prompt. The state also carries the key's **issuance**, its id and the time it was issued, so a key issued again under a revoked key's name starts with nothing, while a rotation keeps it. Revoking a key and issuing it again therefore cuts what its sessions keep.

`session_conversations` holds one row per conversation. Every request of a session the client names reads the targets of all its conversations. A `content` session is xolotl's guess from the system prompt and first user turn, so there only a request that **continues** a conversation reads them: one that carries an earlier assistant message or a Responses output item. A script that repeats a one-shot request doesn't keep an excluded model.

### Lookup and moves

`route_json`, counting tokens and the passthroughs read the session's targets before the policy, and look the name up in a view resolved as if the policy still allowed them (`Policy::resolve_keeping`). A kept model gets back the plain name or rename the exclusion took from it, ahead of another provider that serves the same name, and its `provider/model` form works again. Its lock, if it has one, holds.

A request whose answer completes moves its conversation to the target that served it (`Conversation::moves_with`). The answer completes when it is a 2xx response and either:

- a stream reaches its final event with no error event. The final event is `message_stop`, `response.completed` or `response.incomplete`, a Chat `finish_reason`, or `[DONE]`. An error event is Messages or Responses `error`, or a Chat chunk with an `error`.
- a JSON body is not an error.

A body is a stream when its `Content-Type` says so, or when it doesn't start as JSON. A provider can answer 200 and then send only an error: that moves nothing, and the session goes on with the model it was on. The end of the answer is held until the move is written: a stream from its final event on, or a JSON body's last chunk. The client can't see the answer complete before its next request would see the move. A client that leaves before the end moves nothing.

Every request takes a sequence number when it arrives: microseconds since the epoch, raised past the last one. A move applies only over an older one, so a request that started before a switch and finishes after it doesn't move the conversation back. Counting tokens moves nothing.

- A request keeps its target only if the policy allows the model by name, or the session keeps it already. A target only an alias reaches still moves the conversation, which then keeps nothing, so that the session can't reach that target later under a name the policy refuses. Aliases ignore model rules, so an exclusion never stops them anyway.
- A model the policy never allowed never gets into `session_conversations`, so it stays refused in every session.
- A provider that `providers` no longer allows is not kept, and neither is an account: those apply to every session at once, and an account's lock breaks with `policy`.
- A model the catalog no longer lists loses its plain name whatever the session keeps. Xolotl can't keep a model working that its provider stops serving.
- When the state can't be read, the policy decides alone. When a move can't be written, the conversation stays where it was.
- In a locked session, a switch to another name is a request for another name: it leaves the lock alone, and still moves the conversation. The lock then stays on a model no request reaches, and goes with the session.
- A switch records that the session left the model it moved off (`session_left`). From then on, another conversation's row keeps that model only if the conversation has used it since. The one before a compaction, a finished subagent, and turn summaries all went quiet before the switch, so they don't bring the model back. A subagent still at work on it keeps it for the session, and while it does, switching back works. A conversation's own model always counts, so that it is never refused because another one left the model.

Rows go at `retention_days` after their last move, like the sessions.

### Clients

Measured on 2026-11-24 with each client in a clean container against a mock provider, over several turns, a model switch and a compaction. `src/server/tests/context/models/clients.rs` replays what xolotl reads from each.

| Client | Session | First user turn | Later requests | Other conversations | After a compaction |
|---|---|---|---|---|---|
| Claude Code 2.1.330 | `x-claude-code-session-id` | Holds across turns and a switch | Carry earlier output | A subagent: same session id, `x-claude-code-agent-id`, its own first turn, no earlier output | A new first turn, earlier output |
| Codex 0.171.0 | `session-id`, `thread-id`; `prompt_cache_key` is the thread | Holds | Carry earlier output | None seen | The history is rebuilt: a new first turn, no earlier output |
| Grok 1.0.92 | `x-grok-session-id`, `x-grok-conv-id` | Holds | Carry earlier output | Turn summaries (`x-grok-conv-id: turn-summary-…`, their own first turn, on the conversation's model), title refreshes (the conversation on its model) | The same first turn, no earlier output |
| OpenCode 1.19.6 | `x-opencode-session-id`, `x-session-id` | Holds | Carry earlier output | Title generation on the small model, its own first turn | A new first turn |

So a rule that kept a model only for the conversation that used it, or only for requests that carry earlier output, would refuse every client after a compaction, and Claude Code's subagents. And a rule that kept the session on every model its conversations are on would let every client switch back after a compaction, and Grok after any turn, as its turn summaries run on the conversation's model.

Grok also sends one request at startup with empty session headers, on its default model rather than the selected one. It is a `content` one-shot, so it never keeps a model: excluding Grok's default model refuses it, before and after this feature.

The measurements ran without a real model, so no client called tools on its own, and Claude Code's subagent was started by a scripted tool call. They don't show how often background requests come in a long interactive session.

Options not chosen:

| Option | Why not |
|---|---|
| The model of the session's last request | A background request on another model would end the conversation's model, and an excluded model would fail at the conversation's next turn |
| Every model the session ever used | A conversation could switch back to an excluded model |
| A request on another model whose prompt is as large as the conversation is a switch | A subagent on another model can grow as large as its parent |
| Keep a model only for the conversation that used it, or only for requests that carry earlier output | Every measured client starts a new conversation, or leaves no earlier output, at a compaction, and Claude Code subagents start new ones: the running session would break |
| Link a compacted conversation to the one it summarizes | Only text the client writes links them, so the link would be a guess at each client's format. Recording when the session left a model needs no link: the conversation before a compaction is quiet by the time its successor switches |
| Keep every model a conversation of the session is on | A compacted conversation could switch back through the row of the one before it, and a Grok session through its turn summaries |
| Retire a quiet conversation's row when another switches away | A subagent that is between requests is quiet too; counting its own model whatever the session left keeps it at work |

## Tool usage

`TapStream` reads tool calls from the upstream response on the way through: Messages `tool_use` and `server_tool_use` blocks, Responses `function_call` and `custom_tool_call` items, and Chat `tool_calls`. Only successful responses count. `tool_usage` holds a count and a last use per client key and tool name; `GET /admin/tools` joins it with `mcp_call_log`, which already records `/mcp` calls. Names are as the upstream sent them, so a name shortened for a translated request stays shortened.
