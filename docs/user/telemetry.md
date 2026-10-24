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
| `xolotl.oauth.refreshes` | provider, result | OAuth token refreshes. `result=uncertain`: the provider may have used the refresh token, but no usable answer came back |
| `xolotl.vault.unsaved_credentials` | | Refreshed tokens not yet written to OpenBao. Alert when this stays above 0 |
| `xolotl.mcp.tool_calls` | server, tool, status | MCP tool calls |
| `xolotl.mcp.tool_call.duration` | server, tool | MCP tool call duration |
| `xolotl.mcp.executions` | status | `execute` calls |
| `xolotl.limit.consumption` | client, unit (`requests`, `tokens`), window (`minute`, `day`) | Rate limit consumption of each key |
| `xolotl.limit.rejections` | policy, client, limit | Requests refused by a rate limit |

## Request log

Every LLM request is also in the SQLite request log, for `database.retention_days`
(90). Read it with `xolotl usage` or on the web UI's **Usage** screen.
