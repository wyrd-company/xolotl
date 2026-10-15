//! Shared upstream relay: header filtering, streaming the upstream response
//! back unchanged, and observing token usage on the way through (SSE or JSON)
//! for the request log and metrics.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

use axum::body::Body;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures::Stream;
use opentelemetry::KeyValue;
use serde_json::Value;

use crate::db::{Db, RequestRecord};
use crate::limits::Limiter;
use crate::telemetry::metrics;

/// Request headers never forwarded upstream: hop-by-hop headers, client
/// credentials (replaced by upstream credentials), and encoding (we ask
/// upstream for identity so usage can be observed).
const STRIP_REQUEST: &[&str] = &[
    "host",
    "authorization",
    "x-api-key",
    "x-management-key",
    "x-goog-api-key",
    "api-key",
    "content-length",
    "connection",
    "keep-alive",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "accept-encoding",
    "cookie",
    "forwarded",
    "x-forwarded-for",
    "x-forwarded-host",
    "x-forwarded-proto",
    "x-real-ip",
];

const STRIP_RESPONSE: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "set-cookie",
    "alt-svc",
    "content-length",
];

/// The HTTP client for every upstream call. Redirects are never followed:
/// requests carry provider credentials (and `api-call` puts account tokens in
/// arbitrary headers), which must not reach another origin. A proxied 3xx is
/// relayed to the client as is.
pub fn upstream_client(read_timeout: std::time::Duration) -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(10))
        .read_timeout(read_timeout)
        .pool_idle_timeout(std::time::Duration::from_secs(90))
        .build()
}

pub fn forward_request_headers(src: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::with_capacity(src.len());
    for (k, v) in src {
        if !STRIP_REQUEST.contains(&k.as_str()) {
            out.append(k.clone(), v.clone());
        }
    }
    out
}

pub fn forward_response_headers(src: &reqwest::header::HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::with_capacity(src.len());
    for (k, v) in src {
        if STRIP_RESPONSE.contains(&k.as_str()) {
            continue;
        }
        if let (Ok(name), Ok(val)) = (HeaderName::from_bytes(k.as_ref()), HeaderValue::from_bytes(v.as_bytes())) {
            out.append(name, val);
        }
    }
    out
}

