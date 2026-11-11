//! The NATS JetStream destination: each row is published as one message and
//! counts as sent once the stream has stored it. ClickHouse loads the stream
//! with its NATS table engine, so rows published while ClickHouse is down wait
//! in the stream. While NATS itself is unreachable, rows wait in the queue,
//! and the head row is retried until NATS takes it: rows are dropped only when
//! the queue is full.
// ---
// relationships:
//   implements: ../../docs/design/export.md
// ---

use std::time::Duration;

use anyhow::{Context, Result};
use async_nats::jetstream::{self, ErrorCode, context::PublishErrorKind, message::PublishMessage};
use async_nats::{ConnectOptions, Event, ServerAddr};
use bytes::Bytes;
use serde_json::{Map, Value};

use super::{Exchange, Queue, Sink, count};
use crate::config::NatsConfig;

const SINK: &str = "nats";
/// Rows are published to `<SUBJECT_PREFIX>.<client_id>`.
pub const SUBJECT_PREFIX: &str = "xolotl.exchanges";
/// What the stream's subjects must cover.
pub const SUBJECTS: &str = "xolotl.exchanges.>";
/// Room kept in each message for its headers (`Nats-Msg-Id`).
const HEADER_ROOM: usize = 128;
/// Wait before the first retry of a publish; it doubles up to `RETRY_MAX`.
const RETRY_MIN: Duration = Duration::from_millis(500);
const RETRY_MAX: Duration = Duration::from_secs(10);

/// Connect to NATS, check the stream, and start publishing. `secret` is the
/// vault secret `nats`: `user` and `password`, or `creds`. Fails when NATS
/// can't be reached, or the stream is missing or doesn't take
/// `xolotl.exchanges.>`.
pub async fn start(cfg: NatsConfig, secret: Option<Map<String, Value>>) -> Result<Sink> {
    let publisher = connect(&cfg, secret).await?;
    Ok(Sink::spawn(SINK, cfg.bodies, cfg.max_body_bytes, cfg.queue_mb, |q| publisher.run(q)))
}

async fn connect(cfg: &NatsConfig, secret: Option<Map<String, Value>>) -> Result<Publisher> {
    let servers: Vec<ServerAddr> = cfg
        .url
        .split(',')
        .map(|s| s.trim().parse::<ServerAddr>().with_context(|| format!("export.nats.url: invalid server {s:?}")))
        .collect::<Result<_>>()?;
    let mut opts = ConnectOptions::new()
        .name("xolotl")
        // Keep reconnecting for as long as xolotl runs: rows wait in the queue meanwhile.
        .max_reconnects(None)
        .event_callback(|event| async move {
            match event {
                Event::Connected => tracing::info!("NATS export connected"),
                Event::Disconnected => tracing::warn!("NATS export disconnected; rows wait in the queue"),
                Event::SlowConsumer(_) => {}
                other => tracing::warn!(event = %other, "NATS export"),
            }
        });
    let field = |k: &str| secret.as_ref().and_then(|s| s.get(k)).and_then(Value::as_str).filter(|v| !v.is_empty());
    if let Some(creds) = field("creds") {
        opts = opts.credentials(creds).context("parsing the creds of the vault secret `nats`")?;
    } else if let Some(user) = field("user") {
        opts = opts.user_and_password(user.to_string(), field("password").unwrap_or_default().to_string());
    }
    if let Some(ca) = &cfg.ca_cert {
        // Read here so that a missing file is reported as such, not as a failed connection.
        std::fs::metadata(ca).with_context(|| format!("reading NATS CA {}", ca.display()))?;
        opts = opts.add_root_certificates(ca.clone());
    }
    if cfg.url.split(',').any(|s| s.trim().starts_with("tls://")) {
        opts = opts.require_tls(true);
    }
    let client = opts.connect(servers).await.with_context(|| format!("connecting to NATS at {}", cfg.url))?;
    let js = jetstream::new(client.clone());
    let stream = js.get_stream(&cfg.stream).await.with_context(|| {
        format!("NATS stream {:?} not found: create it with subjects '{SUBJECTS}' (see the NATS export docs)", cfg.stream)
    })?;
    let info = stream.cached_info();
    anyhow::ensure!(
        info.config.subjects.iter().any(|p| covers(p, SUBJECTS)),
        "NATS stream {:?} takes subjects {:?}, which don't cover '{SUBJECTS}'",
        cfg.stream,
        info.config.subjects
    );
    if info.config.duplicate_window.is_zero() {
        tracing::warn!(stream = %cfg.stream, "NATS stream has no duplicate window; a retried publish can store a row twice");
    }
    let stream_max = usize::try_from(info.config.max_message_size).ok().filter(|&m| m > 0);
    tracing::info!(
        url = %cfg.url,
        stream = %cfg.stream,
        max_payload = client.max_payload(),
        stream_max_message = ?stream_max,
        bodies = cfg.bodies,
        "NATS export enabled"
    );
    Ok(Publisher { client, js, stream_max })
}

