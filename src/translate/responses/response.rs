//! OpenAI Responses (`/v1/responses`) responses, JSON and streamed.

use super::*;
use crate::translate::ir::{Block, Emitter, Event, Response, Start, StopReason, Usage};
use crate::translate::sse::{SseEvent, frame};
use crate::translate::{Decoder, Encoder, new_id};

fn read_usage(v: &Value) -> Usage {
    let n = |p: &str| v.pointer(p).and_then(Value::as_u64).unwrap_or(0);
    Usage {
        input: n("/input_tokens"),
        output: n("/output_tokens"),
        cache_read: n("/input_tokens_details/cached_tokens"),
        cache_write: 0,
        reasoning: n("/output_tokens_details/reasoning_tokens"),
    }
}

fn usage_json(u: &Usage) -> Value {
    json!({
        "input_tokens": u.input,
        "input_tokens_details": {"cached_tokens": u.cache_read},
        "output_tokens": u.output,
        "output_tokens_details": {"reasoning_tokens": u.reasoning},
        "total_tokens": u.input + u.output,
    })
}

/// Stop reason of a finished response object.
fn stop_of(resp: &Value, saw_tool_call: bool) -> StopReason {
    match resp.pointer("/incomplete_details/reason").and_then(Value::as_str) {
        Some("max_output_tokens") => StopReason::MaxTokens,
        Some("content_filter") => StopReason::ContentFilter,
        _ if saw_tool_call => StopReason::ToolUse,
        _ => StopReason::End,
    }
}

/// One output item as a block; `None` for items with no counterpart.
fn decode_output_item(item: &Value) -> Option<Block> {
    let s = |k: &str| item.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    match item.get("type").and_then(Value::as_str)? {
        "message" => Some(Block::Text(
            decode_content(item.get("content"))
                .into_iter()
                .filter_map(|p| match p {
                    Part::Text(t) => Some(t),
                    _ => None,
                })
                .collect(),
        )),
        "reasoning" => {
            let mut text = summary_text(item);
            if text.is_empty() {
                text = text_of(item.get("content").unwrap_or(&Value::Null));
            }
            let enc = s("encrypted_content");
            Some(Block::Reasoning(Reasoning { text, opaque: (!enc.is_empty()).then_some(Opaque::OpenaiEncrypted(enc)) }))
        }
        "function_call" => Some(Block::ToolCall(ToolCall { id: s("call_id"), name: s("name"), arguments: s("arguments") })),
        "custom_tool_call" => {
            Some(Block::ToolCall(ToolCall { id: s("call_id"), name: s("name"), arguments: json!({CUSTOM_INPUT: s("input")}).to_string() }))
        }
        _ => None,
    }
}

pub fn decode_response(v: &Value) -> Response {
    let blocks: Vec<Block> = v.get("output").and_then(Value::as_array).into_iter().flatten().filter_map(decode_output_item).collect();
    let saw_tool_call = blocks.iter().any(|b| matches!(b, Block::ToolCall(_)));
    Response { blocks, stop: stop_of(v, saw_tool_call), usage: v.get("usage").map(read_usage).unwrap_or_default() }
}

/// Output items for the client. Calls to `custom` tools go back as
/// `custom_tool_call` with the raw input.
fn output_item(b: &Block, info: &ClientInfo) -> Value {
    match b {
        Block::Text(t) => json!({
            "id": new_id("msg_"), "type": "message", "status": "completed", "role": "assistant",
            "content": [{"type": "output_text", "text": t, "annotations": []}],
        }),
        Block::Reasoning(r) => {
            let summary: Vec<Value> = if r.text.is_empty() { Vec::new() } else { vec![json!({"type": "summary_text", "text": r.text})] };
            let mut item = json!({"id": new_id("rs_"), "type": "reasoning", "summary": summary});
            if let Some(o) = &r.opaque {
                item["encrypted_content"] = json!(o.to_encrypted());
            }
            item
        }
        Block::ToolCall(c) if info.custom_tools.contains(&c.name) => json!({
            "id": new_id("ctc_"), "type": "custom_tool_call", "status": "completed",
            "call_id": c.id, "name": c.name, "input": custom_input(&c.arguments),
        }),
        Block::ToolCall(c) => json!({
            "id": new_id("fc_"), "type": "function_call", "status": "completed",
            "call_id": c.id, "name": c.name, "arguments": c.arguments,
        }),
    }
}

