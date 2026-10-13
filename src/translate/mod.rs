//! Translation between the LLM wire formats: Anthropic Messages, OpenAI
//! Responses and OpenAI Chat Completions.
//!
//! A request in the client's format is decoded into a format-neutral
//! [`ir::Request`] and encoded in the upstream's format. The upstream response
//! (JSON or SSE) is decoded into [`ir::Event`]s and encoded in the client's
//! format, streamed or as one JSON body as the client asked. Usage is still
//! recorded from the raw upstream response, which the pool or gateway taps
//! before it reaches the translator.

mod chat;
pub mod ir;
mod messages;
mod responses;
pub mod sse;

use std::collections::{HashMap, HashSet};
use std::pin::Pin;
use std::task::{Context, Poll};

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::Response;
use futures::{Stream, StreamExt};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::catalog::Wire;
use ir::{Collector, Event, Start};
use sse::{SseEvent, SseParser};

/// What the response side needs to know about the request.
#[derive(Debug, Clone, Default)]
pub struct ClientInfo {
    /// The model name the client asked for, reported back in responses.
    pub model: String,
    /// Responses `custom` (free-form input) tools; their calls go back as
    /// `custom_tool_call` items.
    pub custom_tools: HashSet<String>,
    /// Responses namespace tools, flattened: qualified name -> (namespace, name).
    pub namespaced: HashMap<String, (String, String)>,
    /// Chat clients get a usage chunk only when they ask for one.
    pub include_usage: bool,
}

/// One translated exchange: built from the request, applied to the response.
#[derive(Debug, Clone)]
pub struct Translation {
    pub client: Wire,
    pub upstream: Wire,
    stream: bool,
    info: ClientInfo,
    /// Tool names shortened for the upstream -> the client's names.
    renamed: HashMap<String, String>,
}

/// Translate a request body from `client` to `upstream` format for
/// `upstream_model`. The error is a message for a 400 response.
pub fn request(client: Wire, upstream: Wire, body: &Value, upstream_model: &str) -> Result<(Value, Translation), String> {
    let mut info = ClientInfo { model: body.get("model").and_then(Value::as_str).unwrap_or_default().to_string(), ..Default::default() };
    let mut req = match client {
        Wire::Messages => messages::decode_request(body)?,
        Wire::Responses => responses::decode_request(body, &mut info)?,
        Wire::Chat => chat::decode_request(body, &mut info)?,
    };
    let renamed = if upstream == Wire::Messages { HashMap::new() } else { shorten_tool_names(&mut req) };
    let out = match upstream {
        Wire::Messages => messages::encode_request(&req, upstream_model)?,
        Wire::Responses => responses::encode_request(&req, upstream_model),
        Wire::Chat => chat::encode_request(&req, upstream_model),
    };
    Ok((out, Translation { client, upstream, stream: req.stream, info, renamed }))
}

/// OpenAI tool names are limited to 64 characters from `[A-Za-z0-9_-]`.
const MAX_TOOL_NAME: usize = 64;

fn short_name(name: &str) -> String {
    let valid = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    if name.len() <= MAX_TOOL_NAME && !name.is_empty() && name.chars().all(valid) {
        return name.to_string();
    }
    let hash = hex::encode(&Sha256::digest(name.as_bytes())[..4]);
    let prefix: String = name.chars().map(|c| if valid(c) { c } else { '_' }).take(MAX_TOOL_NAME - hash.len() - 1).collect();
    format!("{prefix}_{hash}")
}

/// Shorten tool names the upstream would reject; returns short -> original.
fn shorten_tool_names(req: &mut ir::Request) -> HashMap<String, String> {
    let mut renamed = HashMap::new();
    let mut rename = |name: &mut String| {
        let short = short_name(name);
        if short != *name {
            renamed.insert(short.clone(), std::mem::replace(name, short));
        }
    };
    for t in &mut req.tools {
        rename(&mut t.name);
    }
    if let Some(ir::ToolChoice::Tool(n)) = &mut req.tool_choice {
        rename(n);
    }
    for m in &mut req.messages {
        for p in &mut m.parts {
            if let ir::Part::ToolCall(c) = p {
                rename(&mut c.name);
            }
        }
    }
    renamed
}

trait Decoder: Send {
    fn feed(&mut self, ev: &SseEvent) -> Vec<Event>;
    /// The upstream stream ended.
    fn end(&mut self) -> Vec<Event>;
}

