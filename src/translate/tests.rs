use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use serde_json::{Value, json};

use super::*;

fn translate(client: Wire, upstream: Wire, body: Value) -> (Value, Translation) {
    request(client, upstream, &body, "up-model").unwrap()
}

fn upstream_response(status: u16, content_type: &str, body: impl Into<String>) -> Response {
    let mut r = Response::new(Body::from(body.into()));
    *r.status_mut() = StatusCode::from_u16(status).unwrap();
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_str(content_type).unwrap());
    r
}

async fn run(t: Translation, resp: Response) -> (u16, HeaderMap, String) {
    let out = t.response(resp).await;
    let status = out.status().as_u16();
    let headers = out.headers().clone();
    let bytes = axum::body::to_bytes(out.into_body(), usize::MAX).await.unwrap();
    (status, headers, String::from_utf8(bytes.to_vec()).unwrap())
}

/// Every SSE `data:` payload, parsed.
fn events(body: &str) -> Vec<Value> {
    let mut p = SseParser::default();
    let mut evs = p.feed(body.as_bytes());
    evs.extend(p.end());
    evs.into_iter().filter_map(|e| e.json()).collect()
}

fn sse(events: &[Value]) -> String {
    events.iter().map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap_or("message"))).collect()
}

fn of_type<'a>(evs: &'a [Value], kind: &str) -> Vec<&'a Value> {
    evs.iter().filter(|e| e["type"] == kind).collect()
}

// ----------------------------------------------------------------- requests

