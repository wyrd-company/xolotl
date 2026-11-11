---
title: ClickHouse export
order: 11
product: xolotl
---

Xolotl can copy each request to an account, and its response, to a ClickHouse
table, so you can query what your agents asked and what the models answered. It is
off until you add `[export.clickhouse]`.

The rows hold prompts and answers as the clients and upstreams sent them.
Anyone who can read the table can read your conversations. Set `bodies = false`
to keep only the request log fields.

## What is exported

One row for each request with a client key that xolotl sends to an account, of
any kind: subscription pools, provider accounts and API key accounts alike. That
is the LLM endpoints (`/v1/messages`, `/v1/responses`, `/v1/chat/completions`),
the other `/v1/...` endpoints such as embeddings and OCR, and passthroughs. Two
kinds of request are not exported: `count_tokens`, which is not an exchange, and
requests with the bootstrap admin token, which have no client key. A request
that moves to the next target of an alias gives one row per target, like the
request log.

Each row has:

- the request log fields: client, provider, account, model, endpoint, status,
  latency, tokens, cost, context size and trace id;
- `account_kind`: the kind of the account that served the request, as on the
  **Accounts** screen, such as `claude`, `opencode-go` or `mistral`. For a
  provider account, `provider` is the account's name and `account_id` is empty;
- the session's `session_key`, the same digest as in the request log and at
  `GET /admin/sessions`, and `session_source`, what it was read from. Both are
  empty for a request without a session: one on an endpoint that is not an
  LLM conversation, such as embeddings;
- `request`: the body the client sent, before translation and before xolotl
  adds anything for the upstream. Headers, and so credentials, are never exported;
- `response`: the body the upstream sent, as JSON or as the SSE stream;
- `output`, `output_text`, `stop_reason` and `tool_calls`: the response
  assembled from the stream, in one format whatever the provider. `output` is a
  JSON array of `text`, `reasoning` and `tool_call` blocks. A stream that was cut
  short keeps what arrived;
- `error`: the failure the response reported, as `type: message`, cut at 4 KiB.

`client_endpoint` is the endpoint the client called and `endpoint` the one the
upstream served. They differ when xolotl translated the request, and then
`response` is in the upstream's format.

Some responses are not read whole, and their rows have less:

- When an alias moves on from a target that answered 429 or 5xx, xolotl drops
  that answer unread. Its row has the status but no `response` or `error`.
- A response that is not text, such as audio or an image, is not kept: its
  `response` is empty and `response_truncated` is true.
- A passthrough request body that is not JSON, such as a file upload, streams
  to the upstream unread: its `request` is empty and `request_truncated` is true.
- A Chat Completions stream cut short while a tool call's arguments arrive
  loses that tool call from `output`. Text before it is kept.

## Set up

Create the database. Xolotl creates the table in it before its first insert,
unless `create_table = false`:

```sql
CREATE DATABASE xolotl;
CREATE USER xolotl IDENTIFIED BY '...';
GRANT CREATE TABLE, INSERT ON xolotl.* TO xolotl;
```

For an `https` url signed by a private CA, point `ca_cert` at the CA's PEM
bundle.

Put the password in OpenBao. A `user` there wins over the config file. The
`url` can't hold credentials:

```sh
bao kv put secret/xolotl/clickhouse user=xolotl password="..."
```

Then configure the export:

```toml
[export.clickhouse]
url = "http://clickhouse:8123"   # the HTTP interface. Env: XOLOTL_CLICKHOUSE_URL
database = "xolotl"
table = "llm_exchanges"
# user = "xolotl"                # unless the OpenBao secret has one
# ca_cert = "/etc/xolotl/clickhouse-ca.pem"  # PEM CA bundle for an https url with a private CA
create_table = true              # CREATE TABLE IF NOT EXISTS before the first insert
ttl_days = 0                     # rows expire after this many days; 0 keeps them
bodies = true                    # false: no request, response or output
max_body_bytes = 4194304         # longer bodies are cut, and the row says so
batch_rows = 100                 # rows per insert
flush_secs = 5                   # longest a row waits for its insert
queue_mb = 256                   # rows past this are dropped
```

`ttl_days` applies only to a table xolotl creates.

## Delivery