/// A custom tool's raw input from the function-style arguments.
fn custom_input(arguments: &str) -> String {
    serde_json::from_str::<Value>(arguments)
        .ok()
        .and_then(|v| v.get(CUSTOM_INPUT).and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| arguments.to_string())
}

fn response_object(id: &str, created: i64, info: &ClientInfo, output: Vec<Value>, stop: Option<(StopReason, &Usage)>) -> Value {
    let mut v =
        json!({"id": id, "object": "response", "created_at": created, "model": info.model, "output": output, "status": "in_progress"});
    if let Some((stop, usage)) = stop {
        let incomplete = match stop {
            StopReason::MaxTokens => Some("max_output_tokens"),
            StopReason::ContentFilter => Some("content_filter"),
            _ => None,
        };
        v["status"] = json!(if incomplete.is_some() { "incomplete" } else { "completed" });
        v["incomplete_details"] = incomplete.map(|r| json!({"reason": r})).unwrap_or(Value::Null);
        v["usage"] = usage_json(usage);
    }
    v
}

pub fn encode_response(r: &Response, info: &ClientInfo) -> Value {
    let output = r.blocks.iter().map(|b| output_item(b, info)).collect();
    response_object(&new_id("resp_"), chrono::Utc::now().timestamp(), info, output, Some((r.stop, &r.usage)))
}

/// Responses stream events -> [`Event`]s.
#[derive(Default)]
pub struct StreamDecoder {
    em: Emitter,
    /// The open item: its type and whether any delta for it arrived.
    open: Option<(String, bool)>,
    saw_tool_call: bool,
}

impl StreamDecoder {
    /// Content of a finished item, for items whose deltas never came.
    fn fill_from_done(&mut self, item: &Value) {
        match decode_output_item(item) {
            Some(Block::Text(t)) => self.em.delta(&t),
            Some(Block::Reasoning(r)) => self.em.delta(&r.text),
            Some(Block::ToolCall(c)) => self.em.delta(&c.arguments),
            None => {}
        }
    }
}

impl Decoder for StreamDecoder {
    fn feed(&mut self, ev: &SseEvent) -> Vec<Event> {
        let Some(v) = ev.json() else { return Vec::new() };
        let s = |p: &str| v.pointer(p).and_then(Value::as_str).unwrap_or_default();
        let kind = s("/type");
        match kind {
            "response.output_item.added" => {
                let item_type = s("/item/type");
                let start = match item_type {
                    "message" => Some(Start::Text),
                    "reasoning" => Some(Start::Reasoning),
                    "function_call" | "custom_tool_call" => {
                        self.saw_tool_call = true;
                        Some(Start::ToolCall { id: s("/item/call_id").to_string(), name: s("/item/name").to_string() })
                    }
                    _ => None,
                };
                if let Some(start) = start {
                    self.em.start(start);
                    self.open = Some((item_type.to_string(), false));
                }
            }
            "response.output_text.delta"
            | "response.refusal.delta"
            | "response.reasoning_summary_text.delta"
            | "response.reasoning_text.delta"
            | "response.function_call_arguments.delta" => {
                if let Some((_, seen)) = &mut self.open {
                    *seen = true;
                    self.em.delta(s("/delta"));
                }
            }
            "response.reasoning_summary_part.added" => {
                if v.get("summary_index").and_then(Value::as_u64).unwrap_or(0) > 0 {
                    self.em.delta("\n\n");
                }
            }
            "response.output_item.done" => {
                if let Some((item_type, seen)) = self.open.take() {
                    let item = v.get("item").cloned().unwrap_or(Value::Null);
                    // Custom tool input is raw text; it is wrapped as JSON arguments once complete.
                    if !seen || item_type == "custom_tool_call" {
                        self.fill_from_done(&item);
                    }
                    if let Some(enc) = item.get("encrypted_content").and_then(Value::as_str).filter(|e| !e.is_empty()) {
                        self.em.opaque(Opaque::OpenaiEncrypted(enc.to_string()));
                    }
                    self.em.stop();
                }
            }
            "response.completed" | "response.incomplete" | "response.done" => {
                let resp = v.get("response").cloned().unwrap_or(Value::Null);
                let usage = resp.get("usage").map(read_usage).unwrap_or_default();
                self.em.finish(stop_of(&resp, self.saw_tool_call), usage);
            }
            "response.failed" => {
                let e = v.pointer("/response/error").cloned().unwrap_or(Value::Null);
                let code = e.get("code").and_then(Value::as_str).unwrap_or("api_error");
                self.em.error(code, e.get("message").and_then(Value::as_str).unwrap_or("upstream response failed"));
            }
            "error" => {
                let code = v
                    .get("code")
                    .and_then(Value::as_str)
                    .or_else(|| v.pointer("/error/type").and_then(Value::as_str))
                    .unwrap_or("api_error");
                let msg = v
                    .get("message")
                    .and_then(Value::as_str)
                    .or_else(|| v.pointer("/error/message").and_then(Value::as_str))
                    .unwrap_or("upstream error");
                self.em.error(code, msg);
            }
            _ => {}
        }
        self.em.take()
    }

