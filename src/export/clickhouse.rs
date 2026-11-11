//! The direct ClickHouse destination: rows are inserted in batches through
//! the HTTP interface. A batch ClickHouse keeps refusing is dropped and counted.
// ---
// relationships:
//   implements: ../../docs/design/export.md
// ---

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use bytes::Bytes;

use super::{Exchange, Queue, Sink, count};
use crate::config::ClickHouseConfig;

const SINK: &str = "clickhouse";
/// A batch is inserted once it holds this many bytes, whatever `batch_rows` is.
const MAX_BATCH_BYTES: usize = 32 * 1024 * 1024;
/// Attempts at inserting one batch before it is dropped.
const INSERT_ATTEMPTS: u32 = 3;

/// Start inserting into ClickHouse. `user` and `password` come from the
/// vault; the vault's user wins over the configured one.
pub fn start(cfg: ClickHouseConfig, user: Option<String>, password: Option<String>) -> Result<Sink> {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::none());
    if let Some(ca) = &cfg.ca_cert {
        let pem = std::fs::read(ca).with_context(|| format!("reading ClickHouse CA {}", ca.display()))?;
        for cert in reqwest::Certificate::from_pem_bundle(&pem).context("parsing ClickHouse CA bundle")? {
            builder = builder.add_root_certificate(cert);
        }
    }
    let http = builder.build()?;
    let client = Client { http, url: cfg.url.clone(), user: user.or_else(|| cfg.user.clone()), password };
    tracing::info!(url = %cfg.url, database = %cfg.database, table = %cfg.table, bodies = cfg.bodies, "ClickHouse export enabled");
    let worker = Worker { client, table_ready: !cfg.create_table, cfg: cfg.clone() };
    Ok(Sink::spawn(SINK, cfg.bodies, cfg.max_body_bytes, cfg.queue_mb, |q| worker.run(q)))
}

/// The ClickHouse HTTP interface.
#[derive(Clone)]
pub(super) struct Client {
    pub(super) http: reqwest::Client,
    pub(super) url: String,
    pub(super) user: Option<String>,
    pub(super) password: Option<String>,
}

impl Client {
    /// Run `query` with `data` as its input. A query without input is sent as
    /// the body: ClickHouse refuses a POST without a `Content-Length`, which
    /// an empty body doesn't get.
    pub(super) async fn query(&self, query: &str, data: Bytes, settings: &[(&str, &str)]) -> Result<()> {
        let mut url = url::Url::parse(&self.url)?;
        let body = if data.is_empty() {
            Bytes::copy_from_slice(query.as_bytes())
        } else {
            url.query_pairs_mut().append_pair("query", query);
            data
        };
        // `wait_end_of_query`: the status says how the query ended, not how it began.
        url.query_pairs_mut()
            .append_pair("input_format_skip_unknown_fields", "1")
            .append_pair("date_time_input_format", "best_effort")
            .append_pair("wait_end_of_query", "1")
            .extend_pairs(settings);
        let mut req = self.http.post(url);
        if let Some(u) = &self.user {
            req = req.header("X-ClickHouse-User", u);
        }
        if let Some(p) = &self.password {
            req = req.header("X-ClickHouse-Key", p);
        }
        let resp = req.body(body).send().await.context("sending to ClickHouse")?;
        let status = resp.status();
        let failed = resp.headers().contains_key("x-clickhouse-exception-code");
        let text = resp.text().await.context("reading ClickHouse's answer")?;
        if !status.is_success() || failed || text.contains("DB::Exception") {
            anyhow::bail!("ClickHouse returned {status}: {}", text.trim().chars().take(500).collect::<String>());
        }
        Ok(())
    }
}

