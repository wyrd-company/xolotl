# Sessions and context

Every LLM request of a client key belongs to a **session**: one conversation. Xolotl records how full each session is, and once a session's prompt cache is worth keeping, it **locks** the session to the account and model that hold that cache. Each conversation of a session also **keeps** the model it is on after the key's policy excludes it. `src/context/` holds the identity, the accounting, the lock and the conversations; the pool (`src/subscription/mod.rs`) and `route_json` (`src/server/mod.rs`) apply the lock, and `route_json` and the passthroughs apply the kept models.

## Session identity

`context::resolve` returns a key from the first of these it finds:

| Source | Read from |
|---|---|
| `claude-code` | `x-claude-code-session-id`, or `session_id` in the JSON of `metadata.user_id` |
| `codex` | `session_id`, `session-id` or `thread-id` |
| `grok` | `x-grok-session-id` or `x-grok-conv-id` |
| `opencode` | `x-opencode-session` |
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
| `target_unavailable` | The requested name no longer offers the locked target. A policy that excludes the locked model doesn't break the lock while the conversation keeps the model ([Models a session keeps](#models-a-session-keeps)) |

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
| Claude Code sends background requests on a cheaper model under its session id | `request_log`: no `session_key` from a `claude-code` session with more than one `model` |
| Claude Code sends a subagent's requests under its session id, on the subagent's model | The exported exchanges of one `claude-code` `session_key`: no two first user turns on two models |
| A client keeps a conversation's first user turn as the conversation grows, apart from cache markers | The exported exchanges of one `session_key`: two requests of one conversation on one model whose first user turns differ |
| A background request or a subagent doesn't open with the conversation's first user turn | The exported exchanges of one `session_key`: a request on another model with the conversation's first user turn, followed by the conversation on its own model again |
| Every request after a conversation's first carries an earlier assistant message or output item | The exported exchanges of one conversation: a later request with no assistant message and no output item |

## Models a session keeps

A model is checked against the key's policy when a conversation first uses it: when the conversation starts with it, or switches to it. Once a request on it succeeds, the conversation is **on** the model and keeps it. A later exclusion by a rule, its `models` switch or `deny_other_models` doesn't stop that conversation's requests to it. A new conversation can't start with it, in a new session or in the same one, and a conversation that switched away can't switch back.

### Conversations

A session can carry more than one conversation: Claude Code is taken to send background requests and subagents under its session id, on other models ([Assumptions](#assumptions)). By model name alone, a background request between two turns on a model looks like a switch away and back. So the model belongs to one conversation (`context::conversation`), told apart by:

- the client key's **issuance**: its id and the time it was issued. A key issued again under a revoked key's name starts with nothing. A rotation keeps the issuance, and its conversations.
- the session key;
- the **opening**: a digest of the first user turn, without the cache markers that clients move as a conversation grows. Neither the model nor the system prompt is part of it: Claude Code names its model in the system prompt.

`session_conversations` holds the target (`provider/model`) each conversation is on. Only a request that **continues** a conversation reads it: one that carries output of an earlier turn, an assistant message or a Responses output item. A request without one starts a conversation, and the policy decides it alone. So a conversation that starts again with the same opening gets nothing from the one before it, in the same session or as a `content` session with the same system prompt. A background request or a subagent has its own opening, so it neither reads nor moves the conversation it runs beside.

### Lookup and moves

`route_json`, counting tokens and the passthroughs read the conversation's target before the policy, and look the name up in a view resolved as if the policy still allowed it (`Policy::resolve_keeping`). A kept model gets back the plain name or rename the exclusion took from it, ahead of another provider that serves the same name, and its `provider/model` form works again. Its lock, if it has one, holds.

A request that succeeds (a 2xx answer) moves its conversation to the target that served it. The move is written before the response goes back to the client, so that the client's next request sees it (`Conversation::served`). Every request takes a sequence number when it arrives: microseconds since the epoch, and never less than one more than the last. A move applies only over an older one, so a request that started before a switch and finishes after it doesn't move the conversation back. Counting tokens moves nothing.

- A request keeps its target only if the policy allows the model by name, or the conversation keeps it already. A target only an alias reaches still moves the conversation, which then keeps nothing, so that the conversation can't reach that target later under a name the policy refuses. Aliases ignore model rules, so an exclusion never stops them anyway.
- A model the policy never allowed never gets into `session_conversations`, so it stays refused in every conversation.
- A provider that `providers` no longer allows is not kept, and neither is an account: those apply to every conversation at once, and an account's lock breaks with `policy`.
- A model the catalog no longer lists loses its plain name whatever the conversation keeps. Xolotl can't keep a model working that its provider stops serving.
- When the state can't be read, the policy decides alone. When a move can't be written, the conversation stays where it was, so switching back to an excluded model it left is then allowed.
- In a locked session, a switch to another name is a request for another name: it leaves the lock alone, and still moves the conversation. The lock then stays on a model no request reaches, and goes with the session.

Rows go at `retention_days` after their last move, like the sessions.

### Limits

- **The client names the conversation.** A conversation is what the key holder sends: a session id, a first user turn and earlier turns. Replaying a conversation it had, the key holder reaches the model that conversation was on, past the exclusion. An exclusion is therefore no boundary against a key holder for the models its conversations were on. To cut those at once, revoke the key and issue it again, or stop allowing the provider.
- **Compaction starts a new conversation.** A client that replaces the history with a summary changes the first user turn (Claude Code does). The conversation that follows is new, and an excluded model is refused there. A first request after compaction that carries no earlier output is a start too.
- **A new subagent is a new conversation.** It can't start on an excluded model, although its parent goes on.

Options not chosen:

| Option | Why not |
|---|---|
| The model of the session's last request | A background request on another model would end the conversation's model, and an excluded model would fail at the conversation's next turn |
| Every model the session ever used | A conversation could switch back to an excluded model |
| A request on another model whose prompt is as large as the conversation is a switch | A subagent on another model can grow as large as its parent |
| Every conversation of a session shares the models its conversations are on | A new conversation of the session, a `content` session that starts again, and a key issued again under the name would all reach an excluded model |
| Link a compacted conversation to the one it summarizes | Only text the client writes links them, a summary inside its own wrapper, so the link would be a guess at each client's format |

## Tool usage

`TapStream` reads tool calls from the upstream response on the way through: Messages `tool_use` and `server_tool_use` blocks, Responses `function_call` and `custom_tool_call` items, and Chat `tool_calls`. Only successful responses count. `tool_usage` holds a count and a last use per client key and tool name; `GET /admin/tools` joins it with `mcp_call_log`, which already records `/mcp` calls. Names are as the upstream sent them, so a name shortened for a translated request stays shortened.