#[test]
fn messages_request_to_responses() {
    let long = "mcp__plugin_some_server__a_tool_with_a_really_long_descriptive_name_beyond_limits";
    let body = json!({
        "model": "fast",
        "system": [{"type": "text", "text": "x-anthropic-billing-header: cc=1"}, {"type": "text", "text": "be brief"}],
        "max_tokens": 1000,
        "stream": true,
        "thinking": {"type": "enabled", "budget_tokens": 20000},
        "tools": [
            {"name": long, "description": "d", "input_schema": {"type": "object"}},
            {"type": "web_search_20250305", "name": "web_search"},
        ],
        "messages": [
            {"role": "user", "content": [
                {"type": "text", "text": "look"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}},
            ]},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "from claude", "signature": "native-sig"},
                {"type": "thinking", "thinking": "from codex", "signature": "xolotl.openai.ENC"},
                {"type": "tool_use", "id": "call_1", "name": long, "input": {"q": 1}},
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "call_1", "content": "result"},
                {"type": "text", "text": "go on"},
            ]},
        ],
    });
    let (out, t) = translate(Wire::Messages, Wire::Responses, body);
    assert_eq!(out["model"], "up-model");
    assert!(out.get("instructions").is_none());
    assert_eq!(out["max_output_tokens"], 1000);
    assert_eq!(out["reasoning"], json!({"effort": "high", "summary": "auto"}));
    assert_eq!(out["include"], json!(["reasoning.encrypted_content"]));
    assert_eq!(out["store"], false);
    assert_eq!(out["stream"], true);
    // Server tools are dropped; the long name is shortened to OpenAI's limit.
    let tools = out["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    let short = tools[0]["name"].as_str().unwrap();
    assert!(short.len() <= 64 && short != long, "{short}");
    let input = out["input"].as_array().unwrap();
    let kinds: Vec<&str> = input.iter().map(|i| i["type"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["message", "message", "reasoning", "function_call", "function_call_output", "message"]);
    // System text leads as a developer message, without the billing block.
    assert_eq!(input[0], json!({"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "be brief"}]}));
    let input = &input[1..];
    assert_eq!(input[0]["content"][1], json!({"type": "input_image", "image_url": "data:image/png;base64,AAAA"}));
    // Only OpenAI's own reasoning is replayed, untagged.
    assert_eq!(input[1]["encrypted_content"], "ENC");
    assert_eq!(input[1]["summary"][0]["text"], "from codex");
    assert_eq!(input[2]["name"], short);
    assert_eq!(input[2]["arguments"], r#"{"q":1}"#);
    assert_eq!(input[3], json!({"type": "function_call_output", "call_id": "call_1", "output": "result"}));
    assert_eq!(t.renamed.get(short).map(String::as_str), Some(long));
}

#[test]
fn responses_request_to_messages() {
    let body = json!({
        "model": "m",
        "instructions": "base",
        "reasoning": {"effort": "medium"},
        "temperature": 0.5,
        "tools": [
            {"type": "function", "name": "shell", "parameters": {"type": "object"}},
            {"type": "custom", "name": "apply_patch", "description": "patch files",
             "format": {"type": "grammar", "syntax": "lark", "definition": "start: x"}},
            {"type": "web_search"},
        ],
        "input": [
            {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "dev rules"}]},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]},
            {"type": "reasoning", "summary": [{"type": "summary_text", "text": "thought"}], "encrypted_content": "xolotl.anthropic.SIG"},
            {"type": "reasoning", "summary": [], "encrypted_content": "openai-only"},
            {"type": "custom_tool_call", "call_id": "c1", "name": "apply_patch", "input": "*** Begin Patch"},
            {"type": "custom_tool_call_output", "call_id": "c1", "output": "done"},
        ],
    });
    let (out, t) = translate(Wire::Responses, Wire::Messages, body);
    assert_eq!(out["system"], "base\n\ndev rules");
    assert_eq!(out["max_tokens"], messages::DEFAULT_MAX_TOKENS);
    let tools = out["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[1]["input_schema"]["required"], json!(["input"]));
    assert!(tools[1]["description"].as_str().unwrap().contains("start: x"));
    assert!(t.info.custom_tools.contains("apply_patch"));
    let msgs = out["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 3);
    let assistant = msgs[1]["content"].as_array().unwrap();
    // Anthropic's signature comes back untagged; OpenAI's reasoning is dropped.
    assert_eq!(assistant[0], json!({"type": "thinking", "thinking": "thought", "signature": "SIG"}));
    assert_eq!(assistant[1], json!({"type": "tool_use", "id": "c1", "name": "apply_patch", "input": {"input": "*** Begin Patch"}}));
    assert_eq!(msgs[2]["content"][0]["type"], "tool_result");
    // The tool turn carries its thinking, so thinking stays on (budget form for non-Anthropic servers) and drops sampling.
    assert_eq!(out["thinking"], json!({"type": "enabled", "budget_tokens": 8192}));
    assert!(out.get("temperature").is_none());
}

#[test]
fn thinking_takes_a_budget_and_is_off_for_unsigned_tool_turns() {
    let body = json!({"model": "m", "reasoning_effort": "xhigh", "messages": [{"role": "user", "content": "hi"}]});
    let (out, _) = request(Wire::Chat, Wire::Messages, &body, "m").unwrap();
    assert_eq!(out["thinking"], json!({"type": "enabled", "budget_tokens": 32768}));
    assert_eq!(out["max_tokens"], 32768 + 4096, "max_tokens leaves room above the budget");
    assert!(out.get("output_config").is_none());

    // A tool turn from a Chat client has no signed thinking to replay.
    let body = json!({"model": "m", "reasoning_effort": "high", "temperature": 0.2, "top_p": 0.9, "messages": [
        {"role": "user", "content": "hi"},
        {"role": "assistant", "content": null, "tool_calls": [{"id": "a.b", "type": "function", "function": {"name": "f", "arguments": "{}"}}]},
        {"role": "tool", "tool_call_id": "a.b", "content": "r1"},
        {"role": "user", "content": "and?"},
    ]});
    let (out, _) = request(Wire::Chat, Wire::Messages, &body, "m").unwrap();
    assert!(out.get("thinking").is_none(), "{out}");
    assert_eq!(out["temperature"], 0.2);
    assert!(out.get("top_p").is_none(), "only one sampling parameter");
    let msgs = out["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 3);
    // Ids are sanitised the same way on both sides; the tool result leads the user turn.
    assert_eq!(msgs[1]["content"][0]["id"], "a_b");
    assert_eq!(msgs[2]["content"][0], json!({"type": "tool_result", "tool_use_id": "a_b", "content": [{"type": "text", "text": "r1"}]}));
    assert_eq!(msgs[2]["content"][1], json!({"type": "text", "text": "and?"}));
}

#[test]
fn forced_tools_turn_thinking_off_and_tool_results_lead_the_user_turn() {
    let body = json!({
        "model": "m", "reasoning": {"effort": "high"}, "tool_choice": "required",
        "tools": [{"type": "function", "name": "f", "parameters": {"type": "object"}}],
        "input": [
            {"type": "function_call", "call_id": "c1", "name": "f", "arguments": "{}"},
            {"type": "message", "role": "user", "content": "a note first"},
            {"type": "function_call_output", "call_id": "c1", "output": "r"},
        ],
    });
    let (out, _) = request(Wire::Responses, Wire::Messages, &body, "m").unwrap();
    assert!(out.get("thinking").is_none(), "Claude rejects thinking with a forced tool: {out}");
    assert_eq!(out["tool_choice"], json!({"type": "any"}));
    let user = out["messages"][1]["content"].as_array().unwrap();
    assert_eq!(user[0]["type"], "tool_result");
    assert_eq!(user[1], json!({"type": "text", "text": "a note first"}));

    // A forced tool alone turns thinking off; the same request unforced keeps it.
    let mut body = json!({"model": "m", "reasoning": {"effort": "high"}, "input": "hi",
        "tools": [{"type": "function", "name": "f", "parameters": {"type": "object"}}]});
    let (out, _) = request(Wire::Responses, Wire::Messages, &body, "m").unwrap();
    assert_eq!(out["thinking"], json!({"type": "enabled", "budget_tokens": 24576}));
    body["tool_choice"] = json!({"type": "function", "name": "f"});
    let (out, _) = request(Wire::Responses, Wire::Messages, &body, "m").unwrap();
    assert!(out.get("thinking").is_none(), "{out}");
    assert_eq!(out["tool_choice"], json!({"type": "tool", "name": "f"}));
}

#[test]
fn claude_models_take_adaptive_thinking_and_others_a_budget() {
    for (effort, sent) in [("minimal", "low"), ("low", "low"), ("medium", "medium"), ("high", "high"), ("xhigh", "xhigh")] {
        let body = json!({"model": "m", "reasoning_effort": effort, "temperature": 0.3, "messages": [{"role": "user", "content": "hi"}]});
        for model in ["claude-haiku-5-5", "anthropic/claude-haiku-5-5"] {
            let (out, _) = request(Wire::Chat, Wire::Messages, &body, model).unwrap();
            assert_eq!(out["thinking"], json!({"type": "adaptive"}), "{model}");
            assert_eq!(out["output_config"], json!({"effort": sent}), "{model} {effort}");
            assert!(out.get("temperature").is_none());
            assert_eq!(out["max_tokens"], messages::DEFAULT_MAX_TOKENS, "no budget to make room for");
        }
        let (out, _) = request(Wire::Chat, Wire::Messages, &body, "minimax-m3").unwrap();
        assert_eq!(out["thinking"]["type"], "enabled", "{effort}");
        assert!(out.get("output_config").is_none());
    }
}

#[test]
fn messages_request_to_chat() {
    let body = json!({
        "model": "m", "stream": true, "stop_sequences": ["END"],
        "tool_choice": {"type": "any", "disable_parallel_tool_use": true},
        "tools": [{"name": "f", "input_schema": {"type": "object"}}],
        "messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": [{"type": "text", "text": "calling"}, {"type": "tool_use", "id": "t1", "name": "f", "input": {}}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": [
                {"type": "text", "text": "see image"},
                {"type": "image", "source": {"type": "url", "url": "https://x/img.png"}},
            ]}]},
        ],
    });
    let (out, _) = translate(Wire::Messages, Wire::Chat, body);
    assert_eq!(out["stream_options"], json!({"include_usage": true}));
    assert_eq!(out["stop"], json!(["END"]));
    assert_eq!(out["tool_choice"], "required");
    assert_eq!(out["parallel_tool_calls"], false);
    let msgs = out["messages"].as_array().unwrap();
    let roles: Vec<&str> = msgs.iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(roles, ["user", "assistant", "tool", "user"]);
    assert_eq!(msgs[1]["tool_calls"][0]["function"], json!({"name": "f", "arguments": "{}"}));
    assert_eq!(msgs[2], json!({"role": "tool", "tool_call_id": "t1", "content": "see image"}));
    assert_eq!(msgs[3]["content"][0], json!({"type": "image_url", "image_url": {"url": "https://x/img.png"}}));
}