/// The table xolotl creates when `create_table` is on.
pub fn create_table_sql(cfg: &ClickHouseConfig) -> String {
    let ttl = if cfg.ttl_days > 0 { format!("\nTTL toDateTime(started_at) + INTERVAL {} DAY", cfg.ttl_days) } else { String::new() };
    format!(
        "CREATE TABLE IF NOT EXISTS {db}.{table} (
    id UUID,
    started_at DateTime64(3, 'UTC'),
    finished_at DateTime64(3, 'UTC'),
    trace_id String,
    client_id LowCardinality(String),
    session_key String,
    session_source LowCardinality(String),
    provider LowCardinality(String),
    account_id LowCardinality(String),
    account_kind LowCardinality(String),
    model LowCardinality(String),
    upstream_model LowCardinality(String),
    client_endpoint LowCardinality(String),
    endpoint LowCardinality(String),
    status UInt16,
    latency_ms UInt64,
    streamed Bool,
    input_tokens Nullable(Int64),
    output_tokens Nullable(Int64),
    cached_tokens Nullable(Int64),
    cache_write_tokens Nullable(Int64),
    context_tokens Nullable(Int64),
    context_window Nullable(Int64),
    cost Nullable(Float64),
    tool_calls Array(LowCardinality(String)),
    stop_reason LowCardinality(String),
    error String,
    request String CODEC(ZSTD(3)),
    request_truncated Bool,
    response String CODEC(ZSTD(3)),
    response_truncated Bool,
    output String CODEC(ZSTD(3)),
    output_text String CODEC(ZSTD(3))
)
ENGINE = MergeTree
PARTITION BY toYYYYMM(started_at)
ORDER BY (client_id, session_key, started_at){ttl}
SETTINGS non_replicated_deduplication_window = 1000",
        db = cfg.database,
        table = cfg.table,
    )
}

struct Worker {
    cfg: ClickHouseConfig,
    client: Client,
    table_ready: bool,
}

impl Worker {
    async fn run(mut self, mut q: Queue) {
        let mut tick = tokio::time::interval(Duration::from_secs(self.cfg.flush_secs.max(1)));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut batch = Vec::new();
        let mut bytes = 0;
        loop {
            tokio::select! {
                row = q.rx.recv() => {
                    let Some(row) = row else { break };
                    bytes += row.size();
                    batch.push(row);
                    if batch.len() >= self.cfg.batch_rows || bytes >= MAX_BATCH_BYTES {
                        self.flush(&q, &mut batch, &mut bytes).await;
                    }
                }
                _ = tick.tick() => self.flush(&q, &mut batch, &mut bytes).await,
                _ = &mut q.stop => break,
            }
        }
        q.rx.close();
        while let Ok(row) = q.rx.try_recv() {
            batch.push(row);
        }
        while !batch.is_empty() {
            let rest = batch.split_off(batch.len().min(self.cfg.batch_rows));
            let mut size = batch.iter().map(|r| r.size()).sum();
            self.flush(&q, &mut batch, &mut size).await;
            batch = rest;
        }
    }

    async fn flush(&mut self, q: &Queue, batch: &mut Vec<Arc<Exchange>>, bytes: &mut usize) {
        if batch.is_empty() {
            return;
        }
        let rows = std::mem::take(batch);
        let size = std::mem::take(bytes);
        let mut body = Vec::with_capacity(size);
        for row in &rows {
            if serde_json::to_writer(&mut body, row.as_ref()).is_ok() {
                body.push(b'\n');
            }
        }
        // Shared by every try, not copied.
        let body = Bytes::from(body);
        let query = format!("INSERT INTO {}.{} FORMAT JSONEachRow", self.cfg.database, self.cfg.table);
        // A retry of an insert that landed but wasn't acknowledged is dropped by ClickHouse.
        let token = uuid::Uuid::now_v7().to_string();
        let mut result = Ok(());
        for attempt in 0..INSERT_ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
            }
            result = self.insert(&query, body.clone(), &token).await;
            if result.is_ok() {
                break;
            }
        }
        for row in &rows {
            q.done(row);
        }
        match result {
            Ok(()) => count(SINK, "inserted", rows.len()),
            Err(e) => {
                count(SINK, "failed", rows.len());
                tracing::warn!(error = %format!("{e:#}"), rows = rows.len(), "ClickHouse export insert failed; rows dropped");
            }
        }
    }

    async fn insert(&mut self, query: &str, body: Bytes, token: &str) -> Result<()> {
        if !self.table_ready {
            self.client.query(&create_table_sql(&self.cfg), Bytes::new(), &[]).await.context("creating the export table")?;
            self.table_ready = true;
        }
        self.client.query(query, body, &[("insert_deduplication_token", token)]).await
    }
}
