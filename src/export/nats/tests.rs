use std::sync::Arc;

use async_nats::jetstream::stream;
use serde_json::json;

use super::*;
use crate::export::{Captured, Exporter};

fn row(id: &str) -> Exchange {
    Exchange {
        id: id.to_string(),
        started_at: "2026-11-07 10:00:00.250".into(),
        finished_at: "2026-11-07 10:00:01.750".into(),
        client_id: "team/laptop".into(),
        session_key: "abc".into(),
        provider: "claude".into(),
        status: 200,
        request: json!({"messages": [{"role": "user", "content": "héllo"}]}).to_string(),
        response: "data: {\"x\":1}\n\n".into(),
        output_text: "ok".into(),
        ..Default::default()
    }
}

#[test]
fn stream_subjects_must_cover_the_rows() {
    for pattern in ["xolotl.exchanges.>", "xolotl.>", ">", "xolotl.*.>", "*.exchanges.>"] {
        assert!(covers(pattern, SUBJECTS), "{pattern}");
    }
    for pattern in ["xolotl.exchanges.*", "xolotl.exchanges", "xolotl.other.>", "xolotl.exchanges.team.>", "xolotl.*", "other.>"] {
        assert!(!covers(pattern, SUBJECTS), "{pattern}");
    }
}

#[test]
fn subjects_are_per_client() {
    assert_eq!(subject("team/laptop"), "xolotl.exchanges.team/laptop");
    assert_eq!(subject(""), "xolotl.exchanges._");
    assert_eq!(subject("a.b *>"), "xolotl.exchanges.a_b___");
}

#[test]
fn escaped_len_matches_serde_json() {
    let s = "plain \"quoted\" back\\slash\nnew\ttab\u{1}ctl é 漢字 🦎";
    assert_eq!(escaped_len(s), serde_json::to_string(s).unwrap().len() - 2);
    for room in 0..escaped_len(s) {
        let p = escaped_prefix(s, room);
        assert!(serde_json::to_string(p).unwrap().len() - 2 <= room, "{room}");
    }
}

#[test]
fn a_row_that_fits_is_sent_whole() {
    let r = row("1");
    let (payload, cut) = encode(&r, 1 << 20).unwrap();
    assert!(!cut);
    assert_eq!(serde_json::from_slice::<Value>(&payload).unwrap(), serde_json::to_value(&r).unwrap());
}

#[test]
fn a_large_row_loses_its_response_first() {
    let mut r = row("1");
    r.request = format!("{{\"text\":\"{}\"}}", "é".repeat(3000));
    r.response = "\"quoted\"\n".repeat(1000);
    let fixed = serde_json::to_vec(&Exchange { request: String::new(), response: String::new(), ..r.clone() }).unwrap().len();
    // Room for the whole request and some of the response.
    let limit = fixed + escaped_len(&r.request) + 500;
    let (payload, cut) = encode(&r, limit).unwrap();
    assert!(cut && payload.len() <= limit, "{}", payload.len());
    let sent: Exchange = serde_json::from_slice(&payload).unwrap();
    assert_eq!(sent.request, r.request);
    assert!(!sent.request_truncated && sent.response_truncated);
    assert!(r.response.starts_with(&sent.response) && !sent.response.is_empty());
    // Room for only part of the request: the response goes entirely.
    let limit = fixed + 1001;
    let (payload, _) = encode(&r, limit).unwrap();
    let sent: Exchange = serde_json::from_slice(&payload).unwrap();
    assert!(payload.len() <= limit);
    assert!(sent.request_truncated && sent.response_truncated && sent.response.is_empty());
    assert!(r.request.starts_with(&sent.request) && !sent.request.is_empty());
    // No room for the rest of the row.
    assert_eq!(encode(&r, fixed - 1).unwrap_err(), serde_json::to_vec(&r).unwrap().len());
}

