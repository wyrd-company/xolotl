//! Anthropic Messages (`/v1/messages`).

use serde_json::{Map, Value, json};

use super::ir::{
    Block, Emitter, Event, Image, Opaque, Part, Reasoning, Request, Response, Role, Start, StopReason, ToolCall, ToolChoice, ToolResult,
    Usage, arguments_object, budget_for_effort, effort_for_budget,
};
use super::sse::{SseEvent, frame};
use super::{ClientInfo, Decoder, Encoder, UpstreamInfo, new_id};

/// `max_tokens` is required by Messages; used when the client set none.
pub const DEFAULT_MAX_TOKENS: u64 = 32_000;

// --------------------------------------------------------------- requests

pub fn decode_request(v: &Value) -> Result<Request, String> {
    let mut r = Request {
        system: match v.get("system") {
            Some(Value::String(s)) if !s.is_empty() => vec![s.clone()],
            // Claude Code's billing attribution block means nothing to other providers.
            Some(Value::Array(blocks)) => blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .filter(|t| !t.starts_with("x-anthropic-billing-header:"))
                .map(str::to_string)
                .collect(),
            _ => Vec::new(),
        },
        max_tokens: v.get("max_tokens").and_then(Value::as_u64),
        temperature: v.get("temperature").and_then(Value::as_f64),
        top_p: v.get("top_p").and_then(Value::as_f64),
        stop: v
            .get("stop_sequences")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        stream: v.get("stream").and_then(Value::as_bool).unwrap_or(false),
        ..Default::default()
    };
    for m in v.get("messages").and_then(Value::as_array).into_iter().flatten() {
        let role = if m.get("role").and_then(Value::as_str) == Some("assistant") { Role::Assistant } else { Role::User };
        let parts = match m.get("content") {
            Some(Value::String(s)) => vec![Part::Text(s.clone())],
            Some(Value::Array(blocks)) => blocks.iter().filter_map(decode_block).collect(),
            _ => Vec::new(),
        };
        r.push(role, parts);
    }
    // Client tools only; server tools (web search, code execution, ...) have no counterpart.
    for t in v.get("tools").and_then(Value::as_array).into_iter().flatten() {
        let kind = t.get("type").and_then(Value::as_str).unwrap_or("custom");
        if let (true, Some(name), Some(schema)) = (kind == "custom", t.get("name").and_then(Value::as_str), t.get("input_schema")) {
            r.tools.push(super::ir::Tool {
                name: name.to_string(),
                description: t.get("description").and_then(Value::as_str).map(str::to_string),
                parameters: schema.clone(),
            });
        }
    }
    if let Some(tc) = v.get("tool_choice") {
        r.tool_choice = match tc.get("type").and_then(Value::as_str) {
            Some("auto") => Some(ToolChoice::Auto),
            Some("any") => Some(ToolChoice::Required),
            Some("none") => Some(ToolChoice::None),
            Some("tool") => tc.get("name").and_then(Value::as_str).map(|n| ToolChoice::Tool(n.to_string())),
            _ => None,
        };
        r.parallel_tool_calls = tc.get("disable_parallel_tool_use").and_then(Value::as_bool).map(|d| !d);
    }
    let thinking = v.get("thinking");
    r.effort =
        match (v.pointer("/output_config/effort").and_then(Value::as_str), thinking.and_then(|t| t.get("type")).and_then(Value::as_str)) {
            (_, Some("disabled")) => None,
            (Some("max"), _) => Some("xhigh".into()),
            (Some(e), _) => Some(e.to_string()),
            (None, Some("enabled")) => {
                thinking.and_then(|t| t.get("budget_tokens")).and_then(Value::as_u64).map(|b| effort_for_budget(b).to_string())
            }
            (None, Some("adaptive")) => Some("medium".into()),
            _ => None,
        };
    Ok(r)
}

fn decode_image(source: &Value) -> Option<Image> {
    match source.get("type").and_then(Value::as_str) {
        Some("base64") => Some(Image::Base64 {
            media_type: source.get("media_type").and_then(Value::as_str).unwrap_or("image/png").to_string(),
            data: source.get("data").and_then(Value::as_str)?.to_string(),
        }),
        Some("url") => Some(Image::Url(source.get("url").and_then(Value::as_str)?.to_string())),
        _ => None,
    }
}

