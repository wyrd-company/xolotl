//! Anthropic Messages (`/v1/messages`) responses, JSON and streamed.

use super::*;
use crate::translate::ir::{Block, Emitter, Event, Response, Start, StopReason, Usage};
use crate::translate::sse::{SseEvent, frame};
use crate::translate::{ClientInfo, Decoder, Encoder, new_id};

fn stop_reason(s: &str) -> StopReason {
    match s {
        "max_tokens" | "model_context_window_exceeded" => StopReason::MaxTokens,
        "tool_use" => StopReason::ToolUse,
        "stop_sequence" => StopReason::StopSequence,
        "refusal" => StopReason::ContentFilter,
        _ => StopReason::End,
    }
}

fn stop_str(s: StopReason) -> &'static str {
    match s {
        StopReason::End => "end_turn",
        StopReason::MaxTokens => "max_tokens",
        StopReason::ToolUse => "tool_use",
        StopReason::StopSequence => "stop_sequence",
        StopReason::ContentFilter => "refusal",
    }
}

/// Merge an Anthropic `usage` object; its `input_tokens` excludes cache reads and writes.
fn read_usage(u: &mut Usage, v: &Value) {
    let n = |k: &str| v.get(k).and_then(Value::as_u64);
    if let Some(cr) = n("cache_read_input_tokens") {
        u.cache_read = cr;
    }
    if let Some(cw) = n("cache_creation_input_tokens") {
        u.cache_write = cw;
    }
    if let Some(i) = n("input_tokens") {
        u.input = i + u.cache_read + u.cache_write;
    }
    if let Some(o) = n("output_tokens") {
        u.output = o;
    }
}

fn usage_json(u: &Usage) -> Value {
    json!({
        "input_tokens": u.input.saturating_sub(u.cache_read + u.cache_write),
        "output_tokens": u.output,
        "cache_read_input_tokens": u.cache_read,
        "cache_creation_input_tokens": u.cache_write,
    })
}

/// An error body sent with a successful HTTP status.
pub fn failure(v: &Value) -> Option<(String, String)> {
    (v.get("type").and_then(Value::as_str) == Some("error")).then(|| {
        let s = |p: &str| v.pointer(p).and_then(Value::as_str).unwrap_or_default().to_string();
        (Some(s("/error/type")).filter(|k| !k.is_empty()).unwrap_or_else(|| "api_error".into()), s("/error/message"))
    })
}

pub fn decode_response(v: &Value) -> Response {
    let mut r = Response::default();
    for b in v.get("content").and_then(Value::as_array).into_iter().flatten() {
        match decode_block(b) {
            Some(Part::Text(t)) => r.blocks.push(Block::Text(t)),
            Some(Part::Reasoning(x)) => r.blocks.push(Block::Reasoning(x)),
            Some(Part::ToolCall(c)) => r.blocks.push(Block::ToolCall(c)),
            _ => {}
        }
    }
    r.stop = stop_reason(v.get("stop_reason").and_then(Value::as_str).unwrap_or_default());
    if let Some(u) = v.get("usage") {
        read_usage(&mut r.usage, u);
    }
    r
}

pub fn encode_response(r: &Response, info: &ClientInfo) -> Value {
    let content: Vec<Value> = r
        .blocks
        .iter()
        .map(|b| match b {
            Block::Text(t) => json!({"type": "text", "text": t}),
            Block::Reasoning(x) => json!({
                "type": "thinking",
                "thinking": x.text,
                "signature": x.opaque.as_ref().map(Opaque::to_signature).unwrap_or_default(),
            }),
            Block::ToolCall(c) => {
                json!({"type": "tool_use", "id": tool_id(&c.id), "name": c.name, "input": arguments_object(&c.arguments)})
            }
        })
        .collect();
    json!({
        "id": new_id("msg_"),
        "type": "message",
        "role": "assistant",
        "model": info.model,
        "content": content,
        "stop_reason": stop_str(r.stop),
        "stop_sequence": null,
        "usage": usage_json(&r.usage),
    })
}

pub fn error_body(kind: &str, message: &str) -> Value {
    let (kind, message) =
        if crate::metadata::too_long(kind, message) { ("invalid_request_error", "prompt is too long") } else { (kind, message) };
    json!({"type": "error", "error": {"type": kind, "message": message}})
}

/// Anthropic stream events -> [`Event`]s.
#[derive(Default)]
pub struct StreamDecoder {
    em: Emitter,
    /// Whether the content block at each index is translated (server tool
    /// blocks are skipped).
    live: Vec<bool>,
    usage: Usage,
    stop: StopReason,
}

impl StreamDecoder {
    fn live(&self, v: &Value) -> bool {
        v.get("index").and_then(Value::as_u64).and_then(|i| self.live.get(i as usize).copied()).unwrap_or(false)
    }
}

