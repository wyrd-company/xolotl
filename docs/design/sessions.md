# Sessions and context

Every LLM request of a client key belongs to a **session**: one conversation. Xolotl records how full each session is, and once a session's prompt cache is worth keeping, it **locks** the session to the account and model that hold that cache. A session also **keeps** each model it is on after the key's policy excludes it. `src/context/` holds the identity, the accounting, the lock and the kept models; the pool (`src/subscription/mod.rs`) and `route_json` (`src/server/mod.rs`) apply the lock, and `route_json` and the passthroughs apply the kept models.

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
| Claude Code sends background requests on a cheaper model under its session id | `request_log`: no `session_key` from a `claude-code` session with more than one `model` |
| Claude Code sends a subagent's requests under its session id, on the subagent's model | The exported exchanges of one `claude-code` `session_key`: no two first user turns on two models |
| A client keeps a conversation's first user turn as the conversation grows, apart from cache markers | The exported exchanges of one `session_key`: two requests of one conversation on one model whose first user turns differ |
| A background request or a subagent doesn't open with the conversation's first user turn | The exported exchanges of one `session_key`: a request on another model with the conversation's first user turn, followed by the conversation on its own model again |

## Models a session keeps

A model is checked against the key's policy when a session first uses it: when the session starts with it, or switches to it. Once a request on it succeeds, the session is **on** the model and keeps it. A later exclusion by a rule, its `models` switch or `deny_other_models` doesn't stop that session's requests to it. A new session can't start with it, and a session that switched away can't switch back.

`session_models` holds the targets (`provider/model`) each session is on. `route_json`, counting tokens and the passthroughs read them before the policy (`context::kept_models`) and look the name up in a view resolved as if the policy still allowed them (`Policy::resolve_keeping`). A kept model gets back the plain name or rename the exclusion took from it, ahead of another provider that serves the same name, and its `provider/model` form works again. Its lock, if it has one, holds.

`Accounting::finish` records the target that served each successful request, in the same transaction as the turn (`use_model`). A request adds its target only if the policy allows the model by name or the session keeps it already:

- A target only an alias reaches is not kept, so that the session can't then reach it under a name the policy refuses. Aliases ignore model rules, so an exclusion never stops them anyway.
- A model the policy never allowed never gets into `session_models`, so it stays refused in every session.
- A provider that `providers` no longer allows is not kept, and neither is an account: those apply to every session at once, and an account's lock breaks with `policy`.
- A model the catalog no longer lists loses its plain name whatever the session keeps. Xolotl can't keep a model working that its provider stops serving.

Rows go at `retention_days` after their last use, like the sessions.

### Switching away

A session can carry more than one conversation: Claude Code is taken to send background requests and subagents under its session id, on other models ([Assumptions](#assumptions)). By model name alone, a background request between two turns on a model looks like a switch away and back. Each kept target therefore records the **opening** of its last request: a digest of the first user turn, without cache markers, which clients move as a conversation grows (`context::opening`). A successful request on another target with the same opening is the same conversation on another model. The session is no longer on the target it left, and switching back is a first use, checked against the policy. A background request or a subagent has its own first user turn, so it switches nothing. Neither the model nor the system prompt is part of the opening: Claude Code names its model in the system prompt.

A target that the policy doesn't let the session keep (an alias-only target) still ends the one its conversation left. In a locked session, a switch to another name is a request for another name: it leaves the lock alone, and still ends the kept model. The lock then stays on a model no request reaches, and goes with the session.

Each target holds only its last opening. When two conversations of a session use one model, as a subagent on the main model does, a switch of the other one is missed until that conversation's next request on it, and switching back is allowed. This fails open: a running conversation is never refused because of it.

Options not chosen:

| Option | Why not |
|---|---|
| The model of the session's last request | A background request on another model would end the conversation's model, and an excluded model would fail at the conversation's next turn |
| Every model the session ever used | A conversation could switch back to an excluded model |
| A request on another model whose prompt is as large as the conversation is a switch | A subagent on another model can grow as large as its parent |

## Tool usage

`TapStream` reads tool calls from the upstream response on the way through: Messages `tool_use` and `server_tool_use` blocks, Responses `function_call` and `custom_tool_call` items, and Chat `tool_calls`. Only successful responses count. `tool_usage` holds a count and a last use per client key and tool name; `GET /admin/tools` joins it with `mcp_call_log`, which already records `/mcp` calls. Names are as the upstream sent them, so a name shortened for a translated request stays shortened.
