# Exchange export

Each LLM request that has a session becomes a row: its request log fields, the request and response bodies, and the response assembled into one format. Rows go to one or more destinations: `[export.clickhouse]` inserts them into a ClickHouse table, and `[export.nats]` publishes them to a NATS JetStream stream that ClickHouse loads. `src/export/mod.rs` holds the row, the capture and the queues; `src/export/clickhouse.rs` and `src/export/nats.rs` hold the destinations. User documentation: [ClickHouse export](../user/clickhouse-export.md) and [NATS export](../user/nats-export.md).

## Capture

The export follows the session ([Sessions and context](sessions.md)), so it covers exactly what has a session: a client key's requests on the three wire formats, including passthroughs, but not `count_tokens` or the bootstrap admin token.

| Part | Taken by | When |
|---|---|---|
| Request | `Exporter::capture`, from `session_for` | Once per request, when its session begins. The client's body, before translation and before `prepare` adds instructions or credentials. It is a slice of the bytes the client sent, cut at `max_body_bytes` on a character boundary, so taking it copies nothing whatever the size. Headers are never taken. |
| Response | `TapStream`, and `relay_buffered` | As the bytes pass, up to `max_body_bytes`. The upstream's body, before translation. |
| Everything else | `Accounting::finish` | When the response finishes or the client disconnects |

`RequestSession::with_capture` holds the request side, and `SessionTrack` carries it to each attempt's `Accounting`. The request is taken once, in `session_for`, and shared by every target an alias tries. It is copied into a row only in the task that builds the row. `Accounting::finish` builds the row in its spawned task, with the request log write, so that assembling a large stream happens after the response has reached the client.

The row's `endpoint` and `response` are the upstream's, and `client_endpoint` and `request` the client's. They differ for a translated request. The request is kept as the client sent it because that is the conversation as the client knows it. The response is kept as the upstream sent it because that is what the tap sees, and it holds what translation drops.

## Assembly

`translate::assemble` decodes a whole upstream body, JSON or SSE, with the decoders that translation uses, and returns the format-neutral `ir::Response` with the error the body reports. A stream that ended early keeps the blocks it got, and the row records both the partial output and the error. A user who interrupts a turn still leaves its output in the table. `output` is the blocks as JSON: `text`, `reasoning` (without the opaque signature or encrypted content) and `tool_call`. `output_text` joins the text blocks.

A response that isn't 2xx is not assembled. `error` is `translate::upstream_error` on its body. The wire is `Wire::for_path` on the upstream endpoint without its query string, which a passthrough may keep.

`SseParser` reads each byte once, whether the body comes whole or as many small chunks: it keeps only an unfinished line, searches for its end from the new bytes, and removes the lines it found in one step. A long line in many chunks, or many lines in one, would otherwise cost time quadratic in its length. This parser also serves translated streams. The Messages decoder keeps its live block indices in a set, as the indices come from the upstream.

Two gaps remain:

- A target that an alias moves on from (`route_json`) is dropped with its body unread, so its row has no response. Reading it would delay the next target.
- The Chat decoder emits tool calls only at `finish_reason`, so a stream cut short in a tool call's arguments loses that call. Emitting it early would change what translated clients receive.

## Destinations

`Exporter` takes each row once and hands it to every `Sink`. A sink is one destination: its own queue, bounded by its own `queue_mb`, and the task that empties it. Capture and assembly are shared, so a new destination adds only how it sends a row.

The capture keeps what the most generous destination wants: bodies when any destination has `bodies`, cut at the largest `max_body_bytes`. `Capture::submit` builds one row for each distinct `(bodies, max_body_bytes)` among the destinations, all with one `id`. A destination that keeps less gets a row built from the request and response cut to its own limit, and assembled from that cut response, so its row is the row it would get on its own. Destinations with the same settings, as with one destination, share a row through an `Arc`. The capture's own row takes the response bytes without a copy. Each destination counts its outcomes in `xolotl.export.rows` under its own `sink`.

## Queue and delivery to ClickHouse

Rows go on an unbounded channel whose size is held under `queue_mb` by a byte count: a row that would pass it is dropped and counted. `Exchange::size` counts every string of the row. `error` is cut at 4 KiB, since an error body without a message becomes the message. One task batches rows by `batch_rows`, 32 MiB or `flush_secs`, whichever comes first, and inserts each batch with `INSERT … FORMAT JSONEachRow` over HTTP. A failed batch is tried three times with backoff (2 s, 4 s), then dropped with a warning. Meanwhile the queue keeps filling, up to its bound.

Every try of a batch carries one `insert_deduplication_token`, and the table has `non_replicated_deduplication_window`. A try that ClickHouse stored but whose answer was lost is then not stored again by the next. Inserts ask for `wait_end_of_query`, and the answer is read to the end: without it ClickHouse can send a 200 before the query fails, and the failure only appears in the body.

A batch body is built once, as `Bytes`, and shared by its tries. The captured response bytes become the `response` column without a copy when they are valid UTF-8.

Memory was measured with a mock upstream at about 320 requests a second of 100 KB requests. With glibc's default of one malloc arena per thread, RSS grew with every run, from 200 MiB to 290 MiB, because rows are allocated on the request threads and freed on the export task's thread. With `MALLOC_ARENA_MAX=2`, which the container image sets, RSS stayed at 105–130 MiB with no change in latency.

