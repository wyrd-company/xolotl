//! OpenAI Responses (`/v1/responses`).

use serde_json::{Map, Value, json};

use super::ir::{Image, Opaque, Part, Reasoning, Request, ResponseFormat, Role, Tool, ToolCall, ToolChoice, ToolResult};

use super::ClientInfo;

mod response;

pub use response::{StreamDecoder, StreamEncoder, decode_response, encode_response, failure};

/// Free-form (`custom`) tools take one string; other formats see them as a
/// function with this single argument.
const CUSTOM_INPUT: &str = "input";

fn custom_parameters() -> Value {
    json!({"type": "object", "properties": {CUSTOM_INPUT: {"type": "string"}}, "required": [CUSTOM_INPUT]})
}

fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join(""),
        _ => String::new(),
    }
}

// --------------------------------------------------------------- requests

fn decode_content(v: Option<&Value>) -> Vec<Part> {
    match v {
        Some(Value::String(s)) => vec![Part::Text(s.clone())],
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| match p.get("type").and_then(Value::as_str) {
                Some("input_text" | "output_text" | "text") => p.get("text").and_then(Value::as_str).map(|t| Part::Text(t.to_string())),
                Some("refusal") => p.get("refusal").and_then(Value::as_str).map(|t| Part::Text(t.to_string())),
                Some("input_image") => {
                    let url = p.get("image_url").and_then(|u| u.as_str().or_else(|| u.get("url").and_then(Value::as_str)))?;
                    Some(Part::Image(Image::from_url(url)))
                }
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

pub fn decode_request(v: &Value, info: &mut ClientInfo) -> Result<Request, String> {
    if v.get("previous_response_id").is_some_and(|p| !p.is_null()) {
        return Err(
            "previous_response_id is not supported when this model is reached through translation; send the whole conversation".into()
        );
    }
    let mut r = Request {
        max_tokens: v.get("max_output_tokens").and_then(Value::as_u64),
        temperature: v.get("temperature").and_then(Value::as_f64),
        top_p: v.get("top_p").and_then(Value::as_f64),
        stream: v.get("stream").and_then(Value::as_bool).unwrap_or(false),
        effort: v.pointer("/reasoning/effort").and_then(Value::as_str).map(str::to_string),
        parallel_tool_calls: v.get("parallel_tool_calls").and_then(Value::as_bool),
        ..Default::default()
    };
    if let Some(i) = v.get("instructions").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        r.system.push(i.to_string());
    }
    match v.get("input") {
        Some(Value::String(s)) => r.push(Role::User, vec![Part::Text(s.clone())]),
        Some(Value::Array(items)) => {
            for item in items {
                decode_item(item, &mut r);
            }
        }
        _ => {}
    }
    for t in v.get("tools").and_then(Value::as_array).into_iter().flatten() {
        decode_tool(t, None, &mut r, info);
    }
    r.tool_choice = match v.get("tool_choice") {
        Some(Value::String(s)) => match s.as_str() {
            "auto" => Some(ToolChoice::Auto),
            "none" => Some(ToolChoice::None),
            "required" => Some(ToolChoice::Required),
            _ => None,
        },
        Some(o @ Value::Object(_)) if matches!(o.get("type").and_then(Value::as_str), Some("function" | "custom")) => {
            o.get("name").and_then(Value::as_str).map(|n| ToolChoice::Tool(qualify(o.get("namespace").and_then(Value::as_str), n)))
        }
        _ => None,
    };
    r.response_format = v.pointer("/text/format").and_then(|f| match f.get("type").and_then(Value::as_str) {
        Some("json_object") => Some(ResponseFormat::JsonObject),
        Some("json_schema") => Some(ResponseFormat::JsonSchema {
            name: f.get("name").and_then(Value::as_str).unwrap_or("output").to_string(),
            schema: f.get("schema").cloned().unwrap_or(Value::Null),
            strict: f.get("strict").and_then(Value::as_bool),
        }),
        _ => None,
    });
    Ok(r)
}

/// The flat name of a tool in a namespace, as CLIProxyAPI spells it:
/// `<namespace>__<name>`. MCP names (`mcp__...`) and names already carrying
/// the namespace stay as they are.
fn qualify(namespace: Option<&str>, name: &str) -> String {
    match namespace.map(str::trim).filter(|n| !n.is_empty()) {
        None => name.to_string(),
        Some(_) if name.starts_with("mcp__") => name.to_string(),
        Some(ns) if name == ns || name.starts_with(&format!("{ns}__")) => name.to_string(),
        Some(ns) if ns.ends_with("__") => format!("{ns}{name}"),
        Some(ns) => format!("{ns}__{name}"),
    }
}

/// One client tool; a namespace contributes each of its tools under a
/// qualified name, remembered so calls go back with their namespace.
fn decode_tool(t: &Value, namespace: Option<&str>, r: &mut Request, info: &mut ClientInfo) {
    let plain = t.get("name").and_then(Value::as_str).unwrap_or_default();
    let name = qualify(namespace, plain);
    if let Some(ns) = namespace
        && name != plain
    {
        info.namespaced.insert(name.clone(), (ns.to_string(), plain.to_string()));
    }
    let description = t.get("description").and_then(Value::as_str).map(str::to_string);
    match t.get("type").and_then(Value::as_str) {
        Some("function") => {
            r.tools.push(Tool { name, description, parameters: t.get("parameters").cloned().unwrap_or_else(|| json!({"type": "object"})) })
        }
        Some("namespace") => {
            for child in t.get("tools").and_then(Value::as_array).into_iter().flatten() {
                decode_tool(child, Some(plain), r, info);
            }
        }
        Some("custom") => {
            // Keep the input grammar where the model can read it.
            let format = t.get("format").filter(|f| f.get("type").and_then(Value::as_str) == Some("grammar"));
            let description = match (description, format) {
                (d, Some(f)) => Some(format!(
                    "{}\n\nThe `{CUSTOM_INPUT}` argument must follow this {} grammar:\n{}",
                    d.unwrap_or_default(),
                    f.get("syntax").and_then(Value::as_str).unwrap_or("formal"),
                    f.get("definition").and_then(Value::as_str).unwrap_or_default()
                )),
                (d, None) => d,
            };
            info.custom_tools.insert(name.clone());
            r.tools.push(Tool { name, description, parameters: custom_parameters() });
        }
        // Hosted tools (web search, file search, shell, ...) have no counterpart.
        _ => {}
    }
}

fn decode_item(item: &Value, r: &mut Request) {
    let s = |k: &str| item.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    let kind = item.get("type").and_then(Value::as_str).unwrap_or(if item.get("role").is_some() { "message" } else { "" });
    match kind {
        "message" => match item.get("role").and_then(Value::as_str) {
            Some("system" | "developer") => {
                let text = text_of(item.get("content").unwrap_or(&Value::Null));
                if !text.is_empty() {
                    r.system.push(text);
                }
            }
            Some("assistant") => r.push(Role::Assistant, decode_content(item.get("content"))),
            _ => r.push(Role::User, decode_content(item.get("content"))),
        },
        "reasoning" => {
            let mut text = summary_text(item);
            if text.is_empty() {
                text = text_of(item.get("content").unwrap_or(&Value::Null));
            }
            let opaque = Opaque::from_encrypted(&s("encrypted_content"));
            r.push(Role::Assistant, vec![Part::Reasoning(Reasoning { text, opaque })]);
        }
        "function_call" | "custom_tool_call" => {
            let name = qualify(item.get("namespace").and_then(Value::as_str), &s("name"));
            let arguments = if kind == "function_call" { s("arguments") } else { json!({CUSTOM_INPUT: s("input")}).to_string() };
            r.push(Role::Assistant, vec![Part::ToolCall(ToolCall { id: s("call_id"), name, arguments })]);
        }
        "function_call_output" | "custom_tool_call_output" => r.push(
            Role::User,
            vec![Part::ToolResult(ToolResult { call_id: s("call_id"), content: decode_content(item.get("output")), is_error: false })],
        ),
        _ => {}
    }
}

fn summary_text(item: &Value) -> String {
    item.get("summary")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|p| p.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn encode_content(parts: &[&Part], role: Role) -> Vec<Value> {
    parts
        .iter()
        .filter_map(|p| match p {
            Part::Text(t) if role == Role::Assistant => Some(json!({"type": "output_text", "text": t})),
            Part::Text(t) => Some(json!({"type": "input_text", "text": t})),
            Part::Image(i) if role == Role::User => Some(json!({"type": "input_image", "image_url": i.to_url()})),
            _ => None,
        })
        .collect()
}

pub fn encode_request(r: &Request, model: &str) -> Value {
    let mut input: Vec<Value> = Vec::new();
    for m in &r.messages {
        // Plain content between other items becomes one message item.
        let mut pending: Vec<&Part> = Vec::new();
        let flush = |pending: &mut Vec<&Part>, input: &mut Vec<Value>| {
            let content = encode_content(pending, m.role);
            pending.clear();
            if !content.is_empty() {
                let role = if m.role == Role::Assistant { "assistant" } else { "user" };
                input.push(json!({"type": "message", "role": role, "content": content}));
            }
        };
        for p in &m.parts {
            match p {
                Part::Text(_) | Part::Image(_) => pending.push(p),
                Part::Reasoning(x) => {
                    // Only OpenAI's own reasoning can be replayed to it.
                    if let Some(Opaque::OpenaiEncrypted(enc)) = &x.opaque {
                        flush(&mut pending, &mut input);
                        let summary: Vec<Value> =
                            if x.text.is_empty() { Vec::new() } else { vec![json!({"type": "summary_text", "text": x.text})] };
                        input.push(json!({"type": "reasoning", "summary": summary, "encrypted_content": enc}));
                    }
                }
                Part::ToolCall(c) => {
                    flush(&mut pending, &mut input);
                    input.push(json!({"type": "function_call", "call_id": c.id, "name": c.name, "arguments": c.arguments}));
                }
                Part::ToolResult(res) => {
                    flush(&mut pending, &mut input);
                    let output = if res.images().next().is_none() {
                        Value::String(res.text())
                    } else {
                        Value::Array(encode_content(&res.content.iter().collect::<Vec<_>>(), Role::User))
                    };
                    input.push(json!({"type": "function_call_output", "call_id": res.call_id, "output": output}));
                }
            }
        }
        flush(&mut pending, &mut input);
    }

    // System text goes in a developer message, as CLIProxyAPI does: the
    // ChatGPT backend reserves `instructions` for Codex's own prompt.
    if !r.system.is_empty() {
        let dev = json!({"type": "message", "role": "developer", "content": [{"type": "input_text", "text": r.system.join("\n\n")}]});
        input.insert(0, dev);
    }
    let mut out = Map::new();
    out.insert("model".into(), json!(model));
    out.insert("input".into(), Value::Array(input));
    if !r.tools.is_empty() {
        let tools: Vec<Value> = r
            .tools
            .iter()
            .map(|t| {
                let mut v = json!({"type": "function", "name": t.name, "parameters": t.parameters});
                if let Some(d) = &t.description {
                    v["description"] = json!(d);
                }
                v
            })
            .collect();
        out.insert("tools".into(), Value::Array(tools));
        if let Some(p) = r.parallel_tool_calls {
            out.insert("parallel_tool_calls".into(), json!(p));
        }
    }
    if let Some(tc) = &r.tool_choice {
        out.insert(
            "tool_choice".into(),
            match tc {
                ToolChoice::Auto => json!("auto"),
                ToolChoice::None => json!("none"),
                ToolChoice::Required => json!("required"),
                ToolChoice::Tool(n) => json!({"type": "function", "name": n}),
            },
        );
    }
    if let Some(n) = r.max_tokens {
        out.insert("max_output_tokens".into(), json!(n));
    }
    if let Some(t) = r.temperature {
        out.insert("temperature".into(), json!(t));
    }
    if let Some(p) = r.top_p {
        out.insert("top_p".into(), json!(p));
    }
    if let Some(e) = &r.effort {
        out.insert("reasoning".into(), json!({"effort": e, "summary": "auto"}));
        // Stateless: reasoning comes back encrypted so the next turn can replay it.
        out.insert("include".into(), json!(["reasoning.encrypted_content"]));
    }
    match &r.response_format {
        Some(ResponseFormat::JsonObject) => {
            out.insert("text".into(), json!({"format": {"type": "json_object"}}));
        }
        Some(ResponseFormat::JsonSchema { name, schema, strict }) => {
            let mut f = json!({"type": "json_schema", "name": name, "schema": schema});
            if let Some(s) = strict {
                f["strict"] = json!(s);
            }
            out.insert("text".into(), json!({"format": f}));
        }
        None => {}
    }
    out.insert("store".into(), Value::Bool(false));
    out.insert("stream".into(), Value::Bool(r.stream));
    Value::Object(out)
}
