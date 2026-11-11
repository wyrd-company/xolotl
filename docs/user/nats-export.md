---
title: NATS export
order: 12
product: xolotl
---

Xolotl can publish each exported exchange to a NATS JetStream stream, and
ClickHouse loads the stream into a table with its NATS table engine. While
ClickHouse is down for maintenance or a restart, rows wait in the stream and
arrive late instead of being lost. Other readers, such as dashboards or evals,
can read the same stream. It is off until you add `[export.nats]`.

The rows are the rows of the [ClickHouse export](clickhouse-export.md): the same
fields, the same capture, and the same privacy warning. The stream keeps prompts
and answers on the NATS server's disk until they expire.

Xolotl only publishes. It does not create the stream, the consumer or the
tables: this page gives the commands, and you run them once.

## How it works

1. Xolotl publishes each row as one JSON message to
   `xolotl.exchanges.<client_id>`, and waits until the stream has stored it.
   The message ID (`Nats-Msg-Id`) is the row's `id`, so a publish that xolotl
   retries after a lost acknowledgement is dropped by the stream.
2. ClickHouse reads the stream through a durable pull consumer, and
   acknowledges each message after it has inserted it. Delivery is
   at-least-once. The target table is a `ReplacingMergeTree` with `id` in its
   sort key, so a message delivered twice is merged into one row.

Needs:

- NATS 2.10 or later, with JetStream.
- ClickHouse 26.10 or later, or 26.3.33, 26.7.14, 26.8.11 or 26.9.2. Older
  versions can stop consuming without an error after NATS reconnects.

## Set up

### NATS server

Messages carry the request and response bodies, and real rows reach several
megabytes. The server default `max_payload` is 1 MB. Set it to 8 MB:

```text
max_payload: 8MB
jetstream {
  store_dir: /data/jetstream
  # sync_interval: always   # see "Durability" below
}
```

### Stream and consumer

```sh
nats stream add XOLOTL --subjects 'xolotl.exchanges.>' --storage file --retention limits \
  --discard old --max-age 7d --max-bytes <sized> --max-msg-size 8MB --dupe-window 2m --replicas 1 --defaults
nats consumer add XOLOTL clickhouse --pull --deliver all --ack explicit --wait 30s --max-deliver=-1 --defaults
```