trait Encoder: Send {
    fn encode(&mut self, e: &Event) -> String;
}

impl Translation {
    fn decoder(&self) -> Box<dyn Decoder> {
        match self.upstream {
            Wire::Messages => Box::<messages::StreamDecoder>::default(),
            Wire::Responses => Box::<responses::StreamDecoder>::default(),
            Wire::Chat => Box::<chat::StreamDecoder>::default(),
        }
    }

    fn encoder(&self) -> Box<dyn Encoder> {
        match self.client {
            Wire::Messages => Box::new(messages::StreamEncoder::new(self.info.clone())),
            Wire::Responses => Box::new(responses::StreamEncoder::new(self.info.clone())),
            Wire::Chat => Box::new(chat::StreamEncoder::new(self.info.clone())),
        }
    }

    /// A JSON response, or the failure it reports despite a 2xx status.
    fn decode_json(&self, v: &Value) -> Result<ir::Response, (String, String)> {
        let failure = match self.upstream {
            Wire::Messages => messages::failure(v),
            Wire::Responses => responses::failure(v),
            Wire::Chat => chat::failure(v),
        };
        if let Some(f) = failure {
            return Err(f);
        }
        Ok(match self.upstream {
            Wire::Messages => messages::decode_response(v),
            Wire::Responses => responses::decode_response(v),
            Wire::Chat => chat::decode_response(v),
        })
    }

    fn encode_json(&self, r: &ir::Response) -> Value {
        match self.client {
            Wire::Messages => messages::encode_response(r, &self.info),
            Wire::Responses => responses::encode_response(r, &self.info),
            Wire::Chat => chat::encode_response(r, &self.info),
        }
    }

    /// Give shortened tool names back their original spelling.
    fn restore(&self, events: &mut [Event]) {
        if self.renamed.is_empty() {
            return;
        }
        for e in events {
            if let Event::Start(Start::ToolCall { name, .. }) = e
                && let Some(original) = self.renamed.get(name)
            {
                *name = original.clone();
            }
        }
    }

    /// An error in the client's envelope.
    pub fn error(&self, status: StatusCode, kind: &str, message: &str) -> Response {
        let body = error_body(self.client, kind, message);
        let mut out = Response::new(Body::from(body.to_string()));
        *out.status_mut() = status;
        out.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
        out
    }

    /// Translate the upstream response for the client.
    pub async fn response(self, resp: Response) -> Response {
        let (parts, body) = resp.into_parts();
        let sse =
            parts.headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|ct| ct.starts_with("text/event-stream"));
        let mut headers = parts.headers;
        headers.remove(header::CONTENT_TYPE);
        headers.remove(header::CONTENT_LENGTH);

        if !parts.status.is_success() {
            let bytes = axum::body::to_bytes(body, MAX_ERROR_BODY).await.unwrap_or_default();
            let (kind, message) = upstream_error(parts.status, &bytes);
            return with_headers(self.error(parts.status, &kind, &message), headers);
        }

        if self.stream {
            let encoder = self.encoder();
            let out = if sse {
                Body::from_stream(TranslateStream {
                    inner: body.into_data_stream(),
                    parser: SseParser::default(),
                    decoder: self.decoder(),
                    encoder,
                    t: self,
                    done: false,
                })
            } else {
                let mut events = match self.read_json(body).await {
                    Ok(r) => r.events(),
                    Err((kind, message)) => vec![Event::Error { kind, message }],
                };
                self.restore(&mut events);
                let mut encoder = encoder;
                Body::from(events.iter().map(|e| encoder.encode(e)).collect::<String>())
            };
            let mut resp = Response::new(out);
            headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
            headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
            *resp.headers_mut() = headers;
            return resp;
        }