impl Decoder for StreamDecoder {
    fn feed(&mut self, ev: &SseEvent) -> Vec<Event> {
        let Some(v) = ev.json() else { return Vec::new() };
        let s = |p: &str| v.pointer(p).and_then(Value::as_str).unwrap_or_default();
        match s("/type") {
            "message_start" => {
                if let Some(u) = v.pointer("/message/usage") {
                    read_usage(&mut self.usage, u);
                }
            }
            "content_block_start" => {
                let index = v.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                if self.live.len() <= index {
                    self.live.resize(index + 1, false);
                }
                let b = v.get("content_block").cloned().unwrap_or(Value::Null);
                let bs = |k: &str| b.get(k).and_then(Value::as_str).unwrap_or_default();
                let live = match bs("type") {
                    "text" => {
                        self.em.start(Start::Text);
                        self.em.delta(bs("text"));
                        true
                    }
                    "thinking" => {
                        self.em.start(Start::Reasoning);
                        self.em.delta(bs("thinking"));
                        true
                    }
                    "redacted_thinking" => {
                        self.em.start(Start::Reasoning);
                        self.em.opaque(Opaque::AnthropicRedacted(bs("data").to_string()));
                        true
                    }
                    "tool_use" => {
                        self.em.start(Start::ToolCall { id: bs("id").to_string(), name: bs("name").to_string() });
                        true
                    }
                    _ => false,
                };
                self.live[index] = live;
            }
            "content_block_delta" if self.live(&v) => match s("/delta/type") {
                "text_delta" => self.em.delta(s("/delta/text")),
                "thinking_delta" => self.em.delta(s("/delta/thinking")),
                "input_json_delta" => self.em.delta(s("/delta/partial_json")),
                "signature_delta" => self.em.opaque(Opaque::AnthropicSignature(s("/delta/signature").to_string())),
                _ => {}
            },
            "content_block_stop" if self.live(&v) => self.em.stop(),
            "message_delta" => {
                if let Some(r) = v.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.stop = stop_reason(r);
                }
                if let Some(u) = v.get("usage") {
                    read_usage(&mut self.usage, u);
                }
            }
            "message_stop" => self.em.finish(self.stop, self.usage),
            "error" => self.em.error(s("/error/type"), s("/error/message")),
            _ => {}
        }
        self.em.take()
    }

    fn end(&mut self) -> Vec<Event> {
        self.em.end();
        self.em.take()
    }
}

/// [`Event`]s -> Anthropic stream events.
pub struct StreamEncoder {
    info: ClientInfo,
    id: String,
    started: bool,
    index: u64,
    open: Option<Start>,
}

impl StreamEncoder {
    pub fn new(info: ClientInfo) -> Self {
        Self { info, id: new_id("msg_"), started: false, index: 0, open: None }
    }
}

impl Encoder for StreamEncoder {
    fn encode(&mut self, e: &Event) -> String {
        let mut out = String::new();
        if !self.started {
            self.started = true;
            let message = json!({
                "id": self.id, "type": "message", "role": "assistant", "model": self.info.model, "content": [],
                "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 0, "output_tokens": 0},
            });
            out += &frame(Some("message_start"), &json!({"type": "message_start", "message": message}));
        }
        let i = self.index;
        match e {
            Event::Start(s) => {
                let block = match s {
                    Start::Text => json!({"type": "text", "text": ""}),
                    Start::Reasoning => json!({"type": "thinking", "thinking": "", "signature": ""}),
                    Start::ToolCall { id, name } => json!({"type": "tool_use", "id": tool_id(id), "name": name, "input": {}}),
                };
                self.open = Some(s.clone());
                out += &frame(Some("content_block_start"), &json!({"type": "content_block_start", "index": i, "content_block": block}));
            }
            Event::Delta(d) => {
                let delta = match self.open {
                    Some(Start::Text) => json!({"type": "text_delta", "text": d}),
                    Some(Start::Reasoning) => json!({"type": "thinking_delta", "thinking": d}),
                    Some(Start::ToolCall { .. }) => json!({"type": "input_json_delta", "partial_json": d}),
                    None => return out,
                };
                out += &frame(Some("content_block_delta"), &json!({"type": "content_block_delta", "index": i, "delta": delta}));
            }
            Event::Opaque(o) => {
                let delta = json!({"type": "signature_delta", "signature": o.to_signature()});
                out += &frame(Some("content_block_delta"), &json!({"type": "content_block_delta", "index": i, "delta": delta}));
            }
            Event::Stop => {
                self.open = None;
                self.index += 1;
                out += &frame(Some("content_block_stop"), &json!({"type": "content_block_stop", "index": i}));
            }
            Event::Finish { stop, usage } => {
                let delta = json!({"type": "message_delta", "delta": {"stop_reason": stop_str(*stop), "stop_sequence": null}, "usage": usage_json(usage)});
                out += &frame(Some("message_delta"), &delta);
                out += &frame(Some("message_stop"), &json!({"type": "message_stop"}));
            }
            Event::Error { kind, message } => out += &frame(Some("error"), &error_body(kind, message)),
        }
        out
    }
}