#[test]
fn chat_and_responses_requests_map_structured_output() {
    let body = json!({"model": "m", "messages": [{"role": "user", "content": "hi"}],
        "response_format": {"type": "json_schema", "json_schema": {"name": "x", "schema": {"type": "object"}, "strict": true}}});
    let (out, _) = translate(Wire::Chat, Wire::Responses, body.clone());
    assert_eq!(out["text"]["format"], json!({"type": "json_schema", "name": "x", "schema": {"type": "object"}, "strict": true}));
    let err = request(Wire::Chat, Wire::Messages, &body, "m").unwrap_err();
    assert!(err.contains("structured output"), "{err}");
}

#[test]
fn responses_previous_response_id_is_rejected() {
    let body = json!({"model": "m", "input": "hi", "previous_response_id": "resp_1"});
    let err = request(Wire::Responses, Wire::Chat, &body, "m").unwrap_err();
    assert!(err.contains("previous_response_id"));
}

// ---------------------------------------------------------------- responses

fn responses_stream() -> String {
    sse(&[
        json!({"type": "response.created", "response": {"id": "r"}}),
        json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "reasoning", "id": "rs_1"}}),
        json!({"type": "response.reasoning_summary_text.delta", "delta": "plan"}),
        json!({"type": "response.output_item.done", "item": {"type": "reasoning", "encrypted_content": "ENC", "summary": [{"type": "summary_text", "text": "plan"}]}}),
        json!({"type": "response.output_item.added", "output_index": 1, "item": {"type": "message", "id": "m1"}}),
        json!({"type": "response.output_text.delta", "delta": "Hel"}),
        json!({"type": "response.output_text.delta", "delta": "lo"}),
        json!({"type": "response.output_item.done", "item": {"type": "message"}}),
        json!({"type": "response.output_item.added", "output_index": 2, "item": {"type": "function_call", "call_id": "call_9", "name": "SHORT"}}),
        json!({"type": "response.function_call_arguments.delta", "delta": "{\"a\":"}),
        json!({"type": "response.function_call_arguments.delta", "delta": "1}"}),
        json!({"type": "response.output_item.done", "item": {"type": "function_call", "arguments": "{\"a\":1}"}}),
        json!({"type": "response.completed", "response": {"status": "completed", "usage": {
            "input_tokens": 100, "input_tokens_details": {"cached_tokens": 40}, "output_tokens": 7, "output_tokens_details": {"reasoning_tokens": 3}}}}),
    ])
}

