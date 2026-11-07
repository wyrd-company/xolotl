//! Export of LLM exchanges for analysis: each session request and its
//! response becomes a row in a ClickHouse table, inserted in batches through
//! the HTTP interface. Exporting never holds up or fails a request: rows wait
//! in a queue bounded by bytes, and are dropped and counted when it is full or
//! ClickHouse keeps refusing them.
// ---
// relationships:
//   implements: ../docs/design/export.md
// ---

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use opentelemetry::KeyValue;
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use crate::catalog::Wire;
use crate::config::ClickHouseConfig;
use crate::db::RequestRecord;
use crate::proxy::TokenUsage;
use crate::telemetry::metrics;
use crate::translate::{self, ir};

/// A batch is inserted once it holds this many bytes, whatever `batch_rows` is.
const MAX_BATCH_BYTES: usize = 32 * 1024 * 1024;
/// Longer `error` values are cut.
const MAX_ERROR_BYTES: usize = 4096;
/// Attempts at inserting one batch before it is dropped.
const INSERT_ATTEMPTS: u32 = 3;
/// How long shutdown waits for the last insert.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(10);

/// One request and its response: a row of the exchange table.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Exchange {
    pub id: String,
    pub started_at: String,
    pub finished_at: String,
    pub trace_id: String,
    pub client_id: String,
    pub session_key: String,
    pub session_source: String,
    pub provider: String,
    pub account_id: String,
    /// The model name the client asked for.
    pub model: String,
    pub upstream_model: String,
    /// The endpoint the client called.
    pub client_endpoint: String,
    /// The endpoint the upstream served; differs from `client_endpoint` when translated.
    pub endpoint: String,
    pub status: u16,
    pub latency_ms: u64,
    pub streamed: bool,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cached_tokens: Option<i64>,
    pub cache_write_tokens: Option<i64>,
    pub context_tokens: Option<i64>,
    pub context_window: Option<i64>,
    pub cost: Option<f64>,
    pub tool_calls: Vec<String>,
    pub stop_reason: String,
    /// The failure the response reports: `kind: message`.
    pub error: String,
    /// The request body as the client sent it.
    pub request: String,
    pub request_truncated: bool,
    /// The response body as the upstream sent it: JSON, or the SSE stream.
    pub response: String,
    pub response_truncated: bool,
    /// The response's blocks in a format-neutral JSON array.
    pub output: String,
    /// The response's text blocks, joined.
    pub output_text: String,
}

impl Exchange {
    /// Roughly the bytes the row holds: every string, and a fixed allowance for the rest.
    fn size(&self) -> usize {
        let strings = [
            &self.id,
            &self.started_at,
            &self.finished_at,
            &self.trace_id,
            &self.client_id,
            &self.session_key,
            &self.session_source,
            &self.provider,
            &self.account_id,
            &self.model,
            &self.upstream_model,
            &self.client_endpoint,
            &self.endpoint,
            &self.stop_reason,
            &self.error,
            &self.request,
            &self.response,
            &self.output,
            &self.output_text,
        ];
        let tools: usize = self.tool_calls.iter().map(|t| t.len() + std::mem::size_of::<String>()).sum();
        strings.iter().map(|s| s.len()).sum::<usize>() + tools + std::mem::size_of::<Self>()
    }
}

/// A response body as the tap saw it.
#[derive(Debug, Clone, Default)]
pub struct Captured {
    pub bytes: Vec<u8>,
    pub sse: bool,
    pub truncated: bool,
}

impl Captured {
    /// Keep at most `limit` bytes of `chunk`.
    pub fn push(&mut self, chunk: &[u8], limit: usize) {
        let room = limit.saturating_sub(self.bytes.len());
        if chunk.len() > room {
            self.truncated = true;
        }
        self.bytes.extend_from_slice(&chunk[..chunk.len().min(room)]);
    }
}