        let collected = if sse {
            self.collect_sse(body).await
        } else {
            self.read_json(body).await.and_then(|r| {
                let mut events = r.events();
                self.restore(&mut events);
                let mut c = Collector::default();
                events.into_iter().for_each(|e| c.push(e));
                c.finish()
            })
        };
        match collected {
            Ok(r) => {
                let mut resp = Response::new(Body::from(self.encode_json(&r).to_string()));
                headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
                *resp.headers_mut() = headers;
                resp
            }
            Err((kind, message)) => with_headers(self.error(StatusCode::BAD_GATEWAY, &kind, &message), headers),
        }
    }

    async fn read_json(&self, body: Body) -> Result<ir::Response, (String, String)> {
        let fail = |m: String| ("api_error".to_string(), m);
        let bytes = axum::body::to_bytes(body, usize::MAX).await.map_err(|e| fail(format!("reading upstream response: {e}")))?;
        let v: Value = serde_json::from_slice(&bytes).map_err(|e| fail(format!("upstream sent invalid JSON: {e}")))?;
        self.decode_json(&v)
    }

    /// Read a whole upstream stream into one response.
    async fn collect_sse(&self, body: Body) -> Result<ir::Response, (String, String)> {
        let mut parser = SseParser::default();
        let mut decoder = self.decoder();
        let mut c = Collector::default();
        let mut stream = body.into_data_stream();
        let push = |events: Vec<Event>, c: &mut Collector| {
            let mut events = events;
            self.restore(&mut events);
            events.into_iter().for_each(|e| c.push(e));
        };
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| ("api_error".to_string(), format!("reading upstream stream: {e}")))?;
            for ev in parser.feed(&chunk) {
                push(decoder.feed(&ev), &mut c);
            }
        }
        for ev in parser.end() {
            push(decoder.feed(&ev), &mut c);
        }
        push(decoder.end(), &mut c);
        c.finish()
    }
}

/// Upstream error bodies are small; anything longer is cut.
const MAX_ERROR_BODY: usize = 1024 * 1024;

fn with_headers(mut resp: Response, headers: HeaderMap) -> Response {
    let ct = resp.headers_mut().remove(header::CONTENT_TYPE);
    *resp.headers_mut() = headers;
    if let Some(ct) = ct {
        resp.headers_mut().insert(header::CONTENT_TYPE, ct);
    }
    resp
}

/// The error type and message of an upstream error body in any format.
fn upstream_error(status: StatusCode, body: &[u8]) -> (String, String) {
    let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let err = v.get("error").unwrap_or(&v);
    let message = err
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| err.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| String::from_utf8_lossy(body).trim().to_string());
    let message = if message.is_empty() { status.canonical_reason().unwrap_or("upstream error").to_string() } else { message };
    let kind = err
        .get("type")
        .or_else(|| err.get("code"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| default_kind(status).to_string());
    (kind, message)
}

fn default_kind(s: StatusCode) -> &'static str {
    match s.as_u16() {
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        429 => "rate_limit_error",
        400..=499 => "invalid_request_error",
        _ => "api_error",
    }
}

fn error_body(wire: Wire, kind: &str, message: &str) -> Value {
    match wire {
        Wire::Messages => messages::error_body(kind, message),
        Wire::Responses | Wire::Chat => serde_json::json!({"error": {"type": kind, "message": message}}),
    }
}

/// Decodes an upstream SSE body and re-encodes it for the client as it
/// arrives. Dropping it drops the upstream body, which finalises accounting.
struct TranslateStream<S> {
    inner: S,
    parser: SseParser,
    decoder: Box<dyn Decoder>,
    encoder: Box<dyn Encoder>,
    t: Translation,
    done: bool,
}

impl<S> TranslateStream<S> {
    fn encode(&mut self, mut events: Vec<Event>) -> String {
        self.t.restore(&mut events);
        events.iter().map(|e| self.encoder.encode(e)).collect()
    }
}

impl<S, E> Stream for TranslateStream<S>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            if self.done {
                return Poll::Ready(None);
            }
            let out = match Pin::new(&mut self.inner).poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Some(Ok(chunk))) => {
                    let mut events = Vec::new();
                    for ev in self.parser.feed(&chunk) {
                        events.extend(self.decoder.feed(&ev));
                    }
                    self.encode(events)
                }
                Poll::Ready(Some(Err(e))) => {
                    self.done = true;
                    let err = Event::Error { kind: "api_error".into(), message: format!("reading upstream stream: {e}") };
                    self.encode(vec![err])
                }
                Poll::Ready(None) => {
                    self.done = true;
                    let mut events = Vec::new();
                    for ev in self.parser.end() {
                        events.extend(self.decoder.feed(&ev));
                    }
                    events.extend(self.decoder.end());
                    self.encode(events)
                }
            };
            if !out.is_empty() {
                return Poll::Ready(Some(Ok(Bytes::from(out))));
            }
        }
    }
}

/// A fresh response identifier with the given prefix.
fn new_id(prefix: &str) -> String {
    format!("{prefix}{}", uuid::Uuid::new_v4().simple())
}

#[cfg(test)]
mod tests;