#[tokio::test]
async fn responses_stream_to_messages_stream() {
    let (_, mut t) = translate(Wire::Messages, Wire::Responses, json!({"model": "fast", "stream": true, "messages": []}));
    t.renamed.insert("SHORT".into(), "original_name".into());
    let (status, headers, body) = run(t, upstream_response(200, "text/event-stream", responses_stream())).await;
    assert_eq!(status, 200);
    assert_eq!(headers[header::CONTENT_TYPE], "text/event-stream");
    let evs = events(&body);
    let kinds: Vec<&str> = evs.iter().map(|e| e["type"].as_str().unwrap()).collect();
    assert_eq!(kinds.first(), Some(&"message_start"));
    assert_eq!(kinds.last(), Some(&"message_stop"));
    assert_eq!(evs[0]["message"]["model"], "fast");
    let starts = of_type(&evs, "content_block_start");
    assert_eq!(starts.len(), 3);
    assert_eq!(starts[0]["content_block"]["type"], "thinking");
    assert_eq!(starts[2]["content_block"], json!({"type": "tool_use", "id": "call_9", "name": "original_name", "input": {}}));
    let deltas = of_type(&evs, "content_block_delta");
    assert!(deltas.iter().any(|d| d["delta"] == json!({"type": "signature_delta", "signature": "xolotl.openai.ENC"})));
    let text: String = deltas.iter().filter_map(|d| d["delta"]["text"].as_str()).collect();
    assert_eq!(text, "Hello");
    let args: String = deltas.iter().filter_map(|d| d["delta"]["partial_json"].as_str()).collect();
    assert_eq!(args, "{\"a\":1}");
    let md = of_type(&evs, "message_delta")[0];
    assert_eq!(md["delta"]["stop_reason"], "tool_use");
    assert_eq!(
        md["usage"],
        json!({"input_tokens": 60, "output_tokens": 7, "cache_read_input_tokens": 40, "cache_creation_input_tokens": 0})
    );
}

