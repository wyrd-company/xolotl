//! Server-sent event framing.

use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

impl SseEvent {
    pub fn json(&self) -> Option<Value> {
        serde_json::from_str(&self.data).ok()
    }
}

/// Splits a byte stream into events; chunk boundaries may fall anywhere.
#[derive(Default)]
pub struct SseParser {
    buf: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
}

impl SseParser {
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        // The buffer holds only the start of an unfinished line, so the search
        // for its end starts at the new bytes, and the lines found are removed
        // once: a long line arriving in many chunks is scanned once.
        let mut buf = std::mem::take(&mut self.buf);
        let mut search = buf.len();
        buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        let mut start = 0;
        while let Some(off) = buf[search..].iter().position(|b| *b == b'\n') {
            let end = search + off;
            self.line(&buf[start..=end], &mut out);
            start = end + 1;
            search = start;
        }
        buf.drain(..start);
        self.buf = buf;
        out
    }

    /// End of input: a trailing event without its blank line still counts.
    pub fn end(&mut self) -> Vec<SseEvent> {
        let mut out = Vec::new();
        let rest = std::mem::take(&mut self.buf);
        if !rest.is_empty() {
            self.line(&rest, &mut out);
        }
        self.dispatch(&mut out);
        out
    }

    fn line(&mut self, line: &[u8], out: &mut Vec<SseEvent>) {
        let line = String::from_utf8_lossy(line);
        let line = line.trim_end_matches(['\n', '\r']);
        if line.is_empty() {
            self.dispatch(out);
            return;
        }
        if line.starts_with(':') {
            return;
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => self.event = Some(value.to_string()),
            "data" => self.data.push(value.to_string()),
            _ => {}
        }
    }

    fn dispatch(&mut self, out: &mut Vec<SseEvent>) {
        let event = self.event.take();
        if !self.data.is_empty() {
            out.push(SseEvent { event, data: std::mem::take(&mut self.data).join("\n") });
        }
    }
}

/// One event in wire form.
pub fn frame(event: Option<&str>, data: &Value) -> String {
    match event {
        Some(e) => format!("event: {e}\ndata: {data}\n\n"),
        None => format!("data: {data}\n\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_events_across_chunks_and_crlf() {
        let mut p = SseParser::default();
        let mut got = p.feed(b"event: a\r\ndata: {\"x\"");
        assert!(got.is_empty());
        got.extend(p.feed(b":1}\r\n\r\n: comment\ndata: one\ndata: two\n\ndata: [DONE]"));
        got.extend(p.end());
        assert_eq!(
            got,
            vec![
                SseEvent { event: Some("a".into()), data: "{\"x\":1}".into() },
                SseEvent { event: None, data: "one\ntwo".into() },
                SseEvent { event: None, data: "[DONE]".into() },
            ]
        );
        assert_eq!(got[0].json().unwrap()["x"], 1);
    }

    #[test]
    fn a_long_line_in_small_chunks_is_scanned_once() {
        // 8 MiB of data in 1 KiB chunks: rescanning the unfinished line on every
        // chunk would read about 32 GiB and not finish in a test run.
        let line = format!("data: {}\n\n", "x".repeat(8 << 20));
        let mut p = SseParser::default();
        let mut got = Vec::new();
        for chunk in line.as_bytes().chunks(1024) {
            got.extend(p.feed(chunk));
        }
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].data.len(), 8 << 20);
        assert!(p.end().is_empty());
    }

    #[test]
    fn every_line_of_one_chunk_is_read_in_order() {
        let body: String = (0..1000).map(|i| format!("data: {i}\n\n")).collect();
        let mut p = SseParser::default();
        let got: Vec<String> = p.feed(body.as_bytes()).into_iter().map(|e| e.data).collect();
        assert_eq!(got, (0..1000).map(|i| i.to_string()).collect::<Vec<_>>());
    }
}
