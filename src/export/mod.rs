//! Export of LLM exchanges for analysis: each session request and its
//! response becomes a row, sent to one or more destinations. [`clickhouse`]
//! inserts rows into a ClickHouse table in batches; [`nats`] publishes each
//! row to a NATS JetStream stream that ClickHouse loads. Exporting never holds
//! up or fails a request: each destination has its own queue bounded by bytes,
//! and a row that doesn't fit is dropped and counted.
// ---
// relationships:
//   implements: ../docs/design/export.md
// ---

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use opentelemetry::KeyValue;
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use crate::catalog::Wire;
use crate::db::RequestRecord;
use crate::proxy::TokenUsage;
use crate::telemetry::metrics;
use crate::translate::{self, ir};

pub mod clickhouse;
pub mod nats;

/// Longer `error` values are cut.
const MAX_ERROR_BYTES: usize = 4096;
/// How long shutdown waits for the last rows.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(10);

/// One request and its response: a row of the exchange table.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
#[cfg_attr(test, derive(serde::Deserialize))]
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

    /// The first `limit` bytes, as if they had been captured with that limit.
    fn limited(&self, limit: usize) -> Self {
        let cut = self.bytes.len() > limit;
        Self { bytes: self.bytes[..self.bytes.len().min(limit)].to_vec(), sse: self.sse, truncated: self.truncated || cut }
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
        self.exporter.bodies.then_some(self.exporter.max_body_bytes)
    }

    /// Hold the exporter open for a row that is being built: shutdown waits
    /// for it before the destinations send what is queued.
    pub fn pending(self: &Arc<Self>) -> Pending {
        self.exporter.in_flight.fetch_add(1, Ordering::AcqRel);
        Pending { capture: self.clone() }
    }

    /// Build the row and queue it. A destination that keeps less than the
    /// capture gets its own row, built from what it keeps, so that its
    /// `output` is what it would have been on its own. The rows share an `id`.
    pub fn submit(&self, mut f: Finished<'_>) {
        let id = uuid::Uuid::now_v7().to_string();
        let response = f.response.take();
        let mut rows = Vec::with_capacity(self.exporter.settings.len());
        for &(bodies, max) in self.exporter.settings.iter().skip(1) {
            let (request, cut) = if bodies { prefix(&self.request, max) } else { (Bytes::new(), false) };
            let response = if bodies { response.as_ref().map(|r| r.limited(max)) } else { None };
            rows.push(Arc::new(self.build(&id, &f, &request, bodies && (self.request_truncated || cut), response)));
        }
        // The capture's own row takes the response bytes without a copy.
        rows.insert(0, Arc::new(self.build(&id, &f, &self.request, self.request_truncated, response)));
        self.exporter.enqueue(&rows);
    }

    #[cfg(test)]
    fn exchange(&self, mut f: Finished<'_>) -> Exchange {
        let response = f.response.take();
        self.build(&uuid::Uuid::now_v7().to_string(), &f, &self.request, self.request_truncated, response)
    }

    fn build(&self, id: &str, f: &Finished<'_>, request: &Bytes, request_truncated: bool, response: Option<Captured>) -> Exchange {
        let r = f.record;
        let finished = f.started_at + chrono::Duration::milliseconds(r.latency_ms);
        let mut row = Exchange {
            id: id.to_string(),
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
            request: String::from_utf8_lossy(request).into_owned(),
            request_truncated,
            ..Default::default()
        };
        let Some(resp) = response else { return row };
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

/// Where rows go: a queue bounded by bytes, and the task that empties it.
pub struct Sink {
    name: &'static str,
    /// Which of the exporter's body settings this destination's rows are built with.
    setting: usize,
    /// Whether the destination takes the request and response bodies.
    bodies: bool,
    max_body_bytes: usize,
    /// Most bytes of rows the queue holds.
    limit: usize,
    tx: mpsc::UnboundedSender<Arc<Exchange>>,
    /// Bytes of rows queued or being sent.
    queued: Arc<AtomicUsize>,
    stop: Mutex<Option<oneshot::Sender<()>>>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// The receiving end of a sink's queue, for its task.
pub struct Queue {
    pub rx: mpsc::UnboundedReceiver<Arc<Exchange>>,
    /// Fires when the exporter shuts down: send what is queued, then stop.
    pub stop: oneshot::Receiver<()>,
    queued: Arc<AtomicUsize>,
}

impl Queue {
    /// `row` has left the queue: sent, refused or dropped.
    pub fn done(&self, row: &Exchange) {
        self.queued.fetch_sub(row.size(), Ordering::AcqRel);
    }
}

impl Sink {
    /// Start the task that empties the queue. `bodies` and `max_body_bytes`
    /// are what the destination keeps of each row; `queue_mb` bounds the queue.
    pub fn spawn<F, Fut>(name: &'static str, bodies: bool, max_body_bytes: usize, queue_mb: usize, run: F) -> Self
    where
        F: FnOnce(Queue) -> Fut,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let (tx, rx) = mpsc::unbounded_channel();
        let (stop_tx, stop) = oneshot::channel();
        let queued = Arc::new(AtomicUsize::new(0));
        let task = tokio::spawn(run(Queue { rx, stop, queued: queued.clone() }));
        Self {
            name,
            setting: 0,
            bodies,
            max_body_bytes,
            limit: queue_mb.saturating_mul(1024 * 1024),
            tx,
            queued,
            stop: Mutex::new(Some(stop_tx)),
            task: Mutex::new(Some(task)),
        }
    }

    fn enqueue(&self, row: Arc<Exchange>) {
        let size = row.size();
        let fits = self.queued.fetch_add(size, Ordering::AcqRel) + size <= self.limit;
        if !fits || self.tx.send(row).is_err() {
            self.queued.fetch_sub(size, Ordering::AcqRel);
            count(self.name, "dropped", 1);
            tracing::debug!(sink = self.name, "export queue full; row dropped");
        }
    }

    /// Bytes of rows queued or being sent.
    pub fn queued(&self) -> usize {
        self.queued.load(Ordering::Acquire)
    }
}

/// Count rows that reached an outcome at a destination, in `xolotl.export.rows`.
pub fn count(sink: &'static str, outcome: &'static str, n: usize) {
    metrics().export_rows.add(n as u64, &[KeyValue::new("sink", sink), KeyValue::new("outcome", outcome)]);
}

/// Takes the rows of session exchanges and hands each to every destination.
pub struct Exporter {
    sinks: Vec<Sink>,
    /// What the capture keeps: what the most generous destination wants.
    bodies: bool,
    max_body_bytes: usize,
    /// The distinct `(bodies, max_body_bytes)` of the destinations, the
    /// capture's own first.
    settings: Vec<(bool, usize)>,
    /// Rows taken but not yet queued ([`Pending`]).
    in_flight: AtomicUsize,
}

impl Exporter {
    pub fn new(mut sinks: Vec<Sink>) -> Arc<Self> {
        let bodies = sinks.iter().any(|s| s.bodies);
        let max_body_bytes = sinks.iter().filter(|s| s.bodies).map(|s| s.max_body_bytes).max().unwrap_or(0);
        let mut settings = vec![(bodies, max_body_bytes)];
        for sink in &mut sinks {
            let own = if sink.bodies { (true, sink.max_body_bytes) } else { (false, 0) };
            sink.setting = settings.iter().position(|s| *s == own).unwrap_or_else(|| {
                settings.push(own);
                settings.len() - 1
            });
        }
        Arc::new(Self { sinks, bodies, max_body_bytes, settings, in_flight: AtomicUsize::new(0) })
    }

    /// The destinations, in the order they were given.
    pub fn sinks(&self) -> &[Sink] {
        &self.sinks
    }

    /// The request side of an exchange on `endpoint`. `body` is the request
    /// as the client sent it; the capture shares its buffer and copies nothing.
    pub fn capture(self: &Arc<Self>, endpoint: &str, body: &Bytes) -> Arc<Capture> {
        let (request, request_truncated) = if self.bodies { prefix(body, self.max_body_bytes) } else { (Bytes::new(), false) };
        Arc::new(Capture { exporter: self.clone(), endpoint: endpoint.to_string(), request, request_truncated })
    }

    /// Queue each destination's row: `rows` follows `settings`.
    fn enqueue(&self, rows: &[Arc<Exchange>]) {
        for sink in &self.sinks {
            sink.enqueue(rows[sink.setting].clone());
        }
    }

    /// Queue one row, as built, for every destination.
    #[cfg(test)]
    fn enqueue_row(&self, row: Exchange) {
        let row = Arc::new(row);
        for sink in &self.sinks {
            sink.enqueue(row.clone());
        }
    }

    /// Wait for the rows being built, have each destination send what is
    /// queued, and stop.
    pub async fn shutdown(&self) {
        let deadline = tokio::time::Instant::now() + SHUTDOWN_WAIT;
        while self.in_flight.load(Ordering::Acquire) > 0 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        for sink in &self.sinks {
            if let Some(stop) = sink.stop.lock().take() {
                let _ = stop.send(());
            }
        }
        for sink in &self.sinks {
            let task = sink.task.lock().take();
            if let Some(task) = task
                && tokio::time::timeout_at(deadline, task).await.is_err()
            {
                tracing::warn!(sink = sink.name, "export did not finish before shutdown; queued rows are lost");
            }
        }
    }
}
#[cfg(test)]
mod tests;