#[tokio::test]
async fn responses_stream_collected_for_non_streaming_chat_client() {
    let (_, mut t) = translate(Wire::Chat, Wire::Responses, json!({"model": "fast", "messages": [{"role": "user", "content": "hi"}]}));
    t.renamed.insert("SHORT".into(), "original_name".into());
    let (status, headers, body) = run(t, upstream_response(200, "text/event-stream", responses_stream())).await;
    assert_eq!(status, 200);
    assert_eq!(headers[header::CONTENT_TYPE], "application/json");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["object"], "chat.completion");
    assert_eq!(v["model"], "fast");
    let msg = &v["choices"][0]["message"];
    assert_eq!(msg["content"], "Hello");
    assert_eq!(msg["reasoning_content"], "plan");
    assert_eq!(msg["tool_calls"][0]["function"], json!({"name": "original_name", "arguments": "{\"a\":1}"}));
    assert_eq!(v["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(v["usage"]["prompt_tokens"], 100);
    assert_eq!(v["usage"]["prompt_tokens_details"]["cached_tokens"], 40);
    assert_eq!(v["usage"]["completion_tokens_details"]["reasoning_tokens"], 3);
}

fn messages_stream() -> String {
    sse(&[
        json!({"type": "message_start", "message": {"usage": {"input_tokens": 10, "cache_read_input_tokens": 90, "cache_creation_input_tokens": 5, "output_tokens": 1}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "hmm"}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "SIG"}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "content_block_start", "index": 1, "content_block": {"type": "server_tool_use", "id": "s", "name": "web_search"}}),
        json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "{}"}}),
        json!({"type": "content_block_stop", "index": 1}),
        json!({"type": "content_block_start", "index": 2, "content_block": {"type": "tool_use", "id": "toolu_1", "name": "apply_patch", "input": {}}}),
        json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "{\"input\":\"*** Begin"}}),
        json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": " Patch\"}"}}),
        json!({"type": "content_block_stop", "index": 2}),
        json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 30}}),
        json!({"type": "message_stop"}),
    ])
}

#[tokio::test]
async fn messages_stream_to_responses_stream_with_custom_tool() {
    let body = json!({"model": "fast", "stream": true, "input": "hi", "tools": [{"type": "custom", "name": "apply_patch"}]});
    let (_, t) = translate(Wire::Responses, Wire::Messages, body);
    let (status, _, body) = run(t, upstream_response(200, "text/event-stream", messages_stream())).await;
    assert_eq!(status, 200);
    let evs = events(&body);
    assert_eq!(evs[0]["type"], "response.created");
    let seqs: Vec<u64> = evs.iter().map(|e| e["sequence_number"].as_u64().unwrap()).collect();
    assert!(seqs.windows(2).all(|w| w[1] == w[0] + 1), "sequence numbers increase by one");
    let done = of_type(&evs, "response.output_item.done");
    assert_eq!(done.len(), 2, "the server tool block is skipped");
    assert_eq!(done[0]["item"]["type"], "reasoning");
    assert_eq!(done[0]["item"]["encrypted_content"], "xolotl.anthropic.SIG");
    assert_eq!(done[0]["item"]["summary"][0]["text"], "hmm");
    assert_eq!(done[1]["item"]["type"], "custom_tool_call");
    assert_eq!(done[1]["item"]["input"], "*** Begin Patch");
    assert_eq!(done[1]["item"]["call_id"], "toolu_1");
    assert!(of_type(&evs, "response.function_call_arguments.delta").is_empty());
    let completed = of_type(&evs, "response.completed");
    assert_eq!(completed.len(), 1);
    let r = &completed[0]["response"];
    assert_eq!(r["status"], "completed");
    assert_eq!(r["output"].as_array().unwrap().len(), 2);
    assert_eq!(r["usage"]["input_tokens"], 105);
    assert_eq!(r["usage"]["input_tokens_details"]["cached_tokens"], 90);
    assert_eq!(r["usage"]["output_tokens"], 30);
}

#[tokio::test]
async fn chat_stream_to_responses_and_messages() {
    let chunks = [
        json!({"choices": [{"index": 0, "delta": {"role": "assistant", "reasoning_content": "think"}}]}),
        json!({"choices": [{"index": 0, "delta": {"content": "Hi"}}]}),
        json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "id": "c0", "function": {"name": "f", "arguments": ""}}]}}]}),
        json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "function": {"arguments": "{}"}}]}}]}),
        json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 1, "function": {"name": "g", "arguments": "{\"x\":2}"}}]}}]}),
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
        json!({"choices": [], "usage": {"prompt_tokens": 5, "completion_tokens": 6}}),
    ];
    let mut stream: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    stream += "data: [DONE]\n\n";

    let (_, t) = translate(Wire::Responses, Wire::Chat, json!({"model": "fast", "input": "hi"}));
    let (_, _, body) = run(t, upstream_response(200, "text/event-stream", stream.clone())).await;
    let v: Value = serde_json::from_str(&body).unwrap();
    let kinds: Vec<&str> = v["output"].as_array().unwrap().iter().map(|o| o["type"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["reasoning", "message", "function_call", "function_call"]);
    assert_eq!(v["output"][3]["call_id"], "call_1", "a missing id is generated");
    assert_eq!(v["output"][3]["arguments"], "{\"x\":2}");
    assert_eq!(v["usage"]["input_tokens"], 5);

    let (_, t) = translate(Wire::Messages, Wire::Chat, json!({"model": "fast", "stream": true, "messages": []}));
    let (_, _, body) = run(t, upstream_response(200, "text/event-stream", stream)).await;
    let evs = events(&body);
    assert_eq!(of_type(&evs, "content_block_start").len(), 4);
    assert_eq!(of_type(&evs, "message_delta")[0]["usage"]["output_tokens"], 6);
}