/// A destination that hands its rows to the test.
fn collecting(bodies: bool, max: usize) -> (Sink, tokio::sync::mpsc::UnboundedReceiver<Exchange>) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let sink = Sink::spawn("test", bodies, max, 64, move |mut q: Queue| async move {
        while let Some(row) = q.rx.recv().await {
            q.done(&row);
            let _ = tx.send(row.as_ref().clone());
        }
    });
    (sink, rx)
}

#[tokio::test]
async fn each_destination_gets_the_row_it_would_get_alone() {
    let record = crate::db::RequestRecord { endpoint: "/v1/chat/completions".into(), status: 200, ..Default::default() };
    let answer = "a".repeat(2000);
    let body = json!({"choices": [{"message": {"role": "assistant", "content": answer}, "finish_reason": "stop"}]}).to_string();
    let request = Bytes::from(format!("{{\"pad\":\"{}\"}}", "p".repeat(3000)));
    let submit = |exporter: &Arc<Exporter>| {
        let capture = exporter.capture("/v1/chat/completions", Some(&request));
        let mut response = Captured::default();
        if let Some(limit) = capture.response_limit() {
            response.push(body.as_bytes(), limit);
        }
        capture.submit(crate::export::Finished {
            record: &record,
            started_at: chrono::Utc::now(),
            session_key: "abc",
            session_source: "test",
            account_kind: "codex",
            upstream_model: "m",
            usage: Default::default(),
            tools: &[],
            response: capture.response_limit().map(|_| response),
        });
    };
    let alone = |bodies, max| async move {
        let (sink, mut rx) = collecting(bodies, max);
        submit(&Exporter::new(vec![sink]));
        rx.recv().await.unwrap()
    };
    let (small, mut small_rx) = collecting(true, 256);
    let (large, mut large_rx) = collecting(true, 1 << 20);
    let (bare, mut bare_rx) = collecting(false, 1 << 20);
    let (same, mut same_rx) = collecting(true, 1 << 20);
    submit(&Exporter::new(vec![small, large, bare, same]));
    let together =
        [small_rx.recv().await.unwrap(), large_rx.recv().await.unwrap(), bare_rx.recv().await.unwrap(), same_rx.recv().await.unwrap()];
    assert!(together.iter().all(|r| r.id == together[0].id), "one id for every destination");
    let expected = [alone(true, 256).await, alone(true, 1 << 20).await, alone(false, 1 << 20).await, alone(true, 1 << 20).await];
    for (got, want) in together.iter().zip(&expected) {
        assert_eq!(
            Exchange { id: String::new(), started_at: String::new(), finished_at: String::new(), ..got.clone() },
            Exchange { id: String::new(), started_at: String::new(), finished_at: String::new(), ..want.clone() }
        );
    }
    assert_eq!((together[0].request.len(), together[0].output_text.len()), (256, 0), "a cut response doesn't assemble");
    assert_eq!(together[1].output_text, answer);
    assert!(together[2].request.is_empty() && together[2].output_text.is_empty());
}

#[tokio::test]
async fn the_capture_keeps_what_the_most_generous_destination_wants() {
    let spawn = |bodies, max| Sink::spawn("test", bodies, max, 1, |q: Queue| async move { drop(q) });
    let exporter = Exporter::new(vec![spawn(false, 0), spawn(true, 10), spawn(true, 20)]);
    let capture = exporter.capture("/v1/messages", Some(&Bytes::from("y".repeat(30))));
    assert_eq!(capture.response_limit(), Some(20));
    assert_eq!(capture.request.len(), 20);
    let exporter = Exporter::new(vec![spawn(false, 4096)]);
    assert_eq!(exporter.capture("/", Some(&Bytes::from_static(b"{}"))).response_limit(), None);
}

// Against a real server with JetStream: `XOLOTL_TEST_NATS=nats://host:4222 cargo test -- --ignored nats`.
// Each test makes its own stream on `xolotl.exchanges.>` and deletes it, so they take turns.

static SERVER: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Server {
    url: String,
    js: jetstream::Context,
    stream: String,
}

