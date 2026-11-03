//! Tool calls named in an upstream LLM response.
// ---
// relationships:
//   implements: ../../docs/design/sessions.md
// ---
use std::collections::HashSet;

use serde_json::Value;

/// Longest tool name kept. Names come from the model, so they are bounded.
const MAX_NAME: usize = 128;

/// Whether an SSE data line can name a tool call, so most lines skip the parse.
pub fn mentions_tool(data: &[u8]) -> bool {
    [&b"tool_use"[..], b"tool_calls", b"function_call", b"custom_tool_call"]
        .iter()
        .any(|needle| data.windows(needle.len()).any(|w| w == *needle))
}

/// The tool calls of one response, from Messages, Responses or Chat payloads
/// and stream events. A stream event names each call once; a Chat stream can
/// repeat the name, so its calls count once per choice and index.
#[derive(Debug, Default)]
pub struct ToolCalls {
    names: Vec<String>,
    chat_seen: HashSet<(u64, u64)>,
}

impl ToolCalls {
    pub fn observe(&mut self, v: &Value) {
        // Messages: a whole response, or the start of a stream's content block.
        for block in array(v, "content").chain(v.get("content_block")) {
            if matches!(block.get("type").and_then(Value::as_str), Some("tool_use" | "server_tool_use")) {
                self.push(block.get("name"), None);
            }
        }
        // Responses: a whole response, or a stream's finished item. The final
        // `response.completed` repeats the items under `response`, so it is not read.
        let done = (v.get("type").and_then(Value::as_str) == Some("response.output_item.done")).then(|| v.get("item")).flatten();
        for item in array(v, "output").chain(done) {
            if matches!(item.get("type").and_then(Value::as_str), Some("function_call" | "custom_tool_call")) {
                self.push(item.get("name"), item.get("namespace").and_then(Value::as_str));
            }
        }
        // Chat: a whole message, or stream deltas.
        for (position, choice) in array(v, "choices").enumerate() {
            let number = choice.get("index").and_then(Value::as_u64).unwrap_or(position as u64);
            for (i, call) in choice.pointer("/message/tool_calls").and_then(Value::as_array).into_iter().flatten().enumerate() {
                self.push_chat(number, call.get("index").and_then(Value::as_u64).unwrap_or(i as u64), call);
            }
            for (i, call) in choice.pointer("/delta/tool_calls").and_then(Value::as_array).into_iter().flatten().enumerate() {
                self.push_chat(number, call.get("index").and_then(Value::as_u64).unwrap_or(i as u64), call);
            }
        }
    }

    fn push_chat(&mut self, choice: u64, index: u64, call: &Value) {
        let name = call.pointer("/function/name");
        if name.and_then(Value::as_str).is_some_and(|n| !n.is_empty()) && self.chat_seen.insert((choice, index)) {
            self.push(name, None);
        }
    }

    fn push(&mut self, name: Option<&Value>, namespace: Option<&str>) {
        let Some(name) = name.and_then(Value::as_str).filter(|n| !n.is_empty()) else { return };
        let full = match namespace.filter(|n| !n.is_empty()) {
            Some(ns) => format!("{ns}__{name}"),
            None => name.to_string(),
        };
        self.names.push(full.chars().take(MAX_NAME).collect());
    }

    pub fn into_names(self) -> Vec<String> {
        self.names
    }
}

fn array<'a>(v: &'a Value, key: &str) -> impl Iterator<Item = &'a Value> {
    v.get(key).and_then(Value::as_array).into_iter().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn names(events: &[Value]) -> Vec<String> {
        let mut calls = ToolCalls::default();
        events.iter().for_each(|e| calls.observe(e));
        calls.into_names()
    }

    #[test]
    fn messages_response_and_stream() {
        let whole = json!({"content": [{"type": "text", "text": "x"}, {"type": "tool_use", "name": "read"}, {"type": "server_tool_use", "name": "search"}]});
        assert_eq!(names(&[whole]), ["read", "search"]);
        let start = json!({"type": "content_block_start", "content_block": {"type": "tool_use", "name": "read"}});
        let text = json!({"type": "content_block_start", "content_block": {"type": "text", "text": ""}});
        assert_eq!(names(&[start, text]), ["read"]);
    }

    #[test]
    fn responses_counts_each_item_once_across_a_stream() {
        let item = json!({"type": "function_call", "name": "read"});
        let events = [
            json!({"type": "response.output_item.added", "item": item}),
            json!({"type": "response.output_item.done", "item": item}),
            json!({"type": "response.completed", "response": {"output": [item]}}),
        ];
        assert_eq!(names(&events), ["read"]);
        let whole = json!({"output": [{"type": "custom_tool_call", "name": "patch"}, {"type": "function_call", "namespace": "ns", "name": "run"}, {"type": "message"}]});
        assert_eq!(names(&[whole]), ["patch", "ns__run"]);
    }

    #[test]
    fn chat_counts_parallel_calls_and_repeated_names_once() {
        let first = json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "function": {"name": "read", "arguments": ""}}, {"index": 1, "function": {"name": "write"}}]}}]});
        let repeat =
            json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "function": {"name": "read", "arguments": "{}"}}]}}]});
        let args = json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 1, "function": {"arguments": "{}"}}]}}]});
        assert_eq!(names(&[first, repeat, args]), ["read", "write"]);
        let whole = json!({"choices": [{"message": {"tool_calls": [{"function": {"name": "a"}}, {"function": {"name": "b"}}]}}]});
        assert_eq!(names(&[whole]), ["a", "b"]);
    }

    #[test]
    fn a_long_name_is_bounded() {
        let whole = json!({"content": [{"type": "tool_use", "name": "x".repeat(500)}]});
        assert_eq!(names(&[whole])[0].len(), MAX_NAME);
    }

    #[test]
    fn the_filter_passes_every_shape_it_must_read() {
        for line in
            [r#"{"type":"tool_use"}"#, r#"{"delta":{"tool_calls":[]}}"#, r#"{"type":"function_call"}"#, r#"{"type":"custom_tool_call"}"#]
        {
            assert!(mentions_tool(line.as_bytes()), "{line}");
        }
        assert!(!mentions_tool(br#"{"type":"response.output_text.delta","delta":"hi"}"#));
    }
}