#[tokio::test]
async fn json_upstream_streams_to_chat_client_with_usage_on_request() {
    let upstream = json!({
        "content": [{"type": "text", "text": "Hi there"}],
        "stop_reason": "max_tokens",
        "usage": {"input_tokens": 3, "output_tokens": 4},
    });
    for include in [false, true] {
        let body = json!({"model": "fast", "stream": true, "stream_options": {"include_usage": include}, "messages": [{"role": "user", "content": "hi"}]});
        let (_, t) = translate(Wire::Chat, Wire::Messages, body);
        let (_, headers, body) = run(t, upstream_response(200, "application/json", upstream.to_string())).await;
        assert_eq!(headers[header::CONTENT_TYPE], "text/event-stream");
        assert!(body.ends_with("data: [DONE]\n\n"));
        let evs = events(&body);
        let text: String = evs.iter().filter_map(|e| e["choices"][0]["delta"]["content"].as_str()).collect();
        assert_eq!(text, "Hi there");
        assert!(evs.iter().any(|e| e["choices"][0]["finish_reason"] == "length"));
        assert_eq!(evs.iter().any(|e| e["usage"]["prompt_tokens"] == 3), include);
    }
}

#[tokio::test]
async fn upstream_errors_take_the_client_envelope() {
    let (_, t) = translate(Wire::Messages, Wire::Responses, json!({"model": "fast", "messages": []}));
    let mut up =
        upstream_response(429, "application/json", json!({"error": {"type": "usage_limit_reached", "message": "slow down"}}).to_string());
    up.headers_mut().insert("retry-after", HeaderValue::from_static("30"));
    let (status, headers, body) = run(t, up).await;
    assert_eq!(status, 429);
    assert_eq!(headers["retry-after"], "30");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v, json!({"type": "error", "error": {"type": "usage_limit_reached", "message": "slow down"}}));

    let (_, t) = translate(Wire::Chat, Wire::Messages, json!({"model": "fast", "messages": []}));
    let up = upstream_response(529, "application/json", messages::error_body("overloaded_error", "busy").to_string());
    let (status, _, body) = run(t, up).await;
    assert_eq!(status, 529);
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap(), json!({"error": {"type": "overloaded_error", "message": "busy"}}));
}

#[tokio::test]
async fn failures_inside_a_stream_are_reported() {
    // A stream cut short ends with an error event, not a silent stop.
    let cut = sse(&[
        json!({"type": "response.output_item.added", "item": {"type": "message"}}),
        json!({"type": "response.output_text.delta", "delta": "par"}),
    ]);
    let (_, t) = translate(Wire::Messages, Wire::Responses, json!({"model": "fast", "stream": true, "messages": []}));
    let (_, _, body) = run(t, upstream_response(200, "text/event-stream", cut.clone())).await;
    let evs = events(&body);
    assert_eq!(evs.last().unwrap()["type"], "error");
    assert_eq!(of_type(&evs, "content_block_stop").len(), 1, "the open block is closed first");

    let (_, t) = translate(Wire::Chat, Wire::Responses, json!({"model": "fast", "messages": []}));
    let (status, _, body) = run(t, upstream_response(200, "text/event-stream", cut)).await;
    assert_eq!(status, 502);
    assert!(body.contains("ended before the response completed"), "{body}");

    let failed = sse(&[json!({"type": "response.failed", "response": {"error": {"code": "server_error", "message": "boom"}}})]);
    let (_, t) = translate(Wire::Chat, Wire::Responses, json!({"model": "fast", "stream": true, "messages": []}));
    let (_, _, body) = run(t, upstream_response(200, "text/event-stream", failed)).await;
    let evs = events(&body);
    assert_eq!(evs.last().unwrap()["error"], json!({"type": "server_error", "message": "boom"}));
}

