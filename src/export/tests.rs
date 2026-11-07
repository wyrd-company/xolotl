use serde_json::json;
use wiremock::matchers::{header, method, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use super::*;

fn bytes_of(v: &Value) -> Bytes {
    Bytes::from(serde_json::to_vec(v).unwrap())
}

fn cfg(url: &str) -> ClickHouseConfig {
    ClickHouseConfig { url: url.to_string(), flush_secs: 1, ..Default::default() }
}

fn record(endpoint: &str, status: u16) -> RequestRecord {
    RequestRecord {
        client_id: Some("team/laptop".into()),
        provider: "claude".into(),
        account_id: Some("acct".into()),
        model: Some("sonnet".into()),
        endpoint: endpoint.into(),
        status,
        latency_ms: 1500,
        trace_id: Some("trace".into()),
        context_tokens: Some(12),
        ..Default::default()
    }
}

fn finished<'a>(record: &'a RequestRecord, tools: &'a [String], response: Option<Captured>) -> Finished<'a> {
    Finished {
        record,
        started_at: DateTime::parse_from_rfc3339("2026-11-07T10:00:00.250Z").unwrap().with_timezone(&Utc),
        session_key: "abc",
        session_source: "claude-code",
        upstream_model: "claude-sonnet-5-5",
        usage: TokenUsage { input: Some(12), output: Some(7), cached: Some(4), cache_write: None },
        tools,
        response,
    }
}

/// Every request ClickHouse received, as (query, body).
async fn received(s: &MockServer) -> Vec<(String, String)> {
    let reqs: Vec<Request> = s.received_requests().await.unwrap_or_default();
    reqs.iter()
        .map(|r| {
            let body = String::from_utf8_lossy(&r.body).into_owned();
            match r.url.query_pairs().find(|(k, _)| k == "query") {
                Some((_, q)) => (q.into_owned(), body),
                // A statement without input data is the body.
                None => (body, String::new()),
            }
        })
        .collect()
}

async fn wait_for_inserts(s: &MockServer, n: usize) -> Vec<Value> {
    for _ in 0..100 {
        let rows: Vec<Value> = received(s)
            .await
            .into_iter()
            .filter(|(q, _)| q.starts_with("INSERT"))
            .flat_map(|(_, b)| b.lines().map(|l| serde_json::from_str(l).unwrap()).collect::<Vec<Value>>())
            .collect();
        if rows.len() >= n {
            return rows;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("ClickHouse got fewer than {n} rows");
}

#[test]
fn truncates_on_a_char_boundary() {
    assert_eq!(truncate("héllo".into(), 2), ("h".to_string(), true));
    assert_eq!(truncate("héllo".into(), 3), ("hé".to_string(), true));
    assert_eq!(truncate("hi".into(), 2), ("hi".to_string(), false));
}

#[test]
fn captured_keeps_the_limit() {
    let mut c = Captured::default();
    c.push(b"abcd", 6);
    assert!(!c.truncated);
    c.push(b"efgh", 6);
    assert_eq!(c.bytes, b"abcdef");
    assert!(c.truncated);
}

#[tokio::test]
async fn assembles_a_streamed_messages_response() {
    let exporter = Exporter::start(cfg("http://127.0.0.1:9"), None, None).unwrap();
    let capture = exporter.capture("/v1/messages", &bytes_of(&json!({"model": "sonnet", "messages": [{"role": "user", "content": "hi"}]})));
    let sse = concat!(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"usage\":{\"input_tokens\":12}}}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello \"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"there\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"Read\",\"input\":{}}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"a\\\"}\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":7}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    );
    let r = record("/v1/messages", 200);
    let tools = vec!["Read".to_string()];
    let resp = Captured { bytes: sse.as_bytes().to_vec(), sse: true, truncated: false };
    let row = capture.exchange(finished(&r, &tools, Some(resp)));
    assert_eq!(row.started_at, "2026-11-07 10:00:00.250");
    assert_eq!(row.finished_at, "2026-11-07 10:00:01.750");
    assert_eq!(row.client_endpoint, "/v1/messages");
    assert_eq!(row.session_key, "abc");
    assert!(row.streamed);
    assert_eq!(row.output_text, "Hello there");
    assert_eq!(row.stop_reason, "tool_use");
    assert_eq!(row.error, "");
    let output: Value = serde_json::from_str(&row.output).unwrap();
    assert_eq!(output[1], json!({"type": "tool_call", "id": "t1", "name": "Read", "arguments": "{\"path\":\"a\"}"}));
    assert_eq!(serde_json::from_str::<Value>(&row.request).unwrap()["messages"][0]["content"], "hi");
    assert_eq!(row.response, sse);
    assert_eq!(row.tool_calls, tools);
}

#[tokio::test]
async fn records_the_upstream_error() {
    let exporter = Exporter::start(cfg("http://127.0.0.1:9"), None, None).unwrap();
    let capture = exporter.capture("/v1/chat/completions", &bytes_of(&json!({"model": "m"})));
    let r = record("/v1/responses", 429);
    let body = br#"{"error":{"type":"usage_limit_reached","message":"try later"}}"#;
    let row = capture.exchange(finished(&r, &[], Some(Captured { bytes: body.to_vec(), sse: false, truncated: false })));
    assert_eq!(row.error, "usage_limit_reached: try later");
    assert_eq!(row.output, "");
    // No response at all: a request that never reached an upstream.
    let row = capture.exchange(finished(&record("/v1/responses", 503), &[], None));
    assert_eq!((row.status, row.response.as_str(), row.error.as_str()), (503, "", ""));
}

#[tokio::test]
async fn without_bodies_keeps_only_the_log_fields() {
    let exporter = Exporter::start(ClickHouseConfig { bodies: false, ..cfg("http://127.0.0.1:9") }, None, None).unwrap();
    let capture = exporter.capture("/v1/messages", &bytes_of(&json!({"model": "secret prompt"})));
    assert_eq!(capture.response_limit(), None);
    let row = capture.exchange(finished(&record("/v1/messages", 200), &[], None));
    assert_eq!(row.request, "");
    assert_eq!(row.input_tokens, Some(12));
}

#[tokio::test]
async fn creates_the_table_then_inserts_batches() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(header("X-ClickHouse-User", "xolotl"))
        .and(header("X-ClickHouse-Key", "pw"))
        .and(query_param("input_format_skip_unknown_fields", "1"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&s)
        .await;
    let exporter =
        Exporter::start(ClickHouseConfig { batch_rows: 2, ttl_days: 30, ..cfg(&s.uri()) }, Some("xolotl".into()), Some("pw".into()))
            .unwrap();
    let capture = exporter.capture("/v1/messages", &bytes_of(&json!({"model": "m"})));
    for _ in 0..3 {
        capture.submit(finished(&record("/v1/messages", 200), &[], None));
    }
    let rows = wait_for_inserts(&s, 3).await;
    assert_eq!(rows[0]["session_key"], "abc");
    assert_eq!(rows[0]["cached_tokens"], 4);
    assert_eq!(rows[0]["cache_write_tokens"], Value::Null);
    let queries = received(&s).await;
    assert!(queries[0].0.starts_with("CREATE TABLE IF NOT EXISTS xolotl.llm_exchanges"), "{}", queries[0].0);
    assert!(queries[0].0.contains("INTERVAL 30 DAY"));
    assert_eq!(queries.iter().filter(|(q, _)| q.starts_with("CREATE")).count(), 1);
    // Two rows filled a batch at once; the third went on the timer.
    assert_eq!(queries.iter().filter(|(q, _)| q == "INSERT INTO xolotl.llm_exchanges FORMAT JSONEachRow").count(), 2);
    exporter.shutdown().await;
    assert_eq!(exporter.queued.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn shutdown_inserts_what_is_queued() {
    let s = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200)).mount(&s).await;
    let exporter = Exporter::start(ClickHouseConfig { create_table: false, flush_secs: 3600, ..cfg(&s.uri()) }, None, None).unwrap();
    let capture = exporter.capture("/v1/messages", &bytes_of(&json!({})));
    capture.submit(finished(&record("/v1/messages", 200), &[], None));
    exporter.shutdown().await;
    let queries = received(&s).await;
    assert_eq!(queries.len(), 1);
    assert!(queries[0].0.starts_with("INSERT"));
}

#[tokio::test]
async fn a_full_queue_drops_rows() {
    // Nothing listens: inserts fail slowly while the queue fills.
    let exporter = Exporter::start(ClickHouseConfig { queue_mb: 1, flush_secs: 3600, ..cfg("http://127.0.0.1:9") }, None, None).unwrap();
    let body = json!({"pad": "x".repeat(400 * 1024)});
    let capture = exporter.capture("/v1/messages", &bytes_of(&body));
    for _ in 0..4 {
        capture.submit(finished(&record("/v1/messages", 200), &[], None));
    }
    let queued = exporter.queued.load(Ordering::Acquire);
    assert!(queued <= 1024 * 1024 && queued > 400 * 1024, "{queued}");
}

#[test]
fn the_table_sql_has_every_column() {
    let sql = create_table_sql(&ClickHouseConfig::default());
    let row = serde_json::to_value(Exchange::default()).unwrap();
    for column in row.as_object().unwrap().keys() {
        assert!(sql.contains(&format!("\n    {column} ")), "{column} is not in the table");
    }
    assert!(!sql.contains("TTL"));
}

/// Against a real server: `XOLOTL_TEST_CLICKHOUSE=http://user:password@host:8123 cargo test -- --ignored clickhouse`.
/// The database `xolotl` must exist.
#[tokio::test]
#[ignore = "needs a ClickHouse server"]
async fn inserts_into_a_real_clickhouse() {
    let Ok(raw) = std::env::var("XOLOTL_TEST_CLICKHOUSE") else { return };
    let url = url::Url::parse(&raw).unwrap();
    let (user, password) = (Some(url.username().to_string()), url.password().map(str::to_string));
    let mut base = url.clone();
    base.set_username("").unwrap();
    base.set_password(None).unwrap();
    let table = format!("test_{}", uuid::Uuid::new_v4().simple());
    let cfg = ClickHouseConfig { url: base.to_string(), table: table.clone(), ttl_days: 7, ..cfg(base.as_str()) };
    let exporter = Exporter::start(cfg, user.clone(), password.clone()).unwrap();
    let capture = exporter.capture("/v1/messages", &bytes_of(&json!({"model": "m", "messages": [{"role": "user", "content": "héllo"}]})));
    let sse = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"usage\":{\"input_tokens\":1}}}\n\n\
               event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
               event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n";
    let tools = vec!["Read".to_string()];
    capture.submit(finished(&record("/v1/messages", 200), &tools, Some(Captured { bytes: sse.into(), sse: true, truncated: false })));
    capture.submit(finished(
        &record("/v1/messages", 429),
        &[],
        Some(Captured { bytes: b"{\"error\":{\"message\":\"slow\"}}".to_vec(), ..Default::default() }),
    ));
    exporter.shutdown().await;

    let client = Client { http: reqwest::Client::new(), url: base.to_string(), user, password };
    let mut url = url::Url::parse(&client.url).unwrap();
    let select = format!(
        "SELECT status, session_key, output_text, tool_calls, cache_write_tokens, streamed, error, \
         JSONExtractString(request, 'messages', 1, 'content') AS first, toString(started_at) AS started \
         FROM xolotl.{table} ORDER BY status FORMAT JSONEachRow"
    );
    url.query_pairs_mut().append_pair("query", &select);
    let mut req = client.http.get(url);
    if let Some(u) = &client.user {
        req = req.header("X-ClickHouse-User", u);
    }
    if let Some(p) = &client.password {
        req = req.header("X-ClickHouse-Key", p);
    }
    let text = req.send().await.unwrap().text().await.unwrap();
    let _ = client.query(&format!("DROP TABLE xolotl.{table}"), bytes::Bytes::new(), &[]).await;
    let rows: Vec<Value> = text.lines().map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{e}: {text}"))).collect();
    assert_eq!(rows.len(), 2, "{text}");
    assert_eq!(rows[0]["output_text"], "ok");
    assert_eq!(rows[0]["error"], "api_error: upstream stream ended before the response completed");
    assert_eq!(rows[0]["tool_calls"], json!(["Read"]));
    assert_eq!(rows[0]["cache_write_tokens"], Value::Null);
    assert_eq!(rows[0]["streamed"], true);
    assert_eq!(rows[0]["first"], "héllo");
    assert_eq!(rows[0]["started"], "2026-11-07 10:00:00.250");
    assert_eq!(rows[1]["error"], "rate_limit_error: slow");
}

#[tokio::test]
async fn a_cut_off_stream_keeps_what_arrived() {
    let exporter = Exporter::start(cfg("http://127.0.0.1:9"), None, None).unwrap();
    let capture = exporter.capture("/v1/responses", &bytes_of(&json!({})));
    let sse = concat!(
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r\"}}\n\n",
        "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"role\":\"assistant\"}}\n\n",
        "data: {\"type\":\"response.content_part.added\",\"output_index\":0,\"content_index\":0,\"part\":{\"type\":\"output_text\",\"text\":\"\"}}\n\n",
        "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"content_index\":0,\"delta\":\"partial ans\"}\n\n",
    );
    let r = record("/v1/responses", 200);
    let row = capture.exchange(finished(&r, &[], Some(Captured { bytes: sse.as_bytes().to_vec(), sse: true, truncated: true })));
    assert_eq!(row.output_text, "partial ans");
    assert!(row.response_truncated);
    assert!(row.error.contains("ended before the response completed"), "{}", row.error);
    assert_eq!(row.stop_reason, "");
}

#[test]
fn prefix_stops_at_the_limit() {
    let b = Bytes::from_static(r#"{"text":"ééééé"}"#.as_bytes());
    let (whole, cut) = prefix(&b, 1024);
    assert_eq!((&whole[..], cut), (&b[..], false));
    // `{"text":"é` is 11 bytes; the limit falls inside the next é.
    let (head, cut) = prefix(&b, 12);
    assert_eq!((std::str::from_utf8(&head).unwrap(), cut), (r#"{"text":"é"#, true));
    assert_eq!(head.as_ptr(), b.as_ptr(), "the prefix shares the body's buffer");
}

#[tokio::test]
async fn row_size_counts_every_field() {
    let exporter = Exporter::start(ClickHouseConfig { bodies: false, ..cfg("http://127.0.0.1:9") }, None, None).unwrap();
    let capture = exporter.capture("/v1/chat/completions", &bytes_of(&json!({})));
    let tools: Vec<String> = (0..1000).map(|i| format!("tool_{i:04}")).collect();
    let row = capture.exchange(finished(&record("/v1/chat/completions", 200), &tools, None));
    assert!(row.size() > 9 * 1000, "{}", row.size());
    // An error body without a message is cut.
    let body = "x".repeat(MAX_ERROR_BYTES * 4);
    let r = record("/v1/chat/completions", 500);
    let resp = Captured { bytes: body.into_bytes(), ..Default::default() };
    let row = Exporter::start(cfg("http://127.0.0.1:9"), None, None).unwrap().capture("/", &bytes_of(&json!({}))).exchange(finished(
        &r,
        &[],
        Some(resp),
    ));
    assert!(row.error.len() <= MAX_ERROR_BYTES && row.error.starts_with("api_error: xxx"), "{}", row.error.len());
}

#[tokio::test]
async fn a_passthrough_query_string_still_assembles() {
    let exporter = Exporter::start(cfg("http://127.0.0.1:9"), None, None).unwrap();
    let capture = exporter.capture("/v1/chat/completions", &bytes_of(&json!({})));
    let body = json!({"choices": [{"message": {"role": "assistant", "content": "yes"}, "finish_reason": "stop"}]}).to_string();
    let r = record("/v1/chat/completions?beta=1", 200);
    let row = capture.exchange(finished(&r, &[], Some(Captured { bytes: body.into_bytes(), ..Default::default() })));
    assert_eq!(row.output_text, "yes");
}

#[tokio::test]
async fn retries_of_a_batch_carry_one_deduplication_token() {
    let s = MockServer::start().await;
    // The first insert fails after ClickHouse may have stored it; the retry must be recognisable.
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500).set_body_string("Code: 999. DB::Exception: lost"))
        .up_to_n_times(1)
        .mount(&s)
        .await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200)).mount(&s).await;
    let exporter = Exporter::start(ClickHouseConfig { create_table: false, batch_rows: 1, ..cfg(&s.uri()) }, None, None).unwrap();
    exporter.capture("/v1/messages", &bytes_of(&json!({}))).submit(finished(&record("/v1/messages", 200), &[], None));
    exporter.shutdown().await;
    let reqs = s.received_requests().await.unwrap();
    let tokens: Vec<String> = reqs
        .iter()
        .map(|r| r.url.query_pairs().find(|(k, _)| k == "insert_deduplication_token").map(|(_, v)| v.into_owned()).unwrap())
        .collect();
    assert_eq!(tokens.len(), 2);
    assert_eq!(tokens[0], tokens[1]);
    assert!(reqs.iter().all(|r| r.url.query_pairs().any(|(k, v)| k == "wait_end_of_query" && v == "1")));
}

#[tokio::test]
async fn an_exception_after_a_200_is_a_failure() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("Code: 241. DB::Exception: memory"))
        .mount(&s)
        .await;
    let client = Client { http: reqwest::Client::new(), url: s.uri(), user: None, password: None };
    assert!(client.query("INSERT INTO t FORMAT JSONEachRow", bytes::Bytes::from_static(b"{}\n"), &[]).await.is_err());
}