Xolotl queues rows and inserts them in batches of `batch_rows`, or every
`flush_secs`. Exporting never delays or fails a request. When ClickHouse is
down, xolotl tries a batch three times, then drops it and logs a warning. Rows
that arrive meanwhile wait in the queue, so an outage of a few minutes loses
only the batches tried during it. When more than `queue_mb` of rows wait, new
rows are dropped. On shutdown xolotl
inserts what is queued, waiting up to ten seconds.

Each try of a batch carries the same `insert_deduplication_token`, so a batch
that ClickHouse stored but did not acknowledge is not stored twice. That needs
`non_replicated_deduplication_window` on the table, which the table xolotl
creates has. On a table you create, set it too, or count rows by distinct `id`.

Queued rows are held in memory, up to `queue_mb`. The container image sets
`MALLOC_ARENA_MAX=2`. Set it too when you run the binary yourself: without it,
glibc keeps freed bodies in per-thread arenas and memory grows under load.

`xolotl.export.rows` counts rows by `outcome`: `inserted`, `dropped` or
`failed`. See [Telemetry](telemetry.md).

## The table

```sql
CREATE TABLE xolotl.llm_exchanges (
    id UUID,
    started_at DateTime64(3, 'UTC'),
    finished_at DateTime64(3, 'UTC'),
    trace_id String,
    client_id LowCardinality(String),
    session_key String,
    session_source LowCardinality(String),
    provider LowCardinality(String),
    account_id LowCardinality(String),
    account_kind LowCardinality(String),
    model LowCardinality(String),           -- the name the client asked for
    upstream_model LowCardinality(String),
    client_endpoint LowCardinality(String),
    endpoint LowCardinality(String),
    status UInt16,
    latency_ms UInt64,
    streamed Bool,
    input_tokens Nullable(Int64),           -- cached and uncached
    output_tokens Nullable(Int64),
    cached_tokens Nullable(Int64),
    cache_write_tokens Nullable(Int64),
    context_tokens Nullable(Int64),
    context_window Nullable(Int64),
    cost Nullable(Float64),                 -- the API price of the tokens
    tool_calls Array(LowCardinality(String)),
    stop_reason LowCardinality(String),
    error String,
    request String CODEC(ZSTD(3)),
    request_truncated Bool,
    response String CODEC(ZSTD(3)),
    response_truncated Bool,
    output String CODEC(ZSTD(3)),
    output_text String CODEC(ZSTD(3))
)
ENGINE = MergeTree
PARTITION BY toYYYYMM(started_at)
ORDER BY (client_id, session_key, started_at)
SETTINGS non_replicated_deduplication_window = 1000
```

To make the table yourself, set `create_table = false`. Xolotl inserts with
`input_format_skip_unknown_fields`, so a table may leave out columns.

### Upgrade a table

Xolotl creates a table only when it is missing, so a table made by an earlier
version has no `account_kind` column. Inserts into it go on working, without
the column. To add it:

```sql
ALTER TABLE xolotl.llm_exchanges ADD COLUMN IF NOT EXISTS account_kind LowCardinality(String) AFTER account_id;
```

Rows inserted before the change have an empty `account_kind`.

## Example queries

The turns of one session, in order:

```sql
SELECT started_at, model, input_tokens, output_tokens, stop_reason, left(output_text, 200)
FROM xolotl.llm_exchanges
WHERE session_key = '…'
ORDER BY started_at;
```

Sessions by size and cache hit ratio:

```sql
SELECT client_id, session_key, count() AS turns, max(context_tokens) AS peak,
       sum(cached_tokens) / sum(input_tokens) AS cache_hit
FROM xolotl.llm_exchanges
WHERE status = 200 AND started_at > now() - INTERVAL 7 DAY
GROUP BY client_id, session_key
ORDER BY peak DESC
LIMIT 20;
```

The tools each model calls:

```sql
SELECT upstream_model, tool, count() AS calls
FROM xolotl.llm_exchanges ARRAY JOIN tool_calls AS tool
GROUP BY upstream_model, tool
ORDER BY calls DESC;
```

Requests and tokens by account kind:

```sql
SELECT account_kind, provider, count() AS requests, sum(input_tokens) AS input, sum(output_tokens) AS output
FROM xolotl.llm_exchanges
WHERE started_at > now() - INTERVAL 7 DAY
GROUP BY account_kind, provider
ORDER BY requests DESC;
```

The system prompt of a Messages request:

```sql
SELECT JSONExtractRaw(request, 'system') FROM xolotl.llm_exchanges
WHERE client_endpoint = '/v1/messages' LIMIT 1;
```