#[test]
fn short_names_are_stable_and_valid() {
    assert_eq!(short_name("read_file"), "read_file");
    let long = "x".repeat(100);
    let a = short_name(&long);
    assert_eq!(a, short_name(&long));
    assert_eq!(a.len(), 64);
    assert_ne!(a, short_name(&"x".repeat(101)));
    assert!(short_name("mcp.server/tool").chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'));
}

#[tokio::test]
async fn interleaved_parallel_chat_tool_calls_keep_their_identity() {
    let chunks = [
        json!({"choices": [{"index": 0, "delta": {"tool_calls": [
            {"index": 0, "id": "call_a", "type": "function", "function": {"name": "first", "arguments": ""}},
            {"index": 1, "id": "call_b", "type": "function", "function": {"name": "second", "arguments": ""}},
        ]}}]}),
        json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "function": {"arguments": "{\"x\":"}}]}}]}),
        json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 1, "function": {"arguments": "{\"y\":2}"}}]}}]}),
        json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "function": {"arguments": "1}"}}]}}]}),
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
    ];
    let stream: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect::<String>() + "data: [DONE]\n\n";

    let (_, t) = translate(Wire::Responses, Wire::Chat, json!({"model": "fast", "input": "hi"}));
    let (_, _, body) = run(t, upstream_response(200, "text/event-stream", stream.clone())).await;
    let v: Value = serde_json::from_str(&body).unwrap();
    let calls: Vec<(&str, &str, &str)> = v["output"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| (o["call_id"].as_str().unwrap(), o["name"].as_str().unwrap(), o["arguments"].as_str().unwrap()))
        .collect();
    assert_eq!(calls, [("call_a", "first", "{\"x\":1}"), ("call_b", "second", "{\"y\":2}")]);

    let (_, t) = translate(Wire::Messages, Wire::Chat, json!({"model": "fast", "stream": true, "messages": []}));
    let (_, _, body) = run(t, upstream_response(200, "text/event-stream", stream)).await;
    let evs = events(&body);
    let starts: Vec<&Value> = of_type(&evs, "content_block_start").into_iter().map(|e| &e["content_block"]).collect();
    assert_eq!(starts.len(), 2);
    assert_eq!((starts[0]["id"].as_str(), starts[0]["name"].as_str()), (Some("call_a"), Some("first")));
    assert_eq!((starts[1]["id"].as_str(), starts[1]["name"].as_str()), (Some("call_b"), Some("second")));
}