/// The request side of an exchange, taken when the request's session begins.
pub struct Capture {
    exporter: Arc<Exporter>,
    endpoint: String,
    /// The client's body, or its first `max_body_bytes`: a slice that shares
    /// the request's buffer.
    request: Bytes,
    request_truncated: bool,
}

impl std::fmt::Debug for Capture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Capture").field("endpoint", &self.endpoint).finish_non_exhaustive()
    }
}

/// A row that will be submitted. Taken when the request finishes, before
/// the row is built in another task.
pub struct Pending {
    capture: Arc<Capture>,
}

impl Pending {
    pub fn submit(self, f: Finished<'_>) {
        self.capture.submit(f);
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        self.capture.exporter.in_flight.fetch_sub(1, Ordering::AcqRel);
    }
}

/// What a finished request adds to its capture.
pub struct Finished<'a> {
    pub record: &'a RequestRecord,
    pub started_at: DateTime<Utc>,
    pub session_key: &'a str,
    pub session_source: &'a str,
    pub upstream_model: &'a str,
    pub usage: TokenUsage,
    pub tools: &'a [String],
    pub response: Option<Captured>,
}

impl Capture {
    /// The most response bytes to keep, or `None` when bodies are not exported.
    pub fn response_limit(&self) -> Option<usize> {
        self.exporter.cfg.bodies.then_some(self.exporter.cfg.max_body_bytes)
    }

    /// Hold the exporter open for a row that is being built: shutdown waits
    /// for it before it inserts what is queued.
    pub fn pending(self: &Arc<Self>) -> Pending {
        self.exporter.in_flight.fetch_add(1, Ordering::AcqRel);
        Pending { capture: self.clone() }
    }

    /// Build the row and queue it.
    pub fn submit(&self, f: Finished<'_>) {
        let row = self.exchange(f);
        self.exporter.enqueue(row);
    }

    fn exchange(&self, f: Finished<'_>) -> Exchange {
        let r = f.record;
        let finished = f.started_at + chrono::Duration::milliseconds(r.latency_ms);
        let mut row = Exchange {
            id: uuid::Uuid::now_v7().to_string(),
            started_at: timestamp(f.started_at),
            finished_at: timestamp(finished),
            trace_id: r.trace_id.clone().unwrap_or_default(),
            client_id: r.client_id.clone().unwrap_or_default(),
            session_key: f.session_key.to_string(),
            session_source: f.session_source.to_string(),
            provider: r.provider.clone(),
            account_id: r.account_id.clone().unwrap_or_default(),
            model: r.model.clone().unwrap_or_default(),
            upstream_model: f.upstream_model.to_string(),
            client_endpoint: self.endpoint.clone(),
            endpoint: r.endpoint.clone(),
            status: r.status,
            latency_ms: r.latency_ms.max(0) as u64,
            input_tokens: f.usage.input,
            output_tokens: f.usage.output,
            cached_tokens: f.usage.cached,
            cache_write_tokens: f.usage.cache_write,
            context_tokens: r.context_tokens,
            context_window: r.context_window,
            cost: r.api_equivalent_cost,
            tool_calls: f.tools.to_vec(),
            request: String::from_utf8_lossy(&self.request).into_owned(),
            request_truncated: self.request_truncated,
            ..Default::default()
        };
        let Some(resp) = f.response else { return row };
        row.streamed = resp.sse;
        row.response_truncated = resp.truncated;
        let success = (200..300).contains(&r.status);
        let path = r.endpoint.split('?').next().unwrap_or_default();
        match Wire::for_path(path) {
            Some(wire) if success => {
                let (out, error) = translate::assemble(wire, &resp.bytes, resp.sse);
                match error {
                    Some((kind, message)) => row.error = error_text(&kind, &message),
                    None => row.stop_reason = stop_reason(out.stop).to_string(),
                }
                row.output_text = output_text(&out);
                row.output = output_json(&out).to_string();
            }
            _ if !success => {
                let status = axum::http::StatusCode::from_u16(r.status).unwrap_or(axum::http::StatusCode::BAD_GATEWAY);
                let (kind, message) = translate::upstream_error(status, &resp.bytes);
                row.error = error_text(&kind, &message);
            }
            _ => {}
        }
        // The captured bytes become the column: a valid body is not copied.
        row.response = String::from_utf8(resp.bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
        row
    }
}

/// `kind: message`, short: an error body without a message is all message.
fn error_text(kind: &str, message: &str) -> String {
    truncate(format!("{kind}: {message}"), MAX_ERROR_BYTES).0
}

fn timestamp(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%d %H:%M:%S%.3f").to_string()
}

fn stop_reason(s: ir::StopReason) -> &'static str {
    match s {
        ir::StopReason::End => "end",
        ir::StopReason::MaxTokens => "max_tokens",
        ir::StopReason::ToolUse => "tool_use",
        ir::StopReason::StopSequence => "stop_sequence",
        ir::StopReason::ContentFilter => "content_filter",
    }
}