/// Remove `name` and set it to `value`.
pub fn set_header(h: &mut HeaderMap, name: &'static str, value: &str) {
    if let Ok(v) = HeaderValue::from_str(value) {
        h.insert(HeaderName::from_static(name), v);
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub input: Option<i64>,
    pub output: Option<i64>,
    pub cached: Option<i64>,
}

impl TokenUsage {
    /// Merge usage found in an OpenAI chat/responses, Anthropic, or Mistral
    /// payload or stream event. Later events override earlier ones.
    pub fn observe(&mut self, v: &Value) {
        for u in [v.get("usage"), v.pointer("/response/usage"), v.pointer("/message/usage")].into_iter().flatten() {
            if !u.is_object() {
                continue;
            }
            let int = |p: &str| u.pointer(p).and_then(Value::as_i64);
            if let Some(n) = int("/prompt_tokens").or_else(|| int("/input_tokens")) {
                // Anthropic reports cache reads separately from input_tokens.
                let cache_read = int("/cache_read_input_tokens").unwrap_or(0);
                let cache_write = int("/cache_creation_input_tokens").unwrap_or(0);
                self.input = Some(n + cache_read + cache_write);
            }
            if let Some(n) = int("/completion_tokens").or_else(|| int("/output_tokens")) {
                self.output = Some(n);
            }
            if let Some(n) = int("/prompt_tokens_details/cached_tokens")
                .or_else(|| int("/input_tokens_details/cached_tokens"))
                .or_else(|| int("/cache_read_input_tokens"))
            {
                self.cached = Some(n);
            }
        }
    }
}

/// Finalises a request: writes the request log row, records metrics and
/// counts the tokens against the client's rate limits.
pub struct Accounting {
    pub db: Db,
    pub record: RequestRecord,
    pub started: Instant,
    limiter: Option<Arc<Limiter>>,
}

impl Accounting {
    pub fn new(db: Db, provider: &str, endpoint: &str, client_id: Option<&str>, model: Option<&str>) -> Self {
        Self {
            db,
            record: RequestRecord {
                client_id: client_id.map(str::to_string),
                provider: provider.to_string(),
                model: model.map(str::to_string),
                endpoint: endpoint.to_string(),
                trace_id: crate::telemetry::current_trace_id(),
                ..Default::default()
            },
            started: Instant::now(),
            limiter: None,
        }
    }

    pub fn with_limiter(mut self, limiter: &Arc<Limiter>) -> Self {
        self.limiter = Some(limiter.clone());
        self
    }

    pub fn finish(mut self, status: u16, usage: TokenUsage) {
        self.record.status = status;
        self.record.input_tokens = usage.input;
        self.record.output_tokens = usage.output;
        self.record.cached_tokens = usage.cached;
        self.record.latency_ms = self.started.elapsed().as_millis() as i64;
        let r = &self.record;
        if let (Some(limiter), Some(client)) = (&self.limiter, &r.client_id) {
            let tokens = usage.input.unwrap_or(0).max(0) + usage.output.unwrap_or(0).max(0);
            limiter.record_tokens(client, tokens as u64, crate::db::now());
        }
        let attrs = [
            KeyValue::new("provider", r.provider.clone()),
            KeyValue::new("model", r.model.clone().unwrap_or_default()),
            KeyValue::new("endpoint", r.endpoint.clone()),
            KeyValue::new("status", i64::from(status)),
            KeyValue::new("client", r.client_id.clone().unwrap_or_default()),
        ];
        let m = metrics();
        m.requests.add(1, &attrs);
        m.duration.record(self.started.elapsed().as_secs_f64(), &attrs[..3]);
        for (dir, n) in [("input", usage.input), ("output", usage.output), ("cached", usage.cached)] {
            if let Some(n) = n.filter(|n| *n > 0) {
                let mut a = attrs[..2].to_vec();
                a.push(KeyValue::new("direction", dir));
                a.push(attrs[4].clone());
                m.tokens.add(n as u64, &a);
            }
        }
        tracing::info!(
            provider = %r.provider,
            model = r.model.as_deref().unwrap_or(""),
            account = r.account_id.as_deref().unwrap_or(""),
            client = r.client_id.as_deref().unwrap_or(""),
            endpoint = %r.endpoint,
            status,
            input_tokens = usage.input,
            output_tokens = usage.output,
            latency_ms = r.latency_ms,
            "request complete"
        );
        let record = self.record;
        let db = self.db;
        tokio::spawn(async move {
            if let Err(e) = db.log_request(&record).await {
                tracing::warn!(error = %e, "failed to write request log");
            }
        });
    }
}

/// Ask a Chat Completions upstream to end its stream with a usage chunk, so
/// the request's tokens can be counted against rate limits. It is requested
/// even when the client didn't ask or said no; such a client gets one extra
/// chunk with empty `choices`. Returns whether the body changed.
pub fn request_stream_usage(path: &str, body: &mut Value) -> bool {
    let chat = path.split('?').next().unwrap_or(path).ends_with("/chat/completions");
    if !chat || body.get("stream").and_then(Value::as_bool) != Some(true) {
        return false;
    }
    let Some(obj) = body.as_object_mut() else { return false };
    let opts = obj.entry("stream_options").or_insert_with(|| Value::Object(Default::default()));
    if !opts.is_object() {
        *opts = Value::Object(Default::default());
    }
    if opts.get("include_usage") == Some(&Value::Bool(true)) {
        return false;
    }
    opts["include_usage"] = Value::Bool(true);
    true
}

/// Max bytes of a non-streamed body buffered for usage parsing.
const MAX_JSON_SCAN: usize = 4 * 1024 * 1024;

/// Wraps an upstream body stream, observes usage, and finalises accounting
/// when the stream ends or the client disconnects.
pub struct TapStream<S> {
    inner: S,
    sse: bool,
    line_buf: Vec<u8>,
    json_buf: Vec<u8>,
    usage: TokenUsage,
    status: u16,
    accounting: Option<Accounting>,
}

impl<S> TapStream<S> {
    pub fn new(inner: S, sse: bool, status: u16, accounting: Option<Accounting>) -> Self {
        Self { inner, sse, line_buf: Vec::new(), json_buf: Vec::new(), usage: TokenUsage::default(), status, accounting }
    }

    fn feed(&mut self, chunk: &[u8]) {
        if self.accounting.is_none() {
            return;
        }
        if !self.sse {
            if self.json_buf.len() + chunk.len() <= MAX_JSON_SCAN {
                self.json_buf.extend_from_slice(chunk);
            }
            return;
        }
        self.line_buf.extend_from_slice(chunk);
        while let Some(pos) = self.line_buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.line_buf.drain(..=pos).collect();
            self.observe_line(&line);
        }
        if self.line_buf.len() > MAX_JSON_SCAN {
            self.line_buf.clear();
        }
    }

    fn observe_line(&mut self, line: &[u8]) {
        let Some(data) = line.strip_prefix(b"data:") else { return };
        // Cheap pre-filter: only parse events that mention usage.
        if !data.windows(5).any(|w| w == b"usage") {
            return;
        }
        if let Ok(v) = serde_json::from_slice::<Value>(data.trim_ascii()) {
            self.usage.observe(&v);
        }
    }

    fn finish(&mut self) {
        if let Some(acc) = self.accounting.take() {
            if self.sse {
                let rest = std::mem::take(&mut self.line_buf);
                self.observe_line(&rest);
            } else if let Ok(v) = serde_json::from_slice::<Value>(&self.json_buf) {
                self.usage.observe(&v);
            }
            acc.finish(self.status, self.usage);
        }
    }
}

