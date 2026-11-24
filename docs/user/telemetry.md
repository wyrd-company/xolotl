---
title: Telemetry
order: 10
product: xolotl
---

Xolotl exports traces, logs and metrics over OTLP/HTTP when
`telemetry.otlp_endpoint` is set. It writes logs to stdout in any case.

## Export

```toml
[telemetry]
otlp_endpoint = "http://openobserve:5080/api/default"   # OpenObserve: /api/<org>
service_name = "xolotl"
metrics_interval_secs = 30
dashboard_url = "https://openobserve.example.lan"       # linked from the web UI
```

Xolotl adds `/v1/traces`, `/v1/metrics` and `/v1/logs` to the endpoint. Put
header values, for example `Authorization`, in the OpenBao secret `telemetry`:

```sh
bao kv put secret/xolotl/telemetry Authorization="Basic ..."
```

The OpenBao values win over `[telemetry.otlp_headers]`.

## Logs

| Setting | Values |
|---|---|
| `log_format` (`XOLOTL_LOG_FORMAT`) | `pretty` (default) or `json`. The container image uses `json` |
| `log_filter` | A `RUST_LOG`-style filter. Default `info` |

## Metrics

| Metric | Attributes | Meaning |
|---|---|---|
| `xolotl.requests` | provider, model, endpoint, status, client | Proxied LLM requests |
| `xolotl.request.duration` | provider, model, endpoint | Request duration |
| `xolotl.tokens` | provider, model, direction, client | Tokens by direction (`input`, `output`, `cached`) |
| `xolotl.account.usage_percent` | account, provider, window | Usage of each account window |
| `xolotl.account.available` | account, provider | 1 when the account can take requests |
| `xolotl.upstream.errors` | provider, status or kind | Upstream errors of subscription accounts |
| `xolotl.pricing.refreshes` | result | Model pricing refresh attempts: `success`, `fetch_error` or `validation_error` (see below) |
| `xolotl.pricing.misses` | provider, model, reason | Requests with missing model pricing: `model_not_found` or `missing_rate` (see below) |
| `xolotl.oauth.refreshes` | provider, result | OAuth token refreshes. `result=uncertain`: the provider may have used the refresh token, but no usable answer came back |
| `xolotl.vault.unsaved_credentials` | | Refreshed tokens not yet written to OpenBao. Alert when this stays above 0 |
| `xolotl.mcp.tool_calls` | server, tool, status | MCP tool calls |
| `xolotl.mcp.tool_call.duration` | server, tool | MCP tool call duration |
| `xolotl.mcp.executions` | status | `execute` calls |
| `xolotl.limit.consumption` | client, unit (`requests`, `tokens`), window (`minute`, `day`) | Rate limit consumption of each key |
| `xolotl.limit.rejections` | policy, client, limit | Requests refused by a rate limit |
| `xolotl.session.context_tokens` | provider, model | Prompt size of each successful request |
| `xolotl.session.context_utilization` | provider, model | Prompt size as a fraction of the model's context window, when known |
| `xolotl.session.locks` | event (`formed`, `broken`), provider, and reason when broken | Context locks. See [Context lock](accounts.md#context-lock) |
| `xolotl.tool_calls` | client, tool | Tool calls named in LLM responses |
| `xolotl.export.connected` | sink (`nats`) | 0 before connection and while disconnected; 1 while connected to NATS. Stream validation errors are logged separately |
| `xolotl.export.rows` | sink (`clickhouse`, `nats`), outcome | Exchanges exported. ClickHouse: `inserted`, `dropped` (queue full) or `failed` (ClickHouse refused the batch). NATS: `published`, `dropped` (queue full) or `refused` (too large for the stream). See [ClickHouse export](clickhouse-export.md) and [NATS export](nats-export.md) |

Pricing refresh results describe each attempted download of the model pricing
registry. An empty `catalog.metadata_url` disables downloads and records no attempt.

- `success`: the registry contained usable metadata and replaced the in-memory
  prices. Malformed fields or entries that can be ignored do not make a usable
  registry fail. A failure to save the local cache does not change this result.
- `fetch_error`: the download failed, including connection, HTTP status or response
  body transfer errors. The last usable prices remain available.
- `validation_error`: the downloaded body was invalid JSON, was not a registry
  object, or contained no usable model metadata. The last usable prices remain
  available.

Pricing misses count once per completed request:

- `model_not_found`: no registry entry or configured metadata override matched
  the served model. This counts even when token usage is absent.
- `missing_rate`: metadata matched, but observed token usage proves a rate is
  required and missing. Input, output, cache-read and cache-write categories are
  checked independently, so partial usage can establish a miss.

A zero-token category needs no rate, and a zero price is valid. Missing input or
output usage alone does not count as a pricing miss or imply a required rate.
The `provider` and `model` attributes use the same request identity as
`xolotl.requests`, including a requested alias where applicable. Registry keys, URLs and error messages are not attributes.

## Request log

Every LLM request is also in the SQLite request log, for `database.retention_days`
(90). Read it with `xolotl usage` or on the web UI's **Usage** screen. Each row
holds `session_key`, a digest that identifies its conversation, `context_tokens`,
the prompt's size, and `context_window`, the model's window when known.
