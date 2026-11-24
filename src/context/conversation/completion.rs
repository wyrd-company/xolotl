//! A response body that moves its conversation only once the answer is
//! complete: a stream must end with the format's final event and no error
//! event. The end of the answer is held back until the move is written, so
//! the client can't see the answer complete before its next request would
//! see the move.
// ---
// relationships:
//   implements: ../../../docs/design/sessions.md
// ---
use std::collections::VecDeque;

use axum::body::{Body, Bytes};
use axum::http::header;
use axum::response::Response;
use futures::{Stream, StreamExt};
use serde_json::Value;

use super::Conversation;
use crate::policy::Target;

/// The longest line kept while looking for the end of a stream.
const MAX_LINE: usize = 4 * 1024 * 1024;

/// How a response body carries its answer.
#[derive(Debug, Default, PartialEq)]
enum Shape {
    /// Not known until the first byte that isn't whitespace.
    #[default]
    Unknown,
    /// Server-sent events.
    Sse,
    /// One JSON document.
    Json,
}

/// What a response body says about its answer.
#[derive(Debug, Default)]
pub(super) struct Outcome {
    shape: Shape,
    /// The SSE line being read, or the JSON body so far (up to `MAX_LINE`).
    line: Vec<u8>,
    /// The JSON body outgrew `MAX_LINE`, so it is taken as an answer.
    overflowed: bool,
    /// The final event has started: `message_stop`, `response.completed` or
    /// `response.incomplete`, a choice's `finish_reason`, or `[DONE]`.
    ended: bool,
    /// An error event: Messages and Responses `error`, a Chat `error`. A
    /// failed Responses stream ends with `response.failed` and no
    /// `response.completed`, so it never ends well.
    failed: bool,
}

impl Outcome {
    /// `sse`: the `Content-Type` says the body is a stream. Otherwise the
    /// first byte tells: a JSON body starts with `{` or `[`, and anything
    /// else is read as a stream, as a server may stream without saying so.
    pub(super) fn new(sse: bool) -> Self {
        Self { shape: if sse { Shape::Sse } else { Shape::Unknown }, ..Default::default() }
    }

    /// Whether the body is read as a stream.
    pub(super) fn streams(&self) -> bool {
        self.shape == Shape::Sse
    }

    /// Read a chunk of the body. True once the end of a stream's answer has
    /// been seen, in this chunk or before: from there on the body is held.
    pub(super) fn feed(&mut self, chunk: &[u8]) -> bool {
        if self.shape == Shape::Unknown
            && let Some(first) = chunk.iter().find(|b| !b.is_ascii_whitespace())
        {
            self.shape = if matches!(first, b'{' | b'[') { Shape::Json } else { Shape::Sse };
        }
        if self.shape != Shape::Sse {
            if self.line.len() + chunk.len() <= MAX_LINE {
                self.line.extend_from_slice(chunk);
            } else {
                self.overflowed = true;
            }
            return false;
        }
        self.line.extend_from_slice(chunk);
        while let Some(pos) = self.line.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.line.drain(..=pos).collect();
            self.observe(line.trim_ascii());
        }
        if self.line.len() > MAX_LINE {
            self.line.clear();
        }
        self.ended
    }

    fn observe(&mut self, line: &[u8]) {
        if let Some(event) = line.strip_prefix(b"event:") {
            match event.trim_ascii() {
                b"error" => self.failed = true,
                b"message_stop" => self.ended = true,
                _ => {}
            }
            return;
        }
        let Some(data) = line.strip_prefix(b"data:").map(<[u8]>::trim_ascii) else { return };
        if data == b"[DONE]" {
            self.ended = true;
            return;
        }
        let Ok(v) = serde_json::from_slice::<Value>(data) else { return };
        self.observe_json(&v);
    }

    fn observe_json(&mut self, v: &Value) {
        match v.get("type").and_then(Value::as_str) {
            Some("message_stop" | "response.completed" | "response.incomplete") => self.ended = true,
            Some("error") => self.failed = true,
            _ => {}
        }
        if v.get("error").is_some_and(|e| !e.is_null()) {
            self.failed = true;
        }
        let choices = v.get("choices").and_then(Value::as_array).into_iter().flatten();
        if choices.filter_map(|c| c.get("finish_reason")).any(|r| !r.is_null()) {
            self.ended = true;
        }
    }

    /// Whether the answer completed: a stream that reached its final event
    /// with no error event, or a JSON body that is not an error.
    pub(super) fn succeeded(&mut self) -> bool {
        let rest = std::mem::take(&mut self.line);
        match self.shape {
            Shape::Sse => {
                self.observe(rest.trim_ascii());
                self.ended && !self.failed
            }
            Shape::Json if !self.overflowed => {
                let Ok(v) = serde_json::from_slice::<Value>(&rest) else { return false };
                self.observe_json(&v);
                !self.failed
            }
            Shape::Json => true,
            Shape::Unknown => false,
        }
    }
}

impl Conversation {
    /// Move the conversation to `target` once `resp` has answered: a 2xx
    /// response whose body completes ([`Outcome`]). A failed stream, or a
    /// client that leaves before the end, moves nothing. `None` for a target
    /// the conversation can't keep ([`Conversation::served`]).
    pub fn moves_with(self, target: Option<Target>, resp: Response) -> Response {
        if !resp.status().is_success() {
            return resp;
        }
        let sse =
            resp.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|ct| ct.starts_with("text/event-stream"));
        let (parts, body) = resp.into_parts();
        let body = completion(body.into_data_stream(), Outcome::new(sse), self, target);
        Response::from_parts(parts, Body::from_stream(body))
    }
}