/// Whether the stream subject `pattern` takes every subject that `subject` matches.
fn covers(pattern: &str, subject: &str) -> bool {
    let p: Vec<&str> = pattern.split('.').collect();
    let s: Vec<&str> = subject.split('.').collect();
    for (i, token) in p.iter().enumerate() {
        if *token == ">" {
            return s.len() > i;
        }
        match s.get(i) {
            None | Some(&">") => return false,
            Some(t) if *token != "*" && token != t => return false,
            _ => {}
        }
    }
    p.len() == s.len()
}

/// The subject of a client's rows. A client id is a policy and key name,
/// which never hold the characters NATS reserves; they are replaced in case.
pub fn subject(client_id: &str) -> String {
    let token: String =
        client_id.chars().map(|c| if c == '.' || c == '*' || c == '>' || c.is_whitespace() || c.is_control() { '_' } else { c }).collect();
    let token = if token.is_empty() { "_".to_string() } else { token };
    format!("{SUBJECT_PREFIX}.{token}")
}

/// Bytes `s` takes as a JSON string, without its quotes, as `serde_json` writes it.
fn escaped_len(s: &str) -> usize {
    s.bytes()
        .map(|b| match b {
            b'"' | b'\\' | b'\n' | b'\r' | b'\t' | 0x08 | 0x0c => 2,
            0..=0x1f => 6,
            _ => 1,
        })
        .sum()
}

/// The longest prefix of `s` that takes at most `room` bytes as a JSON string.
fn escaped_prefix(s: &str, room: usize) -> &str {
    let mut used = 0;
    for (i, c) in s.char_indices() {
        let mut buf = [0; 4];
        used += escaped_len(c.encode_utf8(&mut buf));
        if used > room {
            return &s[..i];
        }
    }
    s
}

/// A row as one message of at most `limit` bytes. A row that doesn't fit has
/// its response cut, then its request, and is marked as cut. `Err` holds the
/// size of a row that doesn't fit even without them.
fn encode(row: &Exchange, limit: usize) -> Result<(Bytes, bool), usize> {
    let whole = serde_json::to_vec(row).map_err(|_| 0usize)?;
    if whole.len() <= limit {
        return Ok((Bytes::from(whole), false));
    }
    let request = escaped_len(&row.request);
    let response = escaped_len(&row.response);
    // The rest of the row, with both bodies empty.
    let fixed = whole.len() - request - response;
    let room = limit.checked_sub(fixed).ok_or(whole.len())?;
    drop(whole);
    // The response is cut first: `output` keeps what it said.
    let request_room = room.min(request);
    let response_room = room - request_room;
    let cut = |s: &str, room: usize| -> (String, bool) {
        let kept = escaped_prefix(s, room);
        (kept.to_string(), kept.len() < s.len())
    };
    let (req, req_cut) = cut(&row.request, request_room);
    let (resp, resp_cut) = cut(&row.response, response_room);
    let fitted = Exchange {
        request: req,
        request_truncated: row.request_truncated || req_cut,
        response: resp,
        response_truncated: row.response_truncated || resp_cut,
        ..row.clone()
    };
    let body = serde_json::to_vec(&fitted).map_err(|_| 0usize)?;
    if body.len() > limit {
        return Err(body.len());
    }
    Ok((Bytes::from(body), true))
}