impl<S, E> Stream for TapStream<S>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
{
    type Item = Result<Bytes, E>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let polled = Pin::new(&mut self.inner).poll_next(cx);
        match &polled {
            Poll::Ready(Some(Ok(chunk))) => {
                let chunk = chunk.clone();
                self.feed(&chunk);
            }
            Poll::Ready(None) => self.finish(),
            _ => {}
        }
        polled
    }
}

impl<S> Drop for TapStream<S> {
    fn drop(&mut self) {
        // Client disconnected mid-stream: still account for what we saw.
        if let Some(acc) = self.accounting.take() {
            acc.finish(self.status, self.usage);
        }
    }
}

pub fn is_sse(headers: &reqwest::header::HeaderMap) -> bool {
    headers.get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|ct| ct.starts_with("text/event-stream"))
}

/// Relay an upstream response to the client, streaming the body.
pub fn relay(resp: reqwest::Response, accounting: Option<Accounting>) -> Response {
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let headers = forward_response_headers(resp.headers());
    let sse = is_sse(resp.headers());
    let stream = TapStream::new(Box::pin(resp.bytes_stream()), sse, status.as_u16(), accounting);
    let mut out = Response::new(Body::from_stream(stream));
    *out.status_mut() = status;
    *out.headers_mut() = headers;
    out
}

/// Relay an SSE stream whose first chunks were already read (peeked). The
/// caller knows the body is SSE even when upstream sent no `Content-Type`.
pub fn relay_sse<S>(
    status: u16,
    headers: &reqwest::header::HeaderMap,
    prefix: Vec<Bytes>,
    rest: S,
    accounting: Option<Accounting>,
) -> Response
where
    S: Stream<Item = reqwest::Result<Bytes>> + Unpin + Send + 'static,
{
    use futures::StreamExt;
    let combined = futures::stream::iter(prefix.into_iter().map(Ok)).chain(rest);
    let stream = TapStream::new(Box::pin(combined), true, status, accounting);
    let mut out = Response::new(Body::from_stream(stream));
    *out.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
    *out.headers_mut() = forward_response_headers(headers);
    out.headers_mut().entry(header::CONTENT_TYPE).or_insert(HeaderValue::from_static("text/event-stream"));
    out
}

/// Relay an already-buffered upstream response.
pub fn relay_buffered(status: u16, headers: &reqwest::header::HeaderMap, body: Bytes, accounting: Option<Accounting>) -> Response {
    if let Some(acc) = accounting {
        let mut usage = TokenUsage::default();
        if is_sse(headers) {
            for line in body.split(|b| *b == b'\n') {
                if let Some(d) = line.strip_prefix(b"data:")
                    && let Ok(v) = serde_json::from_slice::<Value>(d.trim_ascii())
                {
                    usage.observe(&v);
                }
            }
        } else if let Ok(v) = serde_json::from_slice::<Value>(&body) {
            usage.observe(&v);
        }
        acc.finish(status, usage);
    }
    let mut out = Response::new(Body::from(body));
    *out.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
    *out.headers_mut() = forward_response_headers(headers);
    out
}