impl Server {
    async fn new(subjects: &[&str], max_message_size: i32) -> Option<Self> {
        let url = std::env::var("XOLOTL_TEST_NATS").ok()?;
        let client = async_nats::connect(&url).await.unwrap();
        let js = jetstream::new(client);
        let name = format!("XOLOTL_TEST_{}", uuid::Uuid::new_v4().simple());
        js.create_stream(stream::Config {
            name: name.clone(),
            subjects: subjects.iter().map(|s| s.to_string()).collect(),
            max_message_size,
            duplicate_window: Duration::from_secs(120),
            ..Default::default()
        })
        .await
        .unwrap();
        Some(Self { url, js, stream: name })
    }

    fn cfg(&self) -> NatsConfig {
        NatsConfig { url: self.url.clone(), stream: self.stream.clone(), ..Default::default() }
    }

    async fn messages(&self) -> Vec<(String, String, Value)> {
        let stream = self.js.get_stream(&self.stream).await.unwrap();
        let n = stream.get_info().await.unwrap().state.last_sequence;
        let mut out = Vec::new();
        for seq in 1..=n {
            let Ok(m) = stream.get_raw_message(seq).await else { continue };
            let id = m.headers.get("Nats-Msg-Id").map(|v| v.to_string()).unwrap_or_default();
            out.push((m.subject.to_string(), id, serde_json::from_slice(&m.payload).unwrap()));
        }
        out
    }

    async fn delete(self) {
        self.js.delete_stream(&self.stream).await.unwrap();
    }
}

#[tokio::test]
#[ignore = "needs a NATS server"]
async fn nats_startup_checks_the_stream() {
    let _turn = SERVER.lock().await;
    let Some(s) = Server::new(&["xolotl.exchanges.team.>"], -1).await else { return };
    let err = start(s.cfg(), None).await.err().expect("a stream that misses rows is refused");
    assert!(format!("{err:#}").contains("don't cover 'xolotl.exchanges.>'"), "{err:#}");
    let missing = NatsConfig { stream: "XOLOTL_TEST_MISSING".into(), ..s.cfg() };
    let err = start(missing, None).await.err().expect("a missing stream is refused");
    assert!(format!("{err:#}").contains("NATS stream \"XOLOTL_TEST_MISSING\" not found"), "{err:#}");
    let nowhere = NatsConfig { url: "nats://127.0.0.1:9".into(), ..s.cfg() };
    let err = start(nowhere, None).await.err().expect("an unreachable server is refused");
    assert!(format!("{err:#}").contains("connecting to NATS"), "{err:#}");
    s.delete().await;
}

#[tokio::test]
#[ignore = "needs a NATS server"]
async fn nats_publishes_each_row_once() {
    let _turn = SERVER.lock().await;
    let Some(s) = Server::new(&[SUBJECTS], -1).await else { return };
    let publisher = connect(&s.cfg(), None).await.unwrap();
    let r = row(&uuid::Uuid::now_v7().to_string());
    publisher.send(&r).await;
    // A retry after a lost ack: the stream's duplicate window drops it.
    publisher.send(&r).await;
    let messages = s.messages().await;
    assert_eq!(messages.len(), 1);
    let (subject, id, body) = &messages[0];
    assert_eq!(subject, "xolotl.exchanges.team/laptop");
    assert_eq!(id, &r.id);
    assert_eq!(body, &serde_json::to_value(&r).unwrap());
    s.delete().await;
}

#[tokio::test]
#[ignore = "needs a NATS server"]
async fn nats_cuts_or_refuses_rows_too_large_for_the_stream() {
    let _turn = SERVER.lock().await;
    // The stream's own limit is lower than the server's `max_payload`.
    let Some(s) = Server::new(&[SUBJECTS], 64 * 1024).await else { return };
    let publisher = connect(&s.cfg(), None).await.unwrap();
    let mut big = row(&uuid::Uuid::now_v7().to_string());
    big.request = "é".repeat(100 * 1024);
    big.response = "r".repeat(100 * 1024);
    publisher.send(&big).await;
    let mut huge = row(&uuid::Uuid::now_v7().to_string());
    huge.output_text = "o".repeat(100 * 1024);
    publisher.send(&huge).await;
    let messages = s.messages().await;
    assert_eq!(messages.len(), 1, "the row too large without its bodies is refused");
    let (_, id, body) = &messages[0];
    assert_eq!(id, &big.id);
    assert_eq!((body["request_truncated"].as_bool(), body["response_truncated"].as_bool()), (Some(true), Some(true)));
    assert!(serde_json::to_vec(body).unwrap().len() <= 64 * 1024);
    assert!(body["request"].as_str().unwrap().starts_with("éé"));
    s.delete().await;
}