/// Whether the stream will never take this message, however often it is sent.
fn too_large(e: &jetstream::context::PublishError) -> bool {
    if e.kind() == PublishErrorKind::MaxPayloadExceeded {
        return true;
    }
    std::error::Error::source(e)
        .and_then(|s| s.downcast_ref::<jetstream::Error>())
        .is_some_and(|e| matches!(e.error_code(), ErrorCode::STREAM_MESSAGE_EXCEEDS_MAXIMUM | ErrorCode::STREAM_HEADER_EXCEEDS_MAXIMUM))
}

struct Publisher {
    client: async_nats::Client,
    js: jetstream::Context,
    /// The stream's own limit on a message, when it has one.
    stream_max: Option<usize>,
}

impl Publisher {
    async fn run(self, mut q: Queue) {
        let mut stopping = false;
        loop {
            let row = if stopping {
                match q.rx.try_recv() {
                    Ok(row) => row,
                    Err(_) => break,
                }
            } else {
                tokio::select! {
                    row = q.rx.recv() => match row {
                        Some(row) => row,
                        None => break,
                    },
                    _ = &mut q.stop => {
                        // Publish what is queued, until shutdown gives up.
                        stopping = true;
                        q.rx.close();
                        continue;
                    }
                }
            };
            self.send(&row).await;
            q.done(&row);
        }
        let _ = self.client.flush().await;
    }

    /// The largest message the server and the stream take.
    fn limit(&self) -> usize {
        let max = self.client.max_payload();
        self.stream_max.map_or(max, |s| s.min(max)).saturating_sub(HEADER_ROOM)
    }

    /// Publish `row` and wait for the stream to store it, retrying until it
    /// does. A row too large for the stream is refused and counted.
    async fn send(&self, row: &Exchange) {
        let subject = subject(&row.client_id);
        let mut delay = RETRY_MIN;
        let mut failures = 0u32;
        let mut encoded: Option<(usize, Bytes)> = None;
        loop {
            // The server's limit can change when the client reconnects to another server.
            let limit = self.limit();
            let payload = match &encoded {
                Some((l, payload)) if *l == limit => payload.clone(),
                _ => match encode(row, limit) {
                    Ok((payload, cut)) => {
                        if cut {
                            tracing::debug!(id = %row.id, limit, "NATS export: row bodies cut to fit the message size");
                        }
                        encoded = Some((limit, payload.clone()));
                        payload
                    }
                    Err(size) => {
                        count(SINK, "refused", 1);
                        tracing::warn!(id = %row.id, size, limit, "NATS export: row too large even without its bodies; refused");
                        return;
                    }
                },
            };
            let publish = PublishMessage::build().payload(payload).message_id(&row.id);
            let result = match self.js.send_publish(subject.clone(), publish).await {
                Ok(ack) => ack.await,
                Err(e) => Err(e),
            };
            match result {
                Ok(ack) => {
                    count(SINK, "published", 1);
                    if failures > 0 {
                        tracing::info!(failures, duplicate = ack.duplicate, "NATS export: publishing again");
                    }
                    return;
                }
                // The server's limit fell since the row was encoded: encode it again.
                Err(e) if e.kind() == PublishErrorKind::MaxPayloadExceeded && self.limit() != limit => encoded = None,
                Err(e) if too_large(&e) => {
                    count(SINK, "refused", 1);
                    tracing::warn!(id = %row.id, error = %e, "NATS export: the row is too large for the stream; refused");
                    return;
                }
                Err(e) => {
                    failures += 1;
                    if failures == 1 {
                        tracing::warn!(error = %e, "NATS export publish failed; retrying while rows wait in the queue");
                    } else {
                        tracing::debug!(error = %e, failures, "NATS export publish failed again");
                    }
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(RETRY_MAX);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
