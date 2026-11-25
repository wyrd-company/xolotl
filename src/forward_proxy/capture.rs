//! The capture of model requests on an intercepted host. Forwarding hands
//! each request's heads and frames to a capture task of its own, through a
//! queue bounded by bytes, and never waits for it. The capture task keeps a
//! bounded copy of each body, reads the model, session and usage from them,
//! and submits one [`Forwarded`] to the [`CaptureSink`].

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use bytes::Bytes;
use chrono::{DateTime, Utc};
use http::HeaderMap;
use opentelemetry::{KeyValue, metrics::Counter};
use parking_lot::Mutex;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::context::SessionId;
use crate::export::Captured;
use crate::proxy::TokenUsage;

/// The model calls of the Code Assist API: only these are captured.
const MODEL_PATHS: [&str; 2] = ["/v1internal:streamGenerateContent", "/v1internal:generateContent"];
/// Bytes of frames waiting for the capture task, both directions together.
pub(super) const QUEUE_LIMIT: usize = 8 * 1024 * 1024;
/// The most of one SSE event, or of a JSON response, read for metadata.
const MAX_JSON_SCAN: usize = 4 * 1024 * 1024;
/// The longest `model` or `request.session_id` kept.
const MAX_FIELD: usize = 256;
const MAX_TOOL_CALLS: usize = 256;
const MAX_TOOL_NAME: usize = 128;
const SESSION_SOURCE: &str = "cloud-code";
/// A key longer than this is never one of the keys the scanner looks for.
const MAX_KEY: usize = 16;

/// One captured request on an intercepted host.
#[derive(Debug, Clone)]
#[cfg_attr(not(test), expect(dead_code, reason = "the export builds its rows from these fields"))]
pub struct Forwarded {
    pub client_id: String,
    pub host: String,
    /// The path without its query string.
    pub path: String,
    /// Xolotl's own answer when it answered; otherwise the upstream's; 499
    /// when neither answered.
    pub status: u16,
    /// From the guard: the UTC start, and the time to the end of forwarding.
    pub started_at: DateTime<Utc>,
    pub latency_ms: u64,
    pub trace_id: String,
    /// Decoded copies, cut at `body_limit()`; `truncated` when cut, when
    /// bytes were left out, or when not copied; `sse` for `text/event-stream`.
    pub request: Captured,
    pub response: Captured,
    pub observed: Observed,
    /// Xolotl's own failure, or why the request ended early.
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Observed {
    pub model: Option<String>,
    /// The session digest, never the raw value.
    pub session_key: Option<String>,
    pub usage: TokenUsage,
    /// One entry per call, repeats included.
    pub tool_calls: Vec<String>,
    /// False when a limit skipped anything this would have read.
    pub complete: bool,
}

pub trait CaptureSink: Send + Sync {
    /// The most body bytes to copy, or `None` when no destination takes bodies.
    fn body_limit(&self) -> Option<usize>;
    /// Hold the sink open for one request, from admission until its rows are
    /// queued. Shutdown waits for every hold.
    fn hold(&self) -> CaptureHold;
}

/// Keeps the export open for one request's capture.
pub struct CaptureHold {
    submit: Option<Box<dyn FnOnce(Forwarded) + Send>>,
}

impl CaptureHold {
    /// A hold whose `submit` runs `f`. Dropping the hold without submitting
    /// drops `f`, which releases whatever it holds.
    #[cfg_attr(not(test), expect(dead_code, reason = "the export makes the holds"))]
    pub fn new(f: impl FnOnce(Forwarded) + Send + 'static) -> Self {
        Self { submit: Some(Box::new(f)) }
    }