#[tokio::test]
#[ignore = "needs a NATS server"]
async fn nats_export_sends_what_is_queued_on_shutdown() {
    let _turn = SERVER.lock().await;
    let Some(s) = Server::new(&[SUBJECTS], -1).await else { return };
    let exporter = Exporter::new(vec![start(s.cfg(), None).await.unwrap()]);
    let capture = exporter.capture("/v1/messages", Some(&Bytes::from_static(b"{\"model\":\"m\"}")));
    let record = crate::db::RequestRecord {
        client_id: Some("team/laptop".into()),
        provider: "claude".into(),
        endpoint: "/v1/messages".into(),
        status: 200,
        ..Default::default()
    };
    for _ in 0..50 {
        capture.submit(crate::export::Finished {
            record: &record,
            started_at: chrono::Utc::now(),
            session_key: "abc",
            session_source: "test",
            account_kind: "codex",
            upstream_model: "m",
            usage: Default::default(),
            tools: &[],
            response: None,
        });
    }
    exporter.shutdown().await;
    assert_eq!(exporter.sinks()[0].queued(), 0);
    let messages = s.messages().await;
    assert_eq!(messages.len(), 50);
    assert!(messages.iter().all(|(_, id, body)| body["id"] == json!(id) && body["request"] == "{\"model\":\"m\"}"));
    s.delete().await;
}

