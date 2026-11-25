//! Assemble Code Assist replies for exchange export.

use serde_json::Value;

use super::ir::{Block, Reasoning, Response, StopReason, ToolCall};
use super::sse::SseParser;

type Error = (String, String);

pub(crate) fn assemble(body: &[u8], sse: bool) -> (Response, Option<Error>) {
    let mut out = Reply::default();
    let result = if sse {
        let mut parser = SseParser::default();
        parser
            .feed(body)
            .into_iter()
            .chain(parser.end())
            .filter(|event| event.data != "[DONE]")
            .try_for_each(|event| out.feed(event.data.as_bytes()))
            .and_then(|()| {
                if out.finish.is_some() {
                    Ok(())
                } else {
                    Err(("incomplete".into(), "Code Assist stream ended without a finish reason".into()))
                }
            })
    } else {
        out.feed(body)
    };
    let stop = match out.finish.as_deref() {
        Some("STOP") if out.response.has_tool_calls() => StopReason::ToolUse,
        Some("MAX_TOKENS") => StopReason::MaxTokens,
        Some("SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" | "IMAGE_SAFETY") => StopReason::ContentFilter,
        _ => StopReason::End,
    };
    out.response.stop = stop;
    (out.response, result.err())
}

#[derive(Default)]
struct Reply {
    response: Response,
    finish: Option<String>,
}

impl Reply {
    fn feed(&mut self, data: &[u8]) -> Result<(), Error> {
        let value: Value = serde_json::from_slice(data).map_err(|e| ("api_error".into(), format!("invalid Code Assist JSON: {e}")))?;
        let response = value.get("response").unwrap_or(&value);
        if let Some(error) = value.get("error").or_else(|| response.get("error")).filter(|e| !e.is_null()) {
            let kind = error.get("status").and_then(Value::as_str).unwrap_or("api_error");
            let message =
                error.get("message").and_then(Value::as_str).or_else(|| error.as_str()).unwrap_or("Code Assist returned an error");
            return Err((kind.to_owned(), message.to_owned()));
        }
        if response.get("candidates").is_none() && response.get("usageMetadata").is_none() && response.get("promptFeedback").is_none() {
            return Err(("api_error".into(), "missing Code Assist response fields".into()));
        }
        let candidate = &response["candidates"][0];
        if let Some(parts) = candidate.pointer("/content/parts").and_then(Value::as_array) {
            for part in parts {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    self.text(text, part.get("thought").and_then(Value::as_bool) == Some(true));
                }
                if let Some(call) = part.get("functionCall")
                    && let Some(name) = call.get("name").and_then(Value::as_str)
                {
                    self.response.blocks.push(Block::ToolCall(ToolCall {
                        id: call.get("id").and_then(Value::as_str).unwrap_or_default().to_owned(),
                        name: name.to_owned(),
                        arguments: call.get("args").map_or_else(|| "{}".to_owned(), Value::to_string),
                    }));
                }
            }
        }
        if let Some(reason) = candidate.get("finishReason").and_then(Value::as_str) {
            self.finish = Some(reason.to_owned());
        }
        Ok(())
    }

    fn text(&mut self, text: &str, thought: bool) {
        if text.is_empty() {
            return;
        }
        let blocks = &mut self.response.blocks;
        match (blocks.last_mut(), thought) {
            (Some(Block::Text(open)), false) => open.push_str(text),
            (Some(Block::Reasoning(open)), true) => open.text.push_str(text),
            (_, true) => blocks.push(Block::Reasoning(Reasoning { text: text.to_owned(), opaque: None })),
            (_, false) => blocks.push(Block::Text(text.to_owned())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assembles_json_with_reasoning_text_and_tool_call() {
        let (response, error) = assemble(
            br#"{"response":{"candidates":[{"content":{"parts":[
                {"text":"private reasoning","thought":true,"thoughtSignature":"opaque"},
                {"text":"answer"},
                {"functionCall":{"name":"lookup","id":"call","args":{"key":1}}}
            ]},"finishReason":"STOP"}]}}"#,
            false,
        );
        assert_eq!(error, None);
        assert_eq!(response.stop, StopReason::ToolUse);
        assert_eq!(
            response.blocks,
            [
                Block::Reasoning(Reasoning { text: "private reasoning".into(), opaque: None }),
                Block::Text("answer".into()),
                Block::ToolCall(ToolCall { id: "call".into(), name: "lookup".into(), arguments: r#"{"key":1}"#.into() }),
            ]
        );
    }

    #[test]
    fn assembles_partial_text_and_preserves_the_upstream_error() {
        let (reply, error) = assemble(
            b"data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hi\"}]}}]}}\n\ndata: {\"error\":{\"status\":\"UNAVAILABLE\",\"message\":\"try later\"}}",
            true,
        );
        assert_eq!(error, Some(("UNAVAILABLE".into(), "try later".into())));
        assert_eq!(reply.blocks, [Block::Text("hi".into())]);
    }
}