    /// Build the rows, queue them on every destination, then release the hold.
    /// Runs on the capture task and can't fail the request.
    pub fn submit(mut self, forwarded: Forwarded) {
        if let Some(submit) = self.submit.take() {
            submit(forwarded);
        }
    }
}

/// Whether a request on a capture host is a model call.
pub(super) fn is_model_request(method: &http::Method, path: &str) -> bool {
    method == http::Method::POST && MODEL_PATHS.contains(&path)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Direction {
    Request = 0,
    Response = 1,
}

/// What the capture needs of a head.
#[derive(Debug)]
pub(super) struct Head {
    status: Option<u16>,
    text: bool,
    sse: bool,
    encoding: Option<String>,
}

impl Head {
    pub fn new(status: Option<u16>, headers: &HeaderMap) -> Self {
        let content_type = headers.get(http::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or_default();
        let sse = content_type.split(';').next().unwrap_or_default().trim().eq_ignore_ascii_case("text/event-stream");
        let encoding: Vec<&str> = headers
            .get_all(http::header::CONTENT_ENCODING)
            .iter()
            .flat_map(|v| v.to_str().unwrap_or("unreadable").split(','))
            .map(str::trim)
            .filter(|coding| !coding.is_empty() && !coding.eq_ignore_ascii_case("identity"))
            .collect();
        let encoding = (!encoding.is_empty()).then(|| encoding.join(", "));
        Self { status, text: crate::proxy::is_text(headers), sse, encoding }
    }
}

enum Event {
    Head(Direction, Head),
    Frame(Direction, Bytes),
    End(Direction),
    Terminal(Terminal),
}

struct Terminal {
    status: u16,
    error: Option<String>,
    relayed: [u64; 2],
    queued: [u64; 2],
    started_at: DateTime<Utc>,
    latency_ms: u64,
}

/// Who and what a captured request is, known at admission.
pub(super) struct Admission {
    pub client_id: String,
    pub host: String,
    pub path: String,
    pub trace_id: String,
}

/// The forwarding side of one captured request. Every part of forwarding
/// holds a clone; the last one dropped sends the terminal event.
#[derive(Clone)]
pub(super) struct Tap {
    inner: Arc<Guard>,
}

struct Guard {
    tx: mpsc::UnboundedSender<Event>,
    waiting: Arc<AtomicUsize>,
    overflow: AtomicBool,
    started_at: DateTime<Utc>,
    started: Instant,
    state: Mutex<GuardState>,
}

#[derive(Default)]
struct GuardState {
    relayed: [u64; 2],
    queued: [u64; 2],
    /// The status of the last response head.
    response_status: Option<u16>,
    outcome: Option<(u16, Option<String>)>,
}

impl Tap {
    /// Admit a request: take a hold from the sink, and start its capture task.
    pub fn admit(sink: &dyn CaptureSink, admission: Admission, max_decoded: usize, requests: Counter<u64>) -> Self {
        let hold = sink.hold();
        let (tx, rx) = mpsc::unbounded_channel();
        let waiting = Arc::new(AtomicUsize::new(0));
        let task = CaptureTask { rx, waiting: waiting.clone(), body_limit: sink.body_limit(), max_decoded, requests };
        tokio::spawn(task.run(hold, admission));
        let inner = Guard {
            tx,
            waiting,
            overflow: AtomicBool::new(false),
            started_at: Utc::now(),
            started: Instant::now(),
            state: Mutex::new(GuardState::default()),
        };
        Self { inner: Arc::new(inner) }
    }

    pub fn head(&self, direction: Direction, head: Head) {
        if direction == Direction::Response {
            self.inner.state.lock().response_status = head.status;
        }
        let _ = self.inner.tx.send(Event::Head(direction, head));
    }

    /// A data frame, already on its way to the other side.
    pub fn frame(&self, direction: Direction, data: &Bytes) {
        let len = data.len();
        let mut state = self.inner.state.lock();
        state.relayed[direction as usize] += len as u64;
        if self.inner.overflow.load(Ordering::Acquire) {
            return;
        }
        if self.inner.waiting.fetch_add(len, Ordering::AcqRel) + len > QUEUE_LIMIT {
            self.inner.waiting.fetch_sub(len, Ordering::AcqRel);
            self.inner.overflow.store(true, Ordering::Release);
            return;
        }
        state.queued[direction as usize] += len as u64;
        drop(state);
        let _ = self.inner.tx.send(Event::Frame(direction, data.clone()));
    }

    pub fn end(&self, direction: Direction) {
        let _ = self.inner.tx.send(Event::End(direction));
    }

    /// The outcome of the request. The first one set is kept.
    pub fn finish(&self, status: u16, error: Option<String>) {
        self.inner.state.lock().outcome.get_or_insert((status, error));
    }

    /// The upstream's status, when its response head arrived.
    pub fn response_status(&self) -> Option<u16> {
        self.inner.state.lock().response_status
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        let state = std::mem::take(&mut *self.state.lock());
        let (status, error) = state
            .outcome
            .unwrap_or_else(|| (state.response_status.unwrap_or(499), Some("incomplete: the request ended without an outcome".to_owned())));
        let _ = self.tx.send(Event::Terminal(Terminal {
            status,
            error,
            relayed: state.relayed,
            queued: state.queued,
            started_at: self.started_at,
            latency_ms: self.started.elapsed().as_millis() as u64,
        }));
    }
}

struct CaptureTask {
    rx: mpsc::UnboundedReceiver<Event>,
    waiting: Arc<AtomicUsize>,
    body_limit: Option<usize>,
    max_decoded: usize,
    requests: Counter<u64>,
}

impl CaptureTask {
    async fn run(mut self, hold: CaptureHold, admission: Admission) {
        let mut bodies = [BodyCapture::new(Direction::Request), BodyCapture::new(Direction::Response)];
        while let Some(event) = self.rx.recv().await {
            match event {
                Event::Head(direction, head) => bodies[direction as usize].head(head, self.body_limit),
                Event::Frame(direction, data) => {
                    self.waiting.fetch_sub(data.len(), Ordering::AcqRel);
                    bodies[direction as usize].frame(&data, self.max_decoded);
                }
                Event::End(direction) => bodies[direction as usize].end(),
                Event::Terminal(terminal) => {
                    let skipped = bodies.iter().any(|body| body.skipped);
                    let (forwarded, cut) = forwarded(admission, bodies, &terminal);
                    let capture = if skipped {
                        "skipped"
                    } else if cut || !forwarded.observed.complete {
                        "cut"
                    } else {
                        "full"
                    };
                    self.requests.add(
                        1,
                        &[
                            KeyValue::new("host", forwarded.host.clone()),
                            KeyValue::new("status", i64::from(forwarded.status)),
                            KeyValue::new("capture", capture),
                        ],
                    );
                    hold.submit(forwarded);
                    return;
                }
            }
        }
    }
}

/// The request as the capture saw it, and whether a copy was cut.
fn forwarded(admission: Admission, bodies: [BodyCapture; 2], terminal: &Terminal) -> (Forwarded, bool) {
    let [request, response] = bodies;
    let lost = |d: Direction| terminal.relayed[d as usize] > terminal.queued[d as usize];
    let complete = request.complete() && response.complete() && !lost(Direction::Request) && !lost(Direction::Response);
    let observed = Observed {
        model: request.scanner.model.clone(),
        session_key: request.scanner.session.as_deref().map(|value| SessionId::sent(SESSION_SOURCE, value).key),
        usage: response.usage,
        tool_calls: response.tools.clone(),
        complete,
    };
    let (request, request_cut) = request.into_captured(lost(Direction::Request));
    let (response, response_cut) = response.into_captured(lost(Direction::Response));
    let forwarded = Forwarded {
        client_id: admission.client_id,
        host: admission.host,
        path: admission.path,
        status: terminal.status,
        started_at: terminal.started_at,
        latency_ms: terminal.latency_ms,
        trace_id: admission.trace_id,
        request,
        response,
        observed,
        error: terminal.error.clone(),
    };
    (forwarded, request_cut || response_cut)
}

/// One direction's body, as the capture task sees it.
struct BodyCapture {
    direction: Direction,
    copy: Option<(Captured, usize)>,
    /// Bytes arrived with no copy kept.
    uncopied: bool,
    sse: bool,
    /// Whether the body is still read for metadata.
    readable: bool,
    /// The body is in a coding xolotl doesn't decode.
    skipped: bool,
    decoded: usize,
    scanner: Scanner,
    json: Vec<u8>,
    events: Sse,
    usage: TokenUsage,
    tools: Vec<String>,
    complete: bool,
}

impl BodyCapture {
    fn new(direction: Direction) -> Self {
        Self {
            direction,
            copy: None,
            uncopied: false,
            sse: false,
            readable: true,
            skipped: false,
            decoded: 0,
            scanner: Scanner::default(),
            json: Vec::new(),
            events: Sse::default(),
            usage: TokenUsage::default(),
            tools: Vec::new(),
            complete: true,
        }
    }

    fn head(&mut self, head: Head, body_limit: Option<usize>) {
        self.sse = head.sse;
        if head.encoding.is_some() {
            // A coding xolotl doesn't decode: the body is neither copied nor read.
            self.readable = false;
            self.skipped = true;
            self.complete = false;
        }
        self.copy = (self.readable && head.text)
            .then_some(body_limit)
            .flatten()
            .map(|limit| (Captured { sse: head.sse, ..Default::default() }, limit));
    }

    fn frame(&mut self, data: &[u8], max_decoded: usize) {
        match &mut self.copy {
            Some((captured, limit)) => captured.push(data, *limit),
            None => self.uncopied |= !data.is_empty(),
        }
        if !self.readable {
            return;
        }
        let room = max_decoded.saturating_sub(self.decoded);
        let data = if data.len() > room {
            self.complete = false;
            self.readable = false;
            &data[..room]
        } else {
            data
        };
        self.decoded += data.len();
        match (self.direction, self.sse) {
            (Direction::Request, _) => self.scanner.feed(data),
            (Direction::Response, true) => {
                for event in self.events.feed(data) {
                    observe(&event, &mut self.usage, &mut self.tools, &mut self.complete);
                }
            }
            (Direction::Response, false) => {
                if self.json.len() + data.len() > MAX_JSON_SCAN {
                    self.json = Vec::new();
                    self.complete = false;
                    self.readable = false;
                } else {
                    self.json.extend_from_slice(data);
                }
            }
        }
    }

    fn end(&mut self) {
        if !self.readable {
            return;
        }
        match (self.direction, self.sse) {
            (Direction::Request, _) => {}
            (Direction::Response, true) => {
                if let Some(event) = self.events.finish() {
                    observe(&event, &mut self.usage, &mut self.tools, &mut self.complete);
                }
            }
            (Direction::Response, false) => {
                if let Ok(value) = serde_json::from_slice::<Value>(&self.json) {
                    observe(&value, &mut self.usage, &mut self.tools, &mut self.complete);
                }
            }
        }
    }

    /// The copy, and whether it was cut or lost bytes.
    /// Whether the metadata read from this body is whole: no limit skipped
    /// any of it, including a field the scanner dropped or an SSE event over
    /// the scan limit.
    fn complete(&self) -> bool {
        self.complete && !self.scanner.overflow && !self.events.skipped
    }

    fn into_captured(self, lost: bool) -> (Captured, bool) {
        match self.copy {
            Some((mut captured, _)) => {
                let cut = captured.truncated || lost;
                captured.truncated = cut;
                (captured, cut)
            }
            None => (Captured { sse: self.sse, truncated: self.uncopied || lost, bytes: Vec::new() }, lost),
        }
    }
}

/// Read one Code Assist response, the JSON body or one SSE event: the last
/// `usageMetadata`, and each `functionCall`.
fn observe(value: &Value, usage: &mut TokenUsage, tools: &mut Vec<String>, complete: &mut bool) {
    let response = value.get("response").unwrap_or(value);
    if let Some(metadata) = response.get("usageMetadata").filter(|m| m.is_object()) {
        let count = |key: &str| metadata.get(key).and_then(Value::as_i64);
        let output = match (count("candidatesTokenCount"), count("thoughtsTokenCount")) {
            (None, None) => None,
            (candidates, thoughts) => Some(candidates.unwrap_or(0) + thoughts.unwrap_or(0)),
        };
        *usage = TokenUsage { input: count("promptTokenCount"), output, cached: count("cachedContentTokenCount"), cache_write: None };
    }
    let candidates = response.get("candidates").and_then(Value::as_array).into_iter().flatten();
    let parts = candidates.filter_map(|c| c.pointer("/content/parts")?.as_array()).flatten();
    for name in parts.filter_map(|part| part.pointer("/functionCall/name")?.as_str()) {
        if tools.len() >= MAX_TOOL_CALLS {
            *complete = false;
            return;
        }
        tools.push(name.chars().take(MAX_TOOL_NAME).collect());
    }
}

/// The events of an SSE stream, each `data` parsed as JSON.
#[derive(Default)]
struct Sse {
    line: Vec<u8>,
    data: Vec<u8>,
    /// Lines of the current event are being dropped: it is over the limit.
    dropping: bool,
    /// An event was over the limit.
    skipped: bool,
}

impl Sse {
    fn feed(&mut self, mut chunk: &[u8]) -> Vec<Value> {
        let mut events = Vec::new();
        while let Some(end) = chunk.iter().position(|b| *b == b'\n') {
            self.push_line(&chunk[..end]);
            chunk = &chunk[end + 1..];
            let line = std::mem::take(&mut self.line);
            let line = line.strip_suffix(b"\r").unwrap_or(&line);
            if line.is_empty() {
                events.extend(self.dispatch());
            } else if let Some(data) = line.strip_prefix(b"data:") {
                let data = data.strip_prefix(b" ").unwrap_or(data);
                if !self.data.is_empty() {
                    self.data.push(b'\n');
                }
                self.data.extend_from_slice(data);
                if self.data.len() > MAX_JSON_SCAN {
                    self.data = Vec::new();
                    self.dropping = true;
                    self.skipped = true;
                }
            }
        }
        self.push_line(chunk);
        events
    }

    fn push_line(&mut self, bytes: &[u8]) {
        if self.dropping {
            return;
        }
        self.line.extend_from_slice(bytes);
        if self.line.len() > MAX_JSON_SCAN {
            self.line = Vec::new();
            self.dropping = true;
            self.skipped = true;
        }
    }

    fn dispatch(&mut self) -> Option<Value> {
        let dropping = std::mem::take(&mut self.dropping);
        let data = std::mem::take(&mut self.data);
        if dropping || data.is_empty() {
            return None;
        }
        serde_json::from_slice(&data).ok()
    }

    fn finish(&mut self) -> Option<Value> {
        let line = std::mem::take(&mut self.line);
        if !line.is_empty() {
            self.feed(&[line.as_slice(), b"\n"].concat());
        }
        self.dispatch()
    }
}

/// Reads the top-level `model` and `request.session_id` strings of a JSON
/// object as it streams, and keeps nothing else.
#[derive(Default)]
struct Scanner {
    depth: usize,
    /// For depths 1 and 2: whether the container is an object waiting for a key.
    expect_key: [bool; 3],
    is_object: [bool; 3],
    top_key: Option<Vec<u8>>,
    /// Depth 2 is the object under the top-level `request`.
    in_request: bool,
    request_key: Option<Vec<u8>>,
    string: Option<Capture>,
    in_string: bool,
    escape: bool,
    model: Option<String>,
    session: Option<String>,
    /// A field was longer than its limit.
    overflow: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    TopKey,
    RequestKey,
    Model,
    Session,
}

struct Capture {
    role: Role,
    bytes: Vec<u8>,
    overflow: bool,
}

impl Scanner {
    fn feed(&mut self, data: &[u8]) {
        for &byte in data {
            if self.in_string {
                self.string_byte(byte);
                continue;
            }
            match byte {
                b'"' => self.start_string(),
                b'{' | b'[' => {
                    let is_object = byte == b'{';
                    if self.depth == 1 && is_object && !self.expect_key[1] && self.top_key.as_deref() == Some(b"request") {
                        self.in_request = true;
                    }
                    self.depth += 1;
                    if self.depth <= 2 {
                        self.is_object[self.depth] = is_object;
                        self.expect_key[self.depth] = is_object;
                    }
                }
                b'}' | b']' => {
                    if self.depth == 2 {
                        self.in_request = false;
                    }
                    self.depth = self.depth.saturating_sub(1);
                }
                b':' if self.depth <= 2 => self.expect_key[self.depth] = false,
                b',' if self.depth <= 2 => self.expect_key[self.depth] = self.is_object[self.depth],
                _ => {}
            }
        }
    }

    fn start_string(&mut self) {
        self.in_string = true;
        let key = self.depth <= 2 && self.is_object[self.depth] && self.expect_key[self.depth];
        let role = match (self.depth, key) {
            (1, true) => Some(Role::TopKey),
            (2, true) if self.in_request => Some(Role::RequestKey),
            (1, false) if self.top_key.as_deref() == Some(b"model") => Some(Role::Model),
            (2, false) if self.in_request && self.request_key.as_deref() == Some(b"session_id") => Some(Role::Session),
            _ => None,
        };
        self.string = role.map(|role| Capture { role, bytes: Vec::new(), overflow: false });
    }

    fn string_byte(&mut self, byte: u8) {
        if !self.escape && byte == b'"' {
            self.in_string = false;
            if let Some(capture) = self.string.take() {
                self.end_string(capture);
            }
            return;
        }
        self.escape = !self.escape && byte == b'\\';
        if let Some(capture) = &mut self.string {
            let limit = if matches!(capture.role, Role::TopKey | Role::RequestKey) { MAX_KEY } else { MAX_FIELD };
            if capture.bytes.len() < limit {
                capture.bytes.push(byte);
            } else {
                capture.overflow = true;
            }
        }
    }

    fn end_string(&mut self, capture: Capture) {
        let value = || -> Option<String> {
            let quoted = [b"\"".as_slice(), &capture.bytes, b"\""].concat();
            serde_json::from_slice(&quoted).ok()
        };
        match capture.role {
            Role::TopKey => self.top_key = (!capture.overflow).then(|| value().map(String::into_bytes)).flatten(),
            Role::RequestKey => self.request_key = (!capture.overflow).then(|| value().map(String::into_bytes)).flatten(),
            Role::Model | Role::Session if capture.overflow => self.overflow = true,
            Role::Model => self.model = value(),
            Role::Session => self.session = value(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Sink {
        limit: Option<usize>,
        tx: mpsc::UnboundedSender<Forwarded>,
    }

    impl CaptureSink for Sink {
        fn body_limit(&self) -> Option<usize> {
            self.limit
        }

        fn hold(&self) -> CaptureHold {
            let tx = self.tx.clone();
            CaptureHold::new(move |forwarded| tx.send(forwarded).unwrap())
        }
    }

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        pairs.iter().map(|(k, v)| (http::HeaderName::from_static(k), http::HeaderValue::from_static(v))).collect()
    }

    fn admit(limit: Option<usize>) -> (Tap, mpsc::UnboundedReceiver<Forwarded>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let admission = Admission { client_id: "c".into(), host: "h.example".into(), path: "/p".into(), trace_id: String::new() };
        let requests = opentelemetry::global::meter("test").u64_counter("test").build();
        (Tap::admit(&Sink { limit, tx }, admission, 64 * 1024 * 1024, requests), rx)
    }

    /// Frames for `body`, cut into pieces of at most `size`.
    fn frames(body: &[u8], size: usize) -> Vec<Bytes> {
        body.chunks(size).map(Bytes::copy_from_slice).collect()
    }

    const USAGE: &[u8] = b"{\"response\":{\"usageMetadata\":{\"promptTokenCount\":7,\"candidatesTokenCount\":1}}}";

    /// An SSE response of exactly `len` bytes, in events under the scan
    /// limit, whose usage comes last.
    fn response_of(len: usize) -> Vec<u8> {
        let last = [b"data: ".as_slice(), USAGE, b"\n\n"].concat();
        let mut body = Vec::new();
        let mut left = len - last.len();
        while left > 0 {
            let event = left.min(1024 * 1024);
            let pad = event.saturating_sub(b"data: {\"p\":\"\"}\n\n".len());
            let piece = if pad == 0 { vec![b'\n'; event] } else { [b"data: {\"p\":\"".as_slice(), &vec![b'x'; pad], b"\"}\n\n"].concat() };
            left -= piece.len();
            body.extend(piece);
        }
        body.extend(last);
        assert_eq!(body.len(), len);
        body
    }

    /// Queue every event before the capture task runs: the test's runtime
    /// runs it only once the test awaits.
    async fn capture(request: &[u8], response: &[u8]) -> Forwarded {
        let (tap, mut rx) = admit(Some(2 * QUEUE_LIMIT));
        tap.head(Direction::Request, Head::new(None, &headers(&[("content-type", "application/json")])));
        for frame in frames(request, 64 * 1024) {
            tap.frame(Direction::Request, &frame);
        }
        tap.end(Direction::Request);
        tap.head(Direction::Response, Head::new(Some(200), &headers(&[("content-type", "text/event-stream")])));
        for frame in frames(response, 64 * 1024) {
            tap.frame(Direction::Response, &frame);
        }
        tap.end(Direction::Response);
        tap.finish(200, None);
        drop(tap);
        rx.recv().await.unwrap()
    }

    #[tokio::test]
    async fn a_response_that_fills_the_queue_is_complete_and_one_more_byte_is_cut() {
        let request = br#"{"model":"m"}"#;
        let full = capture(request, &response_of(QUEUE_LIMIT - request.len())).await;
        assert!(!full.request.truncated && !full.response.truncated);
        assert_eq!(full.response.bytes.len(), QUEUE_LIMIT - request.len());
        assert!(full.observed.complete);
        assert_eq!(full.observed.usage.input, Some(7));

        let cut = capture(request, &response_of(QUEUE_LIMIT - request.len() + 1)).await;
        assert!(!cut.request.truncated, "the request lost nothing");
        assert!(cut.response.truncated, "the response lost bytes");
        assert!(!cut.observed.complete);
        assert_eq!(cut.observed.usage, TokenUsage::default(), "no token counts from a cut response");
        assert_eq!(cut.observed.model.as_deref(), Some("m"));
    }

    #[tokio::test]
    async fn metadata_a_reader_skipped_leaves_the_observation_incomplete() {
        // An SSE event over the scan limit, then an ordinary usage event.
        let oversized = [b"data: ".as_slice(), &vec![b'x'; MAX_JSON_SCAN + 1], b"\n\n"].concat();
        let response = [oversized, [b"data: ".as_slice(), USAGE, b"\n\n"].concat()].concat();
        let row = capture(br#"{"model":"m"}"#, &response).await;
        assert!(!row.request.truncated && !row.response.truncated, "nothing was lost in the queue or the copy");
        assert_eq!(row.observed.usage.input, Some(7), "the later event is still read");
        assert!(!row.observed.complete, "a skipped event leaves the token counts partial");

        // A `model` longer than its limit.
        let request = format!(r#"{{"model":"{}"}}"#, "m".repeat(MAX_FIELD + 1));
        let row = capture(request.as_bytes(), &[b"data: ".as_slice(), USAGE, b"\n\n"].concat()).await;
        assert_eq!(row.observed.model, None);
        assert!(!row.observed.complete);

        // Without either, the same exchange is complete.
        let row = capture(br#"{"model":"m"}"#, &[b"data: ".as_slice(), USAGE, b"\n\n"].concat()).await;
        assert!(row.observed.complete);
    }

    #[tokio::test]
    async fn bodies_are_copied_to_the_sinks_limit_and_coded_bodies_are_not_read() {
        let (tap, mut rx) = admit(Some(8));
        tap.head(Direction::Request, Head::new(None, &headers(&[("content-type", "application/json"), ("content-encoding", "gzip")])));
        tap.frame(Direction::Request, &Bytes::from_static(b"\x1f\x8b\x08\x00"));
        tap.end(Direction::Request);
        tap.head(Direction::Response, Head::new(Some(200), &headers(&[("content-type", "application/json")])));
        tap.frame(Direction::Response, &Bytes::from_static(USAGE));
        tap.end(Direction::Response);
        tap.finish(200, None);
        drop(tap);
        let row = rx.recv().await.unwrap();
        assert!(row.request.bytes.is_empty() && row.request.truncated, "a coded body is not copied");
        assert_eq!(row.response.bytes, &USAGE[..8]);
        assert!(row.response.truncated);
        assert_eq!(row.observed.usage.input, Some(7), "metadata is read beyond the copy");
        assert!(!row.observed.complete, "the coded request was not read");

        // With no destination taking bodies, nothing is copied and the metadata is still read.
        let (tap, mut rx) = admit(None);
        tap.head(Direction::Response, Head::new(Some(200), &headers(&[])));
        tap.frame(Direction::Response, &Bytes::from_static(USAGE));
        tap.end(Direction::Response);
        tap.finish(200, None);
        drop(tap);
        let row = rx.recv().await.unwrap();
        assert!(row.response.bytes.is_empty());
        assert_eq!(row.observed.usage.output, Some(1));
        assert!(row.observed.complete);
    }

    #[tokio::test]
    async fn a_guard_dropped_without_an_outcome_reports_it() {
        let (tap, mut rx) = admit(Some(1024));
        drop(tap);
        let row = rx.recv().await.unwrap();
        assert_eq!(row.status, 499);
        assert_eq!(row.error.as_deref(), Some("incomplete: the request ended without an outcome"));

        let (tap, mut rx) = admit(Some(1024));
        tap.head(Direction::Response, Head::new(Some(200), &headers(&[])));
        tap.clone().finish(200, Some("proxy_error: first".into()));
        tap.finish(502, Some("second".into()));
        drop(tap);
        let row = rx.recv().await.unwrap();
        assert_eq!((row.status, row.error.as_deref()), (200, Some("proxy_error: first")));
    }

    #[test]
    fn the_scanner_reads_only_the_top_level_model_and_request_session() {
        let scan = |body: &[u8], step: usize| {
            let mut scanner = Scanner::default();
            for chunk in body.chunks(step) {
                scanner.feed(chunk);
            }
            (scanner.model, scanner.session, scanner.overflow)
        };
        let body = r#" {"contents":[{"model":"no"}],"request":{"x":{"session_id":"no"},"session_id":"s\"1é","model":"no"},"model":"gémini","n":[1,{"model":"no"}]}"#;
        let body = body.as_bytes();
        for step in [1, 3, body.len()] {
            assert_eq!(scan(body, step), (Some("gémini".into()), Some("s\"1é".into()), false), "step {step}");
        }
        assert_eq!(scan(br#"[{"model":"no"}]"#, 1), (None, None, false));
        assert_eq!(scan(br#"{"model":7,"request":"session_id"}"#, 1), (None, None, false));
        let long = format!(r#"{{"model":"{}"}}"#, "m".repeat(MAX_FIELD + 1));
        assert_eq!(scan(long.as_bytes(), 7), (None, None, true));
        let exact = format!(r#"{{"model":"{}"}}"#, "m".repeat(MAX_FIELD));
        assert_eq!(scan(exact.as_bytes(), 7).0.map(|m| m.len()), Some(MAX_FIELD));
    }

    #[test]
    fn sse_events_are_read_across_frames() {
        let stream = b"data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"a\"}},{\"functionCall\":{\"name\":\"a\"}}]}}]}}\r\n\r\n: comment\n\ndata: {\"response\":{\"usageMetadata\":{\"promptTokenCount\":3,\"thoughtsTokenCount\":2}}}\n\ndata: {\"response\":{\"usageMetadata\":{\"promptTokenCount\":9}}}";
        for step in [1, 5, stream.len()] {
            let mut sse = Sse::default();
            let mut events: Vec<Value> = stream.chunks(step).flat_map(|chunk| sse.feed(chunk)).collect();
            events.extend(sse.finish());
            let (mut usage, mut tools, mut complete) = (TokenUsage::default(), Vec::new(), true);
            for event in &events {
                observe(event, &mut usage, &mut tools, &mut complete);
            }
            assert_eq!(events.len(), 3, "step {step}");
            assert_eq!(tools, ["a", "a"]);
            assert_eq!(usage, TokenUsage { input: Some(9), ..Default::default() }, "the last usageMetadata");
            assert!(complete && !sse.skipped);
        }
        let mut sse = Sse::default();
        let big = [b"data: ".as_slice(), &vec![b'x'; MAX_JSON_SCAN + 1], b"\n\ndata: {}\n\n"].concat();
        assert_eq!(sse.feed(&big).len(), 1, "the event after the oversized one is read");
        assert!(sse.skipped);
    }

    #[test]
    fn tool_calls_are_bounded() {
        let parts: Vec<Value> = (0..MAX_TOOL_CALLS + 1).map(|_| serde_json::json!({"functionCall": {"name": "n".repeat(200)}})).collect();
        let event = serde_json::json!({"response": {"candidates": [{"content": {"parts": parts}}]}});
        let (mut usage, mut tools, mut complete) = (TokenUsage::default(), Vec::new(), true);
        observe(&event, &mut usage, &mut tools, &mut complete);
        assert_eq!(tools.len(), MAX_TOOL_CALLS);
        assert_eq!(tools[0].len(), MAX_TOOL_NAME);
        assert!(!complete);
    }

    #[test]
    fn only_model_posts_are_captured() {
        assert!(is_model_request(&http::Method::POST, "/v1internal:streamGenerateContent"));
        assert!(is_model_request(&http::Method::POST, "/v1internal:generateContent"));
        assert!(!is_model_request(&http::Method::OPTIONS, "/v1internal:generateContent"));
        assert!(!is_model_request(&http::Method::POST, "/v1internal:countTokens"));
        assert!(!is_model_request(&http::Method::POST, "/v1internal:loadCodeAssist"));
    }
}