#[tokio::test]
async fn shutdown_waits_for_rows_being_built() {
    let s = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200)).mount(&s).await;
    let exporter = Exporter::start(ClickHouseConfig { create_table: false, flush_secs: 3600, ..cfg(&s.uri()) }, None, None).unwrap();
    let pending = exporter.capture("/v1/messages", &bytes_of(&json!({}))).pending();
    let late = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        pending.submit(finished(&record("/v1/messages", 200), &[], None));
    });
    exporter.shutdown().await;
    late.await.unwrap();
    assert_eq!(wait_for_inserts(&s, 1).await.len(), 1);
}

#[tokio::test]
async fn a_ca_bundle_that_cant_be_read_stops_startup() {
    let missing = ClickHouseConfig { ca_cert: Some("/nonexistent/ca.pem".into()), ..cfg("https://127.0.0.1:9") };
    let err = Exporter::start(missing, None, None).err().expect("a missing CA bundle is an error");
    assert!(format!("{err:#}").contains("reading ClickHouse CA /nonexistent/ca.pem"), "{err:#}");

    let dir = tempfile::tempdir().unwrap();
    let garbage = dir.path().join("ca.pem");
    std::fs::write(&garbage, "-----BEGIN CERTIFICATE-----\nnot base64\n-----END CERTIFICATE-----\n").unwrap();
    let bad = ClickHouseConfig { ca_cert: Some(garbage), ..cfg("https://127.0.0.1:9") };
    assert!(Exporter::start(bad, None, None).is_err(), "an unparseable CA bundle is an error");
}