fn decode_block(b: &Value) -> Option<Part> {
    let s = |k: &str| b.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    match b.get("type").and_then(Value::as_str)? {
        "text" => Some(Part::Text(s("text"))),
        "image" => decode_image(b.get("source")?).map(Part::Image),
        "thinking" => Some(Part::Reasoning(Reasoning { text: s("thinking"), opaque: Opaque::from_signature(&s("signature")) })),
        "redacted_thinking" => Some(Part::Reasoning(Reasoning { text: String::new(), opaque: Some(Opaque::AnthropicRedacted(s("data"))) })),
        "tool_use" => Some(Part::ToolCall(ToolCall {
            id: s("id"),
            name: s("name"),
            arguments: b.get("input").map(Value::to_string).unwrap_or_else(|| "{}".into()),
        })),
        "tool_result" => {
            let content = match b.get("content") {
                Some(Value::String(t)) => vec![Part::Text(t.clone())],
                Some(Value::Array(items)) => items
                    .iter()
                    .filter_map(|i| match i.get("type").and_then(Value::as_str) {
                        Some("text") => i.get("text").and_then(Value::as_str).map(|t| Part::Text(t.to_string())),
                        Some("image") => i.get("source").and_then(decode_image).map(Part::Image),
                        _ => None,
                    })
                    .collect(),
                _ => Vec::new(),
            };
            Some(Part::ToolResult(ToolResult {
                call_id: s("tool_use_id"),
                content,
                is_error: b.get("is_error").and_then(Value::as_bool).unwrap_or(false),
            }))
        }
        _ => None,
    }
}

/// Tool use ids must match `^[a-zA-Z0-9_-]+$`.
fn tool_id(id: &str) -> String {
    let id: String = id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect();
    if id.is_empty() { "toolu_xolotl".into() } else { id }
}

fn encode_image(i: &Image) -> Value {
    match i {
        Image::Base64 { media_type, data } => {
            json!({"type": "image", "source": {"type": "base64", "media_type": media_type, "data": data}})
        }
        Image::Url(u) => json!({"type": "image", "source": {"type": "url", "url": u}}),
    }
}

fn encode_part(p: &Part) -> Option<Value> {
    match p {
        Part::Text(t) if t.is_empty() => None,
        Part::Text(t) => Some(json!({"type": "text", "text": t})),
        Part::Image(i) => Some(encode_image(i)),
        // Only Anthropic's own reasoning can be replayed to it.
        Part::Reasoning(r) => match &r.opaque {
            Some(Opaque::AnthropicSignature(sig)) => Some(json!({"type": "thinking", "thinking": r.text, "signature": sig})),
            Some(Opaque::AnthropicRedacted(data)) => Some(json!({"type": "redacted_thinking", "data": data})),
            _ => None,
        },
        Part::ToolCall(c) => {
            Some(json!({"type": "tool_use", "id": tool_id(&c.id), "name": c.name, "input": arguments_object(&c.arguments)}))
        }
        Part::ToolResult(res) => {
            let mut content: Vec<Value> = res
                .content
                .iter()
                .filter_map(|p| if matches!(p, Part::Text(_) | Part::Image(_)) { encode_part(p) } else { None })
                .collect();
            if content.is_empty() {
                content.push(json!({"type": "text", "text": ""}));
            }
            let mut b = json!({"type": "tool_result", "tool_use_id": tool_id(&res.call_id), "content": content});
            if res.is_error {
                b["is_error"] = Value::Bool(true);
            }
            Some(b)
        }
    }
}

/// With thinking on, an assistant turn that called tools must open with its
/// thinking block; turns translated from elsewhere have none to send.
fn tool_turn_lacks_thinking(r: &Request) -> bool {
    r.messages.iter().rev().find(|m| m.role == Role::Assistant).is_some_and(|m| {
        m.parts.iter().any(|p| matches!(p, Part::ToolCall(_)))
            && !m.parts.iter().any(|p| matches!(p, Part::Reasoning(Reasoning { opaque: Some(Opaque::AnthropicSignature(_)), .. })))
    })
}