`Accounting::finish` takes a `Pending` from the capture before it spawns the task that builds the row. On shutdown, after the server has stopped, `Exporter::shutdown` waits for every `Pending` to be submitted or dropped, then has the task insert what is queued. Both waits share one ten-second deadline.

Options not chosen:

- **A bounded channel by row count.** Bodies range from a few hundred bytes to several megabytes, so a row count bounds nothing useful.
- **Retrying a batch without a limit.** Rows would pile up for as long as ClickHouse is down. Dropping is visible in `xolotl.export.rows`, and the SQLite request log still has every request's fields.
- **A ClickHouse client crate.** `reqwest` is already a dependency, and `JSONEachRow` over HTTP needs nothing else.
- **Another allocator.** mimalloc held the same memory with the export on and almost doubled it with the export off.
- **A durable spool on disk.** The export is for analysis, not an audit log. Losing rows while ClickHouse is down is acceptable, and a spool would put prompts on xolotl's disk.

The client follows no redirects, so credentials go only to the configured `url`. An `https` url signed by a private CA takes `ca_cert`, like the OpenBao client.

A statement with no input, such as `CREATE TABLE`, is sent as the POST body, not as the `query` parameter. ClickHouse refuses a POST without a `Content-Length`, and reqwest sends none for an empty body.

## NATS JetStream

ClickHouse's NATS table engine reads the stream through a durable pull consumer and acknowledges a message only after the views it feeds have inserted it. Rows published while ClickHouse is down wait in the stream, and xolotl runs no loader. Other services will publish to NATS the same way, and other readers can consume the stream without touching xolotl.

Each row is one message on `xolotl.exchanges.<client_id>`, serialized as for the direct export (`JSONEachRow`), so the engine parses it with the same column types. A client id is a policy and key name, which hold no character that NATS reserves in a subject; `nats::subject` replaces any as a guard.

The publisher sends one row at a time and waits for its `PubAck`, so a row counts as `published` only once the stream has stored it. The message ID is the row `id`, and the stream's duplicate window drops a retry of a publish whose ack was lost: the client buffers a publish while it is disconnected, and may deliver it after the publisher has given up waiting and sent it again.

A failed publish is retried without a limit, with a backoff from 0.5 s to 10 s, while new rows wait behind it. The queue's byte bound is the only reason a row is dropped. This differs from the direct export, which drops a batch after three tries, because on a single NATS server a NATS restart is the outage that the stream can't cover: only xolotl's queue holds rows meanwhile. The one failure that is not retried is a message the stream will never take: the server's `max_payload`, or the stream's `max_msg_size`, is smaller than the message.

Before it publishes, `nats::encode` fits the row into the smaller of the server's `max_payload`, read from the server's `INFO` at each publish, and the stream's `max_msg_size`, less room for the headers. A row that is too large has its `response` cut first, because `output` keeps what the response said, and then its `request`, each on a character boundary and measured as escaped JSON, and is marked `*_truncated`. A row that doesn't fit even without them is refused and counted as `refused`.

At startup, `nats::start` connects, reads the stream and checks that one of its subjects covers `xolotl.exchanges.>`. It fails if NATS can't be reached, the stream is missing, or its subjects don't cover the rows, so that a misconfigured stream is found at deploy time, not by a silent gap in the table. Xolotl creates neither the stream nor the consumer nor the tables: their limits, retention and permissions belong to whoever runs NATS and ClickHouse. After startup the client reconnects without a limit.

On shutdown the publisher sends what is queued, within the exporter's ten-second deadline.

Options not chosen:

- **A loader in xolotl that reads the stream and inserts into ClickHouse.** ClickHouse's engine does that with a few lines of DDL, and a loader in xolotl would serve only xolotl.
- **Vector as the loader.** Its NATS source has no end-to-end acknowledgement, so a crash loses messages it acknowledged but had not written.
- **Redpanda Connect as the loader.** Its `nats_jetstream` input acknowledges after the write. It is the fallback if the engine causes trouble.
- **Publishing several rows before waiting for their acks.** One row at a time keeps the order and the retry simple, and a local server acknowledges well within the measured peak request rate.
- **Creating the stream at startup.** The stream's retention, size and replicas are infrastructure decisions, and a stream created with defaults would keep prompts with no age limit.

## Table

`create_table` issues `CREATE TABLE IF NOT EXISTS` before the first insert, and again before each later batch until it succeeds. It orders by `(client_id, session_key, started_at)`, so a session's turns are stored together: queries by session read few granules, and the bodies of consecutive turns, which repeat the conversation so far, compress well. Bodies use `ZSTD(3)`. Inserts set `input_format_skip_unknown_fields`, so an operator's own table may leave out columns, for example the bodies.

## Privacy

The SQLite database keeps neither prompts nor client-sent identifiers ([Sessions and context](sessions.md#session-identity)). The export does keep prompts, and so it is off by default. `bodies = false` keeps the row to the fields the request log already has. The session key in the table is the same digest as in SQLite, not the client's identifier.
