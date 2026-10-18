# Rate limits

A policy can cap how much its keys use the LLM endpoints: every request, and the requests sent to one provider or one subscription account. `src/limits.rs` holds the limits and the counters.

```toml
[limits]
per = "key"                  # or "policy": all keys of the policy share one allowance
requests_per_minute = 60
requests_per_day = 5000
tokens_per_minute = 200000
tokens_per_day = 5000000

[limits.providers.vercel]
tokens_per_day = 1000000

[limits.accounts.codex-1a2b3c4d5e]
requests_per_day = 500
```

Every field is optional; a missing one is unlimited. Values must be at least 1: to block a key, disable it.

## Units

| Limit | Counts |
|---|---|
| `requests_per_minute`, `requests_per_day` | Requests admitted to an LLM endpoint: `/v1/...`, `/backend-api/codex/...`, gateway passthroughs (`/<provider>/...`) and `/opencode-go/...`. An alias that falls back to its next target is one request. |
| `tokens_per_minute`, `tokens_per_day` | Input plus output tokens, as the upstream reports them in the response. Input includes cache reads and writes. A streamed Chat Completions request (also through a passthrough) always asks the upstream for its usage chunk (`stream_options.include_usage`), so the client gets one extra chunk with empty `choices` even if it didn't ask for it. |

There is no spend limit. Subscription accounts have no per-request price, and xolotl has no price list for gateway providers. Token budgets apply to both.

`/v1/models`, `/mcp`, the management API and the admin API are not limited.

## Windows and counting

- **Sliding windows.** A minute is the last 60 s, counted in one-second buckets. A day is the last 24 h, counted in one-minute buckets.
- **Per key or per policy.** Consumption is always counted per key. With `per = "policy"` the limits are checked against the sum over every key of the policy. Client ids are `<policy>/<name>`, so a key counts toward the policy it was issued under.
- **When.** A request is counted when it is admitted. Its tokens are counted when its response finishes (or the client disconnects). So a token limit is checked before a request, and the request that crosses the limit still completes.
- **Refusals don't count.** A request refused by a limit, by authentication or by the policy is not counted, so a client that keeps retrying recovers as soon as the window allows.
- **Meters.** Each key has a total meter and one meter per provider and per account. A routed request is admitted on its total meter and its first target's provider meter together; each later alias target counts only on its provider. A pool target counts the total and provider meters together with the account that admits the request, in one admission, so a request no account admits counts nowhere. A pool counts a request on each account it sends it to; an account whose last slot another request took since it was checked is passed over. Tokens count on the total, the provider and the account of the request log row.
- **Skipping.** A provider at its limit makes an alias try its next target; the last target answers 429. Account selection skips accounts at a limit (`Limiter::check`), then counts the chosen account (`Limiter::admit`), so a concurrent request that reached the limit in between moves the request on to another account. With every account at a limit the pool answers 429 with the earliest `retry-after`.
- **Restart.** The request log keeps one row per request with the account that answered, so after a restart an account's request count leaves out attempts that failed over.
- **The bootstrap admin token** has no limits.

## Enforcement

The counters are in memory. At startup they are rebuilt from the request log of the last 24 h: each log row counts one request at the time it was admitted (`started_at`) and its tokens at the time it finished (`ts`), as live counting does. A pool's row without an account is a request refused before any account admitted it, and counts nowhere. This matches the single-instance deployment; several instances would each count only their own requests.

Every 60 s keys with nothing left in the day window are dropped.

## Responses

A request over a limit gets `429` with `retry-after`: the seconds until enough of the window has passed for the limit to admit a request. For tokens this is when consumption drops below the limit. The body is a `rate_limit_error` in the shape of the endpoint that was called: Anthropic's for `/v1/messages` (and passthrough paths under `v1/messages`), OpenAI's otherwise. The message names the limit and whether it counts the key or the policy.

## Visibility

| Where | Shows |
|---|---|
| `xolotl keys list` | Consumption per window for every key, as `used/limit` where the policy sets a limit. `(policy)` marks a shared allowance; the limited windows then show the policy's total. |
| `GET /admin/keys` | Per key: `usage` (the key's own consumption); with limits, also `limits` and `limit_usage` (the consumption the limits are checked against) |
| `GET /admin/policies/<name>/view` | `usage`: all keys of the policy together; `provider_usage`, `account_usage`: the same per limited provider and account |
| Web UI | Policies: the limits and the policy's consumption. Client keys: consumption against each limit. |
| Metrics | `xolotl.limit.consumption` (gauge by client, `unit` requests or tokens, `window` minute or day), `xolotl.limit.rejections` (by policy, client and limit) |