struct Relay<S> {
    inner: S,
    outcome: Outcome,
    /// The end of the answer, from its final event (or, for a body that is
    /// not a stream, its last chunk), held until the move is written.
    held: Vec<Bytes>,
    ready: VecDeque<Result<Bytes, axum::Error>>,
    moves: Option<(Conversation, Option<Target>)>,
    done: bool,
}

fn completion<S>(
    inner: S,
    outcome: Outcome,
    conversation: Conversation,
    target: Option<Target>,
) -> impl Stream<Item = Result<Bytes, axum::Error>>
where
    S: Stream<Item = Result<Bytes, axum::Error>> + Unpin + Send + 'static,
{
    let relay = Relay { inner, outcome, held: Vec::new(), ready: VecDeque::new(), moves: Some((conversation, target)), done: false };
    futures::stream::unfold(relay, |mut r| async move {
        loop {
            if let Some(item) = r.ready.pop_front() {
                return Some((item, r));
            }
            if r.done {
                return None;
            }
            match r.inner.next().await {
                Some(Ok(chunk)) => {
                    if r.outcome.feed(&chunk) {
                        r.held.push(chunk);
                    } else if r.outcome.streams() {
                        r.ready.push_back(Ok(chunk));
                    } else {
                        // Not a stream: only the last chunk is held, as any chunk may be the last.
                        r.ready.extend(r.held.drain(..).map(Ok));
                        r.held.push(chunk);
                    }
                }
                Some(Err(e)) => {
                    r.done = true;
                    r.ready.extend(r.held.drain(..).map(Ok));
                    r.ready.push_back(Err(e));
                }
                None => {
                    r.done = true;
                    if r.outcome.succeeded()
                        && let Some((conversation, target)) = r.moves.take()
                    {
                        conversation.served(target.as_ref()).await;
                    }
                    r.ready.extend(r.held.drain(..).map(Ok));
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(sse: bool, body: &str) -> bool {
        let mut o = Outcome::new(sse);
        // Fed a byte at a time, so that no line arrives whole.
        for b in body.as_bytes().chunks(1) {
            o.feed(b);
        }
        o.succeeded()
    }

    #[test]
    fn a_stream_succeeds_at_its_final_event_with_no_error() {
        let ok = [
            "event: message_start\ndata: {\"type\":\"message_start\"}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
            "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{}}\n\n",
            "data: {\"type\":\"response.incomplete\",\"response\":{}}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"an error\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
            "data: [DONE]",
        ];
        for body in ok {
            assert!(outcome(true, body), "{body}");
        }
        let failed = [
            "event: message_start\ndata: {\"type\":\"message_start\"}\n\n",
            "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\"}}\n\n",
            "data: {\"type\":\"response.created\"}\n\ndata: {\"type\":\"response.failed\",\"response\":{}}\n\n",
            "data: {\"type\":\"error\",\"message\":\"boom\"}\n\n",
            "data: {\"error\":{\"message\":\"boom\",\"type\":\"server_error\"}}\n\ndata: [DONE]\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\n",
            "",
        ];
        for body in failed {
            assert!(!outcome(true, body), "{body}");
        }
        assert!(outcome(false, "{\"ok\":true}"), "a JSON body");
        assert!(!outcome(false, "{\"error\":{\"message\":\"boom\"}}"), "a JSON error");
        assert!(!outcome(false, "{\"type\":\"error\",\"error\":{}}"));
        assert!(!outcome(false, ""), "no body");
        assert!(!outcome(false, "data: {\"error\":{\"message\":\"boom\"}}\n\n"), "a stream that doesn't say so");
        assert!(outcome(false, "\n data: [DONE]\n\n"));
    }

    #[tokio::test]
    async fn the_client_gets_the_final_event_only_once_the_move_is_written() {
        use axum::http::HeaderMap;
        use serde_json::json;

        let db = crate::db::Db::in_memory().await.unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("session_id", "s".parse().unwrap());
        let body = json!({"messages": [{"role": "user", "content": "hello"}, {"role": "assistant", "content": "a"}]});
        let conversation = || Conversation::of(&db, "p/k", None, &headers, &body).unwrap();
        let target = Target { provider: "llama".into(), model: "busy".into() };
        let chunks = [
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        ];
        let upstream = futures::stream::iter(chunks.map(|c| Ok(Bytes::from(c))));
        let mut relayed = Box::pin(completion(upstream, Outcome::new(true), conversation(), Some(target.clone())));
        assert_eq!(relayed.next().await.unwrap().unwrap(), chunks[0]);
        assert!(conversation().kept().await.is_empty());
        assert_eq!(relayed.next().await.unwrap().unwrap(), chunks[1], "the final event");
        assert_eq!(conversation().kept().await, vec![target], "is released after the move");
        assert_eq!(relayed.next().await.unwrap().unwrap(), chunks[2]);
        assert!(relayed.next().await.is_none());
    }

    #[test]
    fn the_end_of_a_stream_is_held_from_its_final_event() {
        let mut o = Outcome::new(true);
        assert!(!o.feed(b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n"));
        assert!(!o.feed(b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"st"));
        assert!(o.feed(b"op\"}]}\n\n"), "the chunk that completes the final event");
        assert!(o.feed(b"data: [DONE]\n\n"));
    }
}