pub fn encode_request(r: &Request, model: &str, up: UpstreamInfo) -> Result<Value, String> {
    if r.response_format.is_some() {
        return Err("structured output (response_format / text.format) can't be translated to /v1/messages".into());
    }
    let mut messages: Vec<Value> = Vec::new();
    let mut last_role: Option<Role> = None;
    for m in &r.messages {
        // Tool results must come first in a user turn.
        let mut parts: Vec<&Part> = m.parts.iter().filter(|p| matches!(p, Part::ToolResult(_))).collect();
        parts.extend(m.parts.iter().filter(|p| !matches!(p, Part::ToolResult(_))));
        let blocks: Vec<Value> = parts.into_iter().filter_map(encode_part).collect();
        if blocks.is_empty() {
            continue;
        }
        if last_role == Some(m.role)
            && let Some(Value::Array(prev)) = messages.last_mut().and_then(|p| p.get_mut("content"))
        {
            prev.extend(blocks);
            continue;
        }
        let role = if m.role == Role::Assistant { "assistant" } else { "user" };
        messages.push(json!({"role": role, "content": blocks}));
        last_role = Some(m.role);
    }

    let mut max_tokens = r.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);
    let mut out = Map::new();
    out.insert("model".into(), json!(model));
    out.insert("messages".into(), Value::Array(messages));
    if !r.system.is_empty() {
        out.insert("system".into(), json!(r.system.join("\n\n")));
    }
    if !r.tools.is_empty() {
        let tools: Vec<Value> = r
            .tools
            .iter()
            .map(|t| {
                let mut v = json!({"name": t.name, "input_schema": t.parameters});
                if let Some(d) = &t.description {
                    v["description"] = json!(d);
                }
                v
            })
            .collect();
        out.insert("tools".into(), Value::Array(tools));
    }
    let mut choice = match &r.tool_choice {
        Some(ToolChoice::Auto) => Some(json!({"type": "auto"})),
        Some(ToolChoice::None) => Some(json!({"type": "none"})),
        Some(ToolChoice::Required) => Some(json!({"type": "any"})),
        Some(ToolChoice::Tool(n)) => Some(json!({"type": "tool", "name": n})),
        None => None,
    };
    if r.parallel_tool_calls == Some(false) && !r.tools.is_empty() {
        choice.get_or_insert_with(|| json!({"type": "auto"}))["disable_parallel_tool_use"] = Value::Bool(true);
    }
    let forced = matches!(r.tool_choice, Some(ToolChoice::Required | ToolChoice::Tool(_)));
    if let Some(c) = choice {
        out.insert("tool_choice".into(), c);
    }
    if !r.stop.is_empty() {
        out.insert("stop_sequences".into(), json!(r.stop));
    }
    let effort = r.effort.as_deref().filter(|e| *e != "none" && !forced && !tool_turn_lacks_thinking(r));
    let budget = effort.and_then(budget_for_effort);
    match (effort, budget) {
        // Thinking rejects sampling parameters.
        (Some(e), _) if up.adaptive_thinking => {
            let effort = match e {
                "minimal" => "low",
                "xhigh" | "max" => "high",
                other => other,
            };
            out.insert("thinking".into(), json!({"type": "adaptive"}));
            out.insert("output_config".into(), json!({"effort": effort}));
        }
        (Some(_), Some(budget)) => {
            max_tokens = max_tokens.max(budget + 4096);
            out.insert("thinking".into(), json!({"type": "enabled", "budget_tokens": budget}));
        }
        // Recent Claude models reject `temperature` and `top_p` together.
        _ => match (r.temperature, r.top_p) {
            (Some(t), _) => {
                out.insert("temperature".into(), json!(t));
            }
            (None, Some(p)) => {
                out.insert("top_p".into(), json!(p));
            }
            (None, None) => {}
        },
    }
    out.insert("max_tokens".into(), json!(max_tokens));
    if r.stream {
        out.insert("stream".into(), Value::Bool(true));
    }
    Ok(Value::Object(out))
}

// -------------------------------------------------------------- responses

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