    fn end(&mut self) -> Vec<Event> {
        self.em.end();
        self.em.take()
    }
}

/// The client-side item being streamed.
struct OpenItem {
    id: String,
    index: usize,
    block: Block,
}

/// [`Event`]s -> Responses stream events.
pub struct StreamEncoder {
    info: ClientInfo,
    id: String,
    created: i64,
    seq: u64,
    started: bool,
    output: Vec<Value>,
    open: Option<OpenItem>,
}

impl StreamEncoder {
    pub fn new(info: ClientInfo) -> Self {
        Self { info, id: new_id("resp_"), created: chrono::Utc::now().timestamp(), seq: 0, started: false, output: Vec::new(), open: None }
    }

    fn emit(&mut self, out: &mut String, kind: &str, mut data: Value) {
        data["type"] = json!(kind);
        data["sequence_number"] = json!(self.seq);
        self.seq += 1;
        *out += &frame(Some(kind), &data);
    }

    fn custom(&self, c: &ToolCall) -> bool {
        self.info.custom_tools.contains(&c.name)
    }

    fn start(&mut self, out: &mut String, s: &Start) {
        let index = self.output.len();
        let (block, id, item) = match s {
            Start::Text => {
                let id = new_id("msg_");
                let item = json!({"id": id, "type": "message", "status": "in_progress", "role": "assistant", "content": []});
                (Block::Text(String::new()), id, item)
            }
            Start::Reasoning => {
                let id = new_id("rs_");
                let item = json!({"id": id, "type": "reasoning", "summary": []});
                (Block::Reasoning(Reasoning { text: String::new(), opaque: None }), id, item)
            }
            Start::ToolCall { id: call_id, name } => {
                let call = ToolCall { id: call_id.clone(), name: name.clone(), arguments: String::new() };
                let (id, item) = if self.custom(&call) {
                    let id = new_id("ctc_");
                    let item = json!({"id": id, "type": "custom_tool_call", "status": "in_progress", "call_id": call_id, "name": name, "input": ""});
                    (id, item)
                } else {
                    let id = new_id("fc_");
                    let item = json!({"id": id, "type": "function_call", "status": "in_progress", "call_id": call_id, "name": name, "arguments": ""});
                    (id, item)
                };
                (Block::ToolCall(call), id, item)
            }
        };
        self.emit(out, "response.output_item.added", json!({"output_index": index, "item": item}));
        if matches!(block, Block::Text(_)) {
            let part = json!({"type": "output_text", "text": "", "annotations": []});
            self.emit(out, "response.content_part.added", json!({"item_id": id, "output_index": index, "content_index": 0, "part": part}));
        }
        self.open = Some(OpenItem { id, index, block });
    }