#[tokio::test]
async fn namespaced_tools_round_trip_through_messages() {
    let tools = json!([
        {"type": "namespace", "name": "alpha", "description": "Alpha tools", "tools": [{"type": "function", "name": "lookup", "parameters": {"type": "object"}}]},
        {"type": "namespace", "name": "beta", "tools": [
            {"type": "function", "name": "lookup", "parameters": {"type": "object"}},
            {"type": "custom", "name": "edit"},
        ]},
    ]);
    let body = json!({
        "model": "fast", "tools": tools, "tool_choice": {"type": "function", "namespace": "beta", "name": "lookup"},
        "input": [
            {"type": "message", "role": "user", "content": "hi"},
            {"type": "function_call", "call_id": "c1", "namespace": "alpha", "name": "lookup", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "c1", "output": "found"},
        ],
    });
    let (out, t) = translate(Wire::Responses, Wire::Messages, body);
    let names: Vec<&str> = out["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["alpha__lookup", "beta__lookup", "beta__edit"]);
    assert_eq!(out["tool_choice"], json!({"type": "tool", "name": "beta__lookup"}));
    assert_eq!(out["messages"][1]["content"][0]["name"], "alpha__lookup");

    // Returned calls go back to the client with their namespace, streamed or not.
    let upstream = json!({
        "content": [
            {"type": "tool_use", "id": "t1", "name": "beta__lookup", "input": {}},
            {"type": "tool_use", "id": "t2", "name": "beta__edit", "input": {"input": "patch"}},
        ],
        "stop_reason": "tool_use", "usage": {"input_tokens": 1, "output_tokens": 1},
    });
    let (_, _, body) = run(t.clone(), upstream_response(200, "application/json", upstream.to_string())).await;
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!((v["output"][0]["namespace"].as_str(), v["output"][0]["name"].as_str()), (Some("beta"), Some("lookup")));
    assert_eq!(v["output"][1]["type"], "custom_tool_call");
    assert_eq!((v["output"][1]["namespace"].as_str(), v["output"][1]["name"].as_str()), (Some("beta"), Some("edit")));
    assert_eq!(v["output"][1]["input"], "patch");

    let mut t = t;
    t.stream = true;
    let (_, _, body) = run(t, upstream_response(200, "application/json", upstream.to_string())).await;
    let evs = events(&body);
    let added = of_type(&evs, "response.output_item.added");
    assert_eq!((added[0]["item"]["namespace"].as_str(), added[0]["item"]["name"].as_str()), (Some("beta"), Some("lookup")));
    let done = of_type(&evs, "response.output_item.done");
    assert_eq!(done[1]["item"]["namespace"], "beta");
}

#[tokio::test]
async fn namespace_members_with_unchanged_names_keep_their_namespace() {
    let body = json!({"model": "fast", "stream": true, "input": "hi", "tools": [{"type": "namespace", "name": "sample_tools", "tools": [
        {"type": "function", "name": "mcp__sample__lookup", "parameters": {"type": "object"}},
        {"type": "function", "name": "sample_tools__find", "parameters": {"type": "object"}},
        {"type": "custom", "name": "sample_tools"},
    ]}]});
    let (out, t) = translate(Wire::Responses, Wire::Messages, body);
    let names: Vec<&str> = out["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["mcp__sample__lookup", "sample_tools__find", "sample_tools"]);

    let upstream = json!({
        "content": [
            {"type": "tool_use", "id": "t1", "name": "mcp__sample__lookup", "input": {}},
            {"type": "tool_use", "id": "t2", "name": "sample_tools__find", "input": {}},
            {"type": "tool_use", "id": "t3", "name": "sample_tools", "input": {"input": "x"}},
        ],
        "stop_reason": "tool_use", "usage": {"input_tokens": 1, "output_tokens": 1},
    });
    let want = [("function_call", "mcp__sample__lookup"), ("function_call", "sample_tools__find"), ("custom_tool_call", "sample_tools")];
    let (_, _, body) = run(t.clone(), upstream_response(200, "application/json", upstream.to_string())).await;
    let evs = events(&body);
    let done = of_type(&evs, "response.output_item.done");
    let mut t = t;
    t.stream = false;
    let (_, _, body) = run(t, upstream_response(200, "application/json", upstream.to_string())).await;
    let v: Value = serde_json::from_str(&body).unwrap();
    for (i, (kind, name)) in want.iter().enumerate() {
        for item in [&done[i]["item"], &v["output"][i]] {
            assert_eq!((item["type"].as_str(), item["name"].as_str()), (Some(*kind), Some(*name)), "{item}");
            assert_eq!(item["namespace"], "sample_tools", "{item}");
        }
    }
}

#[tokio::test]
async fn failed_json_responses_become_client_errors() {
    let failed = json!({"status": "failed", "output": [], "error": {"code": "server_error", "message": "provider failed"}}).to_string();
    let (_, t) = translate(Wire::Messages, Wire::Responses, json!({"model": "fast", "messages": []}));
    let (status, _, body) = run(t, upstream_response(200, "application/json", failed.clone())).await;
    assert_eq!(status, 502);
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap(),
        json!({"type": "error", "error": {"type": "server_error", "message": "provider failed"}})
    );

    let (_, t) = translate(Wire::Messages, Wire::Responses, json!({"model": "fast", "stream": true, "messages": []}));
    let (status, _, body) = run(t, upstream_response(200, "application/json", failed)).await;
    assert_eq!(status, 200, "the stream has started");
    let evs = events(&body);
    assert_eq!(evs.last().unwrap(), &json!({"type": "error", "error": {"type": "server_error", "message": "provider failed"}}));
    assert!(of_type(&evs, "message_delta").is_empty(), "no normal completion");

    // `status: failed` alone is a failure too.
    let (_, t) = translate(Wire::Chat, Wire::Responses, json!({"model": "fast", "messages": []}));
    let (status, _, body) = run(t, upstream_response(200, "application/json", json!({"status": "failed", "output": []}).to_string())).await;
    assert_eq!(status, 502);
    assert!(body.contains("upstream response failed"), "{body}");

    // Chat and Messages error bodies with a 2xx status fail the same way.
    let (_, t) = translate(Wire::Responses, Wire::Chat, json!({"model": "fast", "input": "hi"}));
    let (status, _, body) =
        run(t, upstream_response(200, "application/json", json!({"error": {"type": "x", "message": "m"}}).to_string())).await;
    assert_eq!((status, body.contains("\"m\"")), (502, true));
    let (_, t) = translate(Wire::Chat, Wire::Messages, json!({"model": "fast", "messages": []}));
    let (status, _, _) =
        run(t, upstream_response(200, "application/json", messages::error_body("overloaded_error", "busy").to_string())).await;
    assert_eq!(status, 502);
}