Set `--max-bytes` from [Sizing](#sizing). `--max-age` is how long the stream
keeps prompts. `--dupe-window` must be longer than one publish retry (a few
seconds); 2 minutes is the NATS default. Use `--replicas 3` on a cluster.
`--defaults` takes the default for every option not given, instead of asking.

### ClickHouse tables

The target table, the NATS engine table that reads the consumer, and the views
that move rows from one to the other:

```sql
CREATE TABLE xolotl.exchanges (
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
    model LowCardinality(String),
    upstream_model LowCardinality(String),
    client_endpoint LowCardinality(String),
    endpoint LowCardinality(String),
    status UInt16,
    latency_ms UInt64,
    streamed Bool,
    input_tokens Nullable(Int64),
    output_tokens Nullable(Int64),
    cached_tokens Nullable(Int64),
    cache_write_tokens Nullable(Int64),
    context_tokens Nullable(Int64),
    context_window Nullable(Int64),
    cost Nullable(Float64),
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
ENGINE = ReplacingMergeTree
PARTITION BY toYYYYMM(started_at)
ORDER BY (client_id, session_key, started_at, id);

CREATE TABLE xolotl.exchanges_queue (
    id UUID,
    started_at DateTime64(3, 'UTC'),
    finished_at DateTime64(3, 'UTC'),
    trace_id String,
    client_id String,
    session_key String,
    session_source String,
    provider String,
    account_id String,
    account_kind String,
    model String,
    upstream_model String,
    client_endpoint String,
    endpoint String,
    status UInt16,
    latency_ms UInt64,
    streamed Bool,
    input_tokens Nullable(Int64),
    output_tokens Nullable(Int64),
    cached_tokens Nullable(Int64),
    cache_write_tokens Nullable(Int64),
    context_tokens Nullable(Int64),
    context_window Nullable(Int64),
    cost Nullable(Float64),
    tool_calls Array(String),
    stop_reason String,
    error String,
    request String,
    request_truncated Bool,
    response String,
    response_truncated Bool,
    output String,
    output_text String
)
ENGINE = NATS SETTINGS
    nats_url = 'nats:4222',
    nats_subjects = 'xolotl.exchanges.>',
    nats_stream = 'XOLOTL',
    nats_consumer_name = 'clickhouse',
    nats_format = 'JSONEachRow',
    nats_max_block_size = 1000,
    nats_flush_interval_ms = 1000,
    nats_handle_error_mode = 'stream';

CREATE MATERIALIZED VIEW xolotl.exchanges_mv TO xolotl.exchanges AS
SELECT * FROM xolotl.exchanges_queue WHERE _error IS NULL;

-- Messages that don't parse are acknowledged and kept here, instead of being
-- delivered again forever.
CREATE TABLE xolotl.exchanges_errors (
    at DateTime DEFAULT now(),
    subject String,
    raw String,
    error String
)
ENGINE = MergeTree ORDER BY at TTL at + INTERVAL 30 DAY;

CREATE MATERIALIZED VIEW xolotl.exchanges_errors_mv TO xolotl.exchanges_errors AS
SELECT _subject AS subject, _raw_message AS raw, _error AS error
FROM xolotl.exchanges_queue WHERE _error IS NOT NULL;
```

The view filters must be `_error IS NULL` and `_error IS NOT NULL` exactly.
The NATS engine's `_error` is `Nullable(String)`, unlike the Kafka engine's
`String`. A filter such as `length(_error) = 0` is NULL for every good row, so
the view drops them all, and the engine still acknowledges them.

To add a TTL, add `TTL toDateTime(started_at) + INTERVAL 30 DAY` to the target
table.

When NATS needs credentials, add `nats_username` and `nats_password`, or
`nats_credential_file`, to the engine settings, and `nats_secure = 1` for TLS.

### Xolotl

Put the credentials in OpenBao, either a user and password, or the contents of
a `.creds` file:

```sh
bao kv put secret/xolotl/nats user=xolotl password="..."
bao kv put secret/xolotl/nats creds=@xolotl.creds
```

Then configure the export:

```toml
[export.nats]
url = "nats://nats:4222"         # tls:// requires TLS. Several servers: comma-separated. Env: XOLOTL_NATS_URL
stream = "XOLOTL"                # must exist, with subjects covering xolotl.exchanges.>
# ca_cert = "/etc/xolotl/nats-ca.pem"  # PEM CA bundle for a server with a private CA
bodies = true                    # false: no request, response or output
max_body_bytes = 4194304         # longer bodies are cut, and the row says so
queue_mb = 32                    # rows waiting while NATS is unreachable; rows past this are dropped
```

At startup xolotl connects to NATS and checks that the stream exists and that
its subjects cover `xolotl.exchanges.>`. If not, it stops with an error that
says which. Start NATS before xolotl, or let your service manager restart
xolotl until NATS is up.

You can keep `[export.clickhouse]` too. Each destination then has its own
queue, and gets the bodies its own `bodies` and `max_body_bytes` allow.

## Delivery

- **ClickHouse down.** Rows wait in the stream, up to its `max-bytes` and
  `max-age`. ClickHouse inserts them when it is back, with no xolotl restart.
- **NATS down.** Rows wait in xolotl's queue, in memory, up to `queue_mb`.
  Xolotl retries the oldest row until NATS takes it, waiting up to 10 seconds
  between tries, and then publishes the rest. Only when the queue is full are
  new rows dropped. A xolotl restart loses what is queued, after up to 10
  seconds spent publishing it.
- **A row too large for a message.** The largest message is the server's
  `max_payload`, or the stream's `max-msg-size` when that is smaller. Xolotl
  cuts the row's `response`, then its `request`, until the row fits, and sets
  `response_truncated` or `request_truncated`. A row that doesn't fit even
  without them, for example because of a very long `output`, is refused.

`xolotl.export.rows` with `sink = nats` counts rows by `outcome`:

| Outcome | Meaning |
| --- | --- |
| `published` | The stream stored the row |
| `dropped` | The queue was full |
| `refused` | The row is too large for the stream, even without its bodies |

See [Telemetry](telemetry.md).

## Operations

### Sizing

`max-bytes` must hold the rows that arrive during the longest planned
ClickHouse maintenance, with a margin: rows per second × seconds × row size.
Rows measured 50–150 KB with bodies. For example, 50 rows a second for a
2-hour window at 100 KB is about 36 GB; set 50 GB. With `--discard old`, a full
stream deletes its oldest rows to take new ones.

`queue_mb` bounds the rows held in memory while NATS restarts. 32 MiB holds
roughly 200–600 rows.

### Permissions

Only xolotl publishes to `xolotl.exchanges.>`, and only ClickHouse consumes:

```text
authorization {
  users = [
    { user: xolotl, password: "...", permissions: {
        publish: ["xolotl.exchanges.>", "$JS.API.STREAM.INFO.XOLOTL"]
        subscribe: ["_INBOX.>"] } }
    { user: clickhouse, password: "...", permissions: {
        publish: ["$JS.API.CONSUMER.MSG.NEXT.XOLOTL.clickhouse", "$JS.API.CONSUMER.INFO.XOLOTL.clickhouse",
                  "$JS.API.STREAM.INFO.XOLOTL", "$JS.ACK.XOLOTL.clickhouse.>"]
        subscribe: ["_INBOX.>"] } }
  ]
}
```

### Lag alert

The NATS monitoring endpoint `/jsz?consumers=true` reports `num_pending` (not
yet delivered) and `num_ack_pending` (delivered, not yet acknowledged) for the
`clickhouse` consumer. Alert when the backlog stays above a threshold, or when
it is not zero and the ack floor does not move. A stalled loader otherwise
looks the same as quiet traffic.

### Count check

The lag alert can't see a view that drops rows, because the engine still
acknowledges them. Compare, hour by hour, the rows xolotl published
(`xolotl.export.rows`, `sink = nats`, `outcome = published`) with the rows in
ClickHouse:

```sql
SELECT toStartOfHour(started_at) AS hour, uniqExact(id) AS rows
FROM xolotl.exchanges
WHERE started_at > now() - INTERVAL 1 DAY
GROUP BY hour ORDER BY hour;
```

Also check that `xolotl.exchanges_errors` stays empty.

### Duplicates

Delivery is at-least-once, so a row can be inserted twice, for example after
ClickHouse is killed while it inserts. `ReplacingMergeTree` merges them in the
background. For exact counts, use `FINAL` or `uniqExact(id)`.

### Changing the table

The durable consumer keeps its place, so the stream holds rows while you work:

1. Drop the views and the queue table.
2. Alter the target table.
3. Create the queue table and the views again, with the new columns.

The queue table ignores fields it has no column for, so rows with a new field
load into the old tables, without it, until you make the change.

Tables made from an earlier version of this page have no `account_kind`. Add
it with the steps above, and in step 2:

```sql
ALTER TABLE xolotl.exchanges ADD COLUMN IF NOT EXISTS account_kind LowCardinality(String) AFTER account_id;
```

In step 3, the queue table has `account_kind String` after `account_id`, as in
[ClickHouse tables](#clickhouse-tables).

### Replay

To load rows again, for example after a wrong view:

1. Drop the views and the queue table.
2. `nats consumer rm XOLOTL clickhouse`, then add it again with
   `--deliver <first lost stream sequence>` instead of `--deliver all`.
3. Create the queue table and the views again.

`ReplacingMergeTree` merges the rows loaded twice.

### Durability

JetStream writes to disk every 2 minutes by default. On a single server, a
power loss or kernel crash (not a process crash) can lose up to the last 2
minutes of rows that it had acknowledged. Replicas on a cluster cover this. On
a single server, set `jetstream { sync_interval: always }` if that matters. Its
throughput cost has not been measured. ClickHouse's equivalent is
`fsync_after_insert = 1` on the target table.

On a single server, a NATS restart is the outage that matters: rows wait only
in xolotl's queue while it lasts.