    fn delta(&mut self, out: &mut String, d: &str) {
        let Some(mut item) = self.open.take() else { return };
        let (id, index) = (item.id.clone(), item.index);
        match &mut item.block {
            Block::Text(t) => {
                t.push_str(d);
                self.emit(out, "response.output_text.delta", json!({"item_id": id, "output_index": index, "content_index": 0, "delta": d}));
            }
            Block::Reasoning(r) => {
                if r.text.is_empty() {
                    let part = json!({"type": "summary_text", "text": ""});
                    self.emit(
                        out,
                        "response.reasoning_summary_part.added",
                        json!({"item_id": id, "output_index": index, "summary_index": 0, "part": part}),
                    );
                }
                r.text.push_str(d);
                self.emit(
                    out,
                    "response.reasoning_summary_text.delta",
                    json!({"item_id": id, "output_index": index, "summary_index": 0, "delta": d}),
                );
            }
            Block::ToolCall(c) => {
                c.arguments.push_str(d);
                // Custom tool input is only known once the JSON arguments are complete.
                if !self.info.custom_tools.contains(&c.name) {
                    self.emit(out, "response.function_call_arguments.delta", json!({"item_id": id, "output_index": index, "delta": d}));
                }
            }
        }
        self.open = Some(item);
    }

    fn stop(&mut self, out: &mut String) {
        let Some(item) = self.open.take() else { return };
        let (id, index) = (item.id.clone(), item.index);
        let mut done = output_item(&item.block, &self.info);
        done["id"] = json!(id);
        match &item.block {
            Block::Text(t) => {
                self.emit(out, "response.output_text.done", json!({"item_id": id, "output_index": index, "content_index": 0, "text": t}));
                let part = json!({"type": "output_text", "text": t, "annotations": []});
                self.emit(
                    out,
                    "response.content_part.done",
                    json!({"item_id": id, "output_index": index, "content_index": 0, "part": part}),
                );
            }
            Block::Reasoning(r) if !r.text.is_empty() => {
                self.emit(
                    out,
                    "response.reasoning_summary_text.done",
                    json!({"item_id": id, "output_index": index, "summary_index": 0, "text": r.text}),
                );
                let part = json!({"type": "summary_text", "text": r.text});
                self.emit(
                    out,
                    "response.reasoning_summary_part.done",
                    json!({"item_id": id, "output_index": index, "summary_index": 0, "part": part}),
                );
            }
            Block::Reasoning(_) => {}
            Block::ToolCall(c) if self.custom(c) => {
                let input = custom_input(&c.arguments);
                self.emit(out, "response.custom_tool_call_input.delta", json!({"item_id": id, "output_index": index, "delta": input}));
                self.emit(out, "response.custom_tool_call_input.done", json!({"item_id": id, "output_index": index, "input": input}));
            }
            Block::ToolCall(c) => {
                self.emit(
                    out,
                    "response.function_call_arguments.done",
                    json!({"item_id": id, "output_index": index, "arguments": c.arguments}),
                );
            }
        }
        self.emit(out, "response.output_item.done", json!({"output_index": index, "item": done}));
        self.output.push(done);
    }
}

impl Encoder for StreamEncoder {
    fn encode(&mut self, e: &Event) -> String {
        let mut out = String::new();
        if !self.started {
            self.started = true;
            let r = response_object(&self.id, self.created, &self.info, Vec::new(), None);
            self.emit(&mut out, "response.created", json!({"response": r}));
            self.emit(&mut out, "response.in_progress", json!({"response": r}));
        }
        match e {
            Event::Start(s) => self.start(&mut out, s),
            Event::Delta(d) => self.delta(&mut out, d),
            Event::Opaque(o) => {
                if let Some(OpenItem { block: Block::Reasoning(r), .. }) = &mut self.open {
                    r.opaque = Some(o.clone());
                }
            }
            Event::Stop => self.stop(&mut out),
            Event::Finish { stop, usage } => {
                let r = response_object(&self.id, self.created, &self.info, self.output.clone(), Some((*stop, usage)));
                let kind = if r["status"] == "incomplete" { "response.incomplete" } else { "response.completed" };
                self.emit(&mut out, kind, json!({"response": r}));
            }
            Event::Error { kind, message } => {
                let mut r = response_object(&self.id, self.created, &self.info, self.output.clone(), None);
                r["status"] = json!("failed");
                r["error"] = json!({"code": kind, "message": message});
                self.emit(&mut out, "response.failed", json!({"response": r}));
            }
        }
        out
    }
}