/// The queue holds rows while NATS restarts. `XOLOTL_TEST_NATS_CONTAINER` names
/// the docker container of the server, which the test stops and starts.
#[tokio::test]
#[ignore = "needs a NATS server in docker"]
async fn nats_rows_wait_for_a_restart() {
    let _turn = SERVER.lock().await;
    let Ok(container) = std::env::var("XOLOTL_TEST_NATS_CONTAINER") else { return };
    let Some(s) = Server::new(&[SUBJECTS], -1).await else { return };
    let sink = start(s.cfg(), None).await.unwrap();
    let exporter = Exporter::new(vec![sink]);
    let docker = |verb: &str| {
        let ok = std::process::Command::new("docker").args([verb, &container]).status().unwrap().success();
        assert!(ok, "docker {verb} {container}");
    };
    docker("stop");
    let ids: Vec<String> = (0..20).map(|_| uuid::Uuid::now_v7().to_string()).collect();
    for id in &ids {
        exporter.enqueue_row(row(id));
    }
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(exporter.sinks()[0].queued() > 0, "rows wait while NATS is down");
    docker("start");
    for _ in 0..300 {
        if exporter.sinks()[0].queued() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(exporter.sinks()[0].queued(), 0, "the queue drained after the restart");
    // The test's client reconnects too.
    let s = Server { js: jetstream::new(async_nats::connect(&s.url).await.unwrap()), ..s };
    let stored: Vec<String> = s.messages().await.into_iter().map(|(_, id, _)| id).collect();
    assert_eq!(stored, ids);
    exporter.shutdown().await;
    s.delete().await;
}

/// Through ClickHouse, set up as the NATS export docs say: stream `XOLOTL`,
/// consumer `clickhouse`, and the tables in database `xolotl`.
/// `XOLOTL_TEST_NATS_E2E=nats://host:4222`, `XOLOTL_TEST_NATS_USER`/`_PASSWORD`,
/// `XOLOTL_TEST_CLICKHOUSE=http://user:password@host:8123`, and
/// `XOLOTL_TEST_CLICKHOUSE_CONTAINER`, which the test stops while it publishes.
#[tokio::test]
#[ignore = "needs NATS and ClickHouse in docker"]
async fn nats_rows_reach_clickhouse_after_its_outage() {
    let _turn = SERVER.lock().await;
    let (Ok(url), Ok(ch), Ok(container)) =
        (std::env::var("XOLOTL_TEST_NATS_E2E"), std::env::var("XOLOTL_TEST_CLICKHOUSE"), std::env::var("XOLOTL_TEST_CLICKHOUSE_CONTAINER"))
    else {
        return;
    };
    let mut secret = Map::new();
    for (k, env) in [("user", "XOLOTL_TEST_NATS_USER"), ("password", "XOLOTL_TEST_NATS_PASSWORD")] {
        if let Ok(v) = std::env::var(env) {
            secret.insert(k.into(), Value::String(v));
        }
    }
    let cfg = NatsConfig { url, ..Default::default() };
    let exporter = Exporter::new(vec![start(cfg, Some(secret)).await.unwrap()]);
    let docker = |verb: &str| {
        let ok = std::process::Command::new("docker").args([verb, &container]).status().unwrap().success();
        assert!(ok, "docker {verb} {container}");
    };
    let select = |q: String| {
        let ch = ch.clone();
        async move {
            let u = url::Url::parse(&ch).unwrap();
            let mut base = u.clone();
            base.set_username("").unwrap();
            base.set_password(None).unwrap();
            let resp = reqwest::Client::new()
                .post(base)
                .basic_auth(u.username(), u.password())
                .body(q)
                .send()
                .await
                .and_then(|r| r.error_for_status());
            match resp {
                Ok(r) => r.text().await.ok(),
                Err(_) => None,
            }
        }
    };
    docker("stop");
    let run = uuid::Uuid::new_v4().simple().to_string();
    let mut ids = Vec::new();
    for i in 0..200 {
        let mut r = row(&uuid::Uuid::now_v7().to_string());
        r.session_key = run.clone();
        r.request = format!("{{\"n\":{i},\"text\":\"{}\"}}", "héllo 漢字 🦎 ".repeat(200));
        ids.push(r.id.clone());
        exporter.enqueue_row(r);
    }
    // Too large for the server's 8 MB `max_payload`: cut, not lost.
    let mut big = row(&uuid::Uuid::now_v7().to_string());
    big.session_key = run.clone();
    big.request = "q".repeat(5 << 20);
    big.response = "r".repeat(5 << 20);
    exporter.enqueue_row(big.clone());
    for _ in 0..300 {
        if exporter.sinks()[0].queued() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(exporter.sinks()[0].queued(), 0, "the stream took the rows while ClickHouse was down");
    docker("start");
    let count = format!("SELECT count(), uniqExact(id) FROM xolotl.exchanges FINAL WHERE session_key = '{run}' FORMAT TSV");
    let mut seen = String::new();
    for _ in 0..600 {
        if let Some(t) = select(count.clone()).await {
            seen = t.trim().to_string();
            if seen == "201\t201" {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert_eq!(seen, "201\t201", "every row once, after ClickHouse came back");
    let q = format!(
        "SELECT toUInt8(request_truncated), toUInt8(response_truncated), JSONExtractString(request, 'text') \
         FROM xolotl.exchanges FINAL WHERE id = '{}' FORMAT TSV",
        ids[0]
    );
    let text = select(q).await.unwrap();
    assert_eq!(text, format!("0\t0\t{}\n", "héllo 漢字 🦎 ".repeat(200)));
    // To fit 8 MB, the response was cut and the request kept.
    let q = format!(
        "SELECT toUInt8(request_truncated), toUInt8(response_truncated), length(request), length(request) + length(response) <= 8388608 \
         FROM xolotl.exchanges FINAL WHERE id = '{}' FORMAT TSV",
        big.id
    );
    assert_eq!(select(q).await.unwrap(), "0\t1\t5242880\t1\n");
    let errors = select("SELECT count() FROM xolotl.exchanges_errors FORMAT TSV".into()).await.unwrap();
    assert_eq!(errors.trim(), "0");
    exporter.shutdown().await;
}