/// JSON error in the OpenAI-style envelope most clients understand.
pub fn error_response(status: StatusCode, kind: &str, message: impl Into<String>) -> Response {
    let body = serde_json::json!({"error": {"type": kind, "message": message.into()}});
    (status, [(header::CONTENT_TYPE, "application/json")], body.to_string()).into_response()
}

/// JSON error in the Anthropic envelope (for `/v1/messages` clients).
pub fn anthropic_error(status: StatusCode, kind: &str, message: impl Into<String>) -> Response {
    let body = serde_json::json!({"type": "error", "error": {"type": kind, "message": message.into()}});
    (status, [(header::CONTENT_TYPE, "application/json")], body.to_string()).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn usage_openai_chat() {
        let mut u = TokenUsage::default();
        u.observe(&json!({"usage": {"prompt_tokens": 10, "completion_tokens": 3, "prompt_tokens_details": {"cached_tokens": 4}}}));
        assert_eq!(u, TokenUsage { input: Some(10), output: Some(3), cached: Some(4) });
    }

    #[test]
    fn usage_responses_completed_event() {
        let mut u = TokenUsage::default();
        u.observe(&json!({"type": "response.completed", "response": {"usage": {"input_tokens": 7, "output_tokens": 2, "input_tokens_details": {"cached_tokens": 1}}}}));
        assert_eq!(u, TokenUsage { input: Some(7), output: Some(2), cached: Some(1) });
    }

    #[test]
    fn usage_anthropic_stream() {
        let mut u = TokenUsage::default();
        u.observe(&json!({"type": "message_start", "message": {"usage": {"input_tokens": 5, "cache_read_input_tokens": 100, "output_tokens": 1}}}));
        u.observe(&json!({"type": "message_delta", "usage": {"output_tokens": 42}}));
        assert_eq!(u, TokenUsage { input: Some(105), output: Some(42), cached: Some(100) });
    }

    #[tokio::test]
    async fn tap_stream_parses_split_sse() {
        use futures::StreamExt;
        let db = Db::in_memory().await.unwrap();
        let chunks: Vec<Result<Bytes, std::io::Error>> = vec![
            Ok(Bytes::from_static(b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"us")),
            Ok(Bytes::from_static(b"age\":{\"input_tokens\":3,\"output_tokens\":4}}}\n\n")),
        ];
        let acc = Accounting::new(db.clone(), "codex", "/v1/responses", Some("c"), Some("m"));
        let mut tap = TapStream::new(futures::stream::iter(chunks), true, 200, Some(acc));
        while tap.next().await.is_some() {}
        assert_eq!(tap.usage, TokenUsage { input: Some(3), output: Some(4), cached: None });
        drop(tap);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let s = db.usage_summary("client", 0, &Default::default()).await.unwrap();
        assert_eq!(s[0].output_tokens, 4);
    }

    #[test]
    fn stream_usage_is_requested_on_chat_streams_only() {
        for opts in [None, Some(json!({"include_usage": false})), Some(json!(null))] {
            let mut body = json!({"model": "m", "stream": true});
            if let Some(o) = opts {
                body["stream_options"] = o;
            }
            assert!(request_stream_usage("/v1/chat/completions", &mut body));
            assert_eq!(body["stream_options"]["include_usage"], true);
        }
        let mut asked = json!({"stream": true, "stream_options": {"include_usage": true}});
        assert!(!request_stream_usage("/v1/chat/completions", &mut asked));
        let mut not_streaming = json!({"stream": false});
        assert!(!request_stream_usage("/v1/chat/completions", &mut not_streaming));
        assert!(not_streaming.get("stream_options").is_none());
        let mut responses = json!({"stream": true});
        assert!(!request_stream_usage("/v1/responses", &mut responses));
    }

    #[test]
    fn strips_client_credentials() {
        let mut h = HeaderMap::new();
        h.insert("authorization", "Bearer x".parse().unwrap());
        h.insert("x-api-key", "x".parse().unwrap());
        h.insert("anthropic-beta", "foo".parse().unwrap());
        let out = forward_request_headers(&h);
        assert!(out.get("authorization").is_none());
        assert!(out.get("x-api-key").is_none());
        assert_eq!(out.get("anthropic-beta").unwrap(), "foo");
    }
}