fn output_text(r: &ir::Response) -> String {
    r.blocks
        .iter()
        .filter_map(|b| match b {
            ir::Block::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn output_json(r: &ir::Response) -> Value {
    let blocks: Vec<Value> = r
        .blocks
        .iter()
        .map(|b| match b {
            ir::Block::Text(t) => json!({"type": "text", "text": t}),
            ir::Block::Reasoning(x) => json!({"type": "reasoning", "text": x.text}),
            ir::Block::ToolCall(c) => json!({"type": "tool_call", "id": c.id, "name": c.name, "arguments": c.arguments}),
        })
        .collect();
    Value::Array(blocks)
}

/// Keep at most `limit` bytes of `s`, cut on a character boundary.
fn truncate(mut s: String, limit: usize) -> (String, bool) {
    if s.len() <= limit {
        return (s, false);
    }
    let mut end = limit;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    s.shrink_to_fit();
    (s, true)
}

/// At most `limit` bytes of `b`, cut on a UTF-8 character boundary. The
/// prefix shares `b`'s buffer, so it costs the same whatever the length.
fn prefix(b: &Bytes, limit: usize) -> (Bytes, bool) {
    if b.len() <= limit {
        return (b.clone(), false);
    }
    let mut end = limit;
    // Back off continuation bytes (`10xxxxxx`) to the start of a character.
    while end > 0 && b[end] & 0xC0 == 0x80 {
        end -= 1;
    }
    (b.slice(..end), true)
}

/// The ClickHouse HTTP interface.
#[derive(Clone)]
struct Client {
    http: reqwest::Client,
    url: String,
    user: Option<String>,
    password: Option<String>,
}

impl Client {
    /// Run `query` with `data` as its input. A query without input is sent as
    /// the body: ClickHouse refuses a POST without a `Content-Length`, which
    /// an empty body doesn't get.
    async fn query(&self, query: &str, data: Bytes, settings: &[(&str, &str)]) -> Result<()> {
        let mut url = url::Url::parse(&self.url)?;
        let body = if data.is_empty() {
            Bytes::copy_from_slice(query.as_bytes())
        } else {
            url.query_pairs_mut().append_pair("query", query);
            data
        };
        // `wait_end_of_query`: the status says how the query ended, not how it began.
        url.query_pairs_mut()
            .append_pair("input_format_skip_unknown_fields", "1")
            .append_pair("date_time_input_format", "best_effort")
            .append_pair("wait_end_of_query", "1")
            .extend_pairs(settings);
        let mut req = self.http.post(url);
        if let Some(u) = &self.user {
            req = req.header("X-ClickHouse-User", u);
        }
        if let Some(p) = &self.password {
            req = req.header("X-ClickHouse-Key", p);
        }
        let resp = req.body(body).send().await.context("sending to ClickHouse")?;
        let status = resp.status();
        let failed = resp.headers().contains_key("x-clickhouse-exception-code");
        let text = resp.text().await.context("reading ClickHouse's answer")?;
        if !status.is_success() || failed || text.contains("DB::Exception") {
            anyhow::bail!("ClickHouse returned {status}: {}", text.trim().chars().take(500).collect::<String>());
        }
        Ok(())
    }
}

/// The table xolotl creates when `create_table` is on.
pub fn create_table_sql(cfg: &ClickHouseConfig) -> String {
    let ttl = if cfg.ttl_days > 0 { format!("\nTTL toDateTime(started_at) + INTERVAL {} DAY", cfg.ttl_days) } else { String::new() };
    format!(
        "CREATE TABLE IF NOT EXISTS {db}.{table} (
    id UUID,
    started_at DateTime64(3, 'UTC'),
    finished_at DateTime64(3, 'UTC'),
    trace_id String,
    client_id LowCardinality(String),
    session_key String,
    session_source LowCardinality(String),
    provider LowCardinality(String),
    account_id LowCardinality(String),
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
ENGINE = MergeTree
PARTITION BY toYYYYMM(started_at)
ORDER BY (client_id, session_key, started_at){ttl}
SETTINGS non_replicated_deduplication_window = 1000",
        db = cfg.database,
        table = cfg.table,
    )
}

/// Queues exchanges and inserts them into ClickHouse in the background.
pub struct Exporter {
    cfg: ClickHouseConfig,
    tx: mpsc::UnboundedSender<Exchange>,
    /// Bytes of rows queued or being inserted.
    queued: Arc<AtomicUsize>,
    /// Rows taken but not yet queued ([`Pending`]).
    in_flight: AtomicUsize,
    stop: Mutex<Option<oneshot::Sender<()>>>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Exporter {
    /// Start the exporter. `user` and `password` come from the vault; the
    /// vault's user wins over the configured one.
    pub fn start(cfg: ClickHouseConfig, user: Option<String>, password: Option<String>) -> Result<Arc<Self>> {
        let mut builder = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::none());
        if let Some(ca) = &cfg.ca_cert {
            let pem = std::fs::read(ca).with_context(|| format!("reading ClickHouse CA {}", ca.display()))?;
            for cert in reqwest::Certificate::from_pem_bundle(&pem).context("parsing ClickHouse CA bundle")? {
                builder = builder.add_root_certificate(cert);
            }
        }
        let http = builder.build()?;
        let client = Client { http, url: cfg.url.clone(), user: user.or_else(|| cfg.user.clone()), password };
        let (tx, rx) = mpsc::unbounded_channel();
        let (stop_tx, stop_rx) = oneshot::channel();
        let queued = Arc::new(AtomicUsize::new(0));
        let worker = Worker { cfg: cfg.clone(), client, queued: queued.clone(), table_ready: !cfg.create_table };
        let task = tokio::spawn(worker.run(rx, stop_rx));
        tracing::info!(url = %cfg.url, database = %cfg.database, table = %cfg.table, bodies = cfg.bodies, "ClickHouse export enabled");
        Ok(Arc::new(Self {
            cfg,
            tx,
            queued,
            in_flight: AtomicUsize::new(0),
            stop: Mutex::new(Some(stop_tx)),
            task: Mutex::new(Some(task)),
        }))
    }

    /// The request side of an exchange on `endpoint`. `body` is the request
    /// as the client sent it; the capture shares its buffer and copies nothing.
    pub fn capture(self: &Arc<Self>, endpoint: &str, body: &Bytes) -> Arc<Capture> {
        let (request, request_truncated) = if self.cfg.bodies { prefix(body, self.cfg.max_body_bytes) } else { (Bytes::new(), false) };
        Arc::new(Capture { exporter: self.clone(), endpoint: endpoint.to_string(), request, request_truncated })
    }

    fn enqueue(&self, row: Exchange) {
        let size = row.size();
        let limit = self.cfg.queue_mb.saturating_mul(1024 * 1024);
        let fits = self.queued.fetch_add(size, Ordering::AcqRel) + size <= limit;
        if !fits || self.tx.send(row).is_err() {
            self.queued.fetch_sub(size, Ordering::AcqRel);
            count("dropped", 1);
            tracing::debug!("ClickHouse export queue full; row dropped");
        }
    }

    /// Wait for the rows being built, insert what is queued, and stop.
    pub async fn shutdown(&self) {
        let deadline = tokio::time::Instant::now() + SHUTDOWN_WAIT;
        while self.in_flight.load(Ordering::Acquire) > 0 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        if let Some(stop) = self.stop.lock().take() {
            let _ = stop.send(());
        }
        let task = self.task.lock().take();
        if let Some(task) = task
            && tokio::time::timeout_at(deadline, task).await.is_err()
        {
            tracing::warn!("ClickHouse export did not finish before shutdown; queued rows are lost");
        }
    }
}

fn count(outcome: &'static str, n: usize) {
    metrics().export_rows.add(n as u64, &[KeyValue::new("sink", "clickhouse"), KeyValue::new("outcome", outcome)]);
}

struct Worker {
    cfg: ClickHouseConfig,
    client: Client,
    queued: Arc<AtomicUsize>,
    table_ready: bool,
}

impl Worker {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Exchange>, mut stop: oneshot::Receiver<()>) {
        let mut tick = tokio::time::interval(Duration::from_secs(self.cfg.flush_secs.max(1)));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut batch = Vec::new();
        let mut bytes = 0;
        loop {
            tokio::select! {
                row = rx.recv() => {
                    let Some(row) = row else { break };
                    bytes += row.size();
                    batch.push(row);
                    if batch.len() >= self.cfg.batch_rows || bytes >= MAX_BATCH_BYTES {
                        self.flush(&mut batch, &mut bytes).await;
                    }
                }
                _ = tick.tick() => self.flush(&mut batch, &mut bytes).await,
                _ = &mut stop => break,
            }
        }
        rx.close();
        while let Ok(row) = rx.try_recv() {
            batch.push(row);
        }
        while !batch.is_empty() {
            let rest = batch.split_off(batch.len().min(self.cfg.batch_rows));
            let mut size = batch.iter().map(Exchange::size).sum();
            self.flush(&mut batch, &mut size).await;
            batch = rest;
        }
    }

    async fn flush(&mut self, batch: &mut Vec<Exchange>, bytes: &mut usize) {
        if batch.is_empty() {
            return;
        }
        let rows = std::mem::take(batch);
        let size = std::mem::take(bytes);
        let mut body = Vec::with_capacity(size);
        for row in &rows {
            if serde_json::to_writer(&mut body, row).is_ok() {
                body.push(b'\n');
            }
        }
        // Shared by every try, not copied.
        let body = Bytes::from(body);
        let query = format!("INSERT INTO {}.{} FORMAT JSONEachRow", self.cfg.database, self.cfg.table);
        // A retry of an insert that landed but wasn't acknowledged is dropped by ClickHouse.
        let token = uuid::Uuid::now_v7().to_string();
        let mut result = Ok(());
        for attempt in 0..INSERT_ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
            }
            result = self.insert(&query, body.clone(), &token).await;
            if result.is_ok() {
                break;
            }
        }
        self.queued.fetch_sub(rows.iter().map(Exchange::size).sum(), Ordering::AcqRel);
        match result {
            Ok(()) => count("inserted", rows.len()),
            Err(e) => {
                count("failed", rows.len());
                tracing::warn!(error = %format!("{e:#}"), rows = rows.len(), "ClickHouse export insert failed; rows dropped");
            }
        }
    }

    async fn insert(&mut self, query: &str, body: Bytes, token: &str) -> Result<()> {
        if !self.table_ready {
            self.client.query(&create_table_sql(&self.cfg), Bytes::new(), &[]).await.context("creating the export table")?;
            self.table_ready = true;
        }
        self.client.query(query, body, &[("insert_deduplication_token", token)]).await
    }
}

#[cfg(test)]
mod tests;
