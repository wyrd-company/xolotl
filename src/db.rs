//! SQLite runtime state: account health/cooldowns, usage snapshots and the
//! request log. Nothing secret is stored here.

use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{FromRow, SqlitePool};

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

#[derive(Clone)]
pub struct Db {
    pool: SqlitePool,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize, PartialEq)]
pub struct AccountRow {
    pub id: String,
    pub provider: String,
    pub label: String,
    pub email: Option<String>,
    /// Provider-side identifier (ChatGPT account id, Anthropic org/account uuid).
    pub external_id: Option<String>,
    pub plan: Option<String>,
    pub enabled: bool,
    pub priority: i64,
    pub cooldown_until: Option<i64>,
    pub cooldown_reason: Option<String>,
    pub auth_failed: bool,
    pub last_error: Option<String>,
    pub last_used_at: Option<i64>,
    pub created_at: i64,
}

impl AccountRow {
    pub fn cooling_down(&self, at: i64) -> bool {
        self.cooldown_until.is_some_and(|t| t > at)
    }
}

/// A normalized quota window, e.g. Claude's 5 hour window or Codex's weekly window.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UsageWindow {
    pub name: String,
    pub used_percent: f64,
    pub resets_at: Option<i64>,
    pub window_secs: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountUsage {
    pub account_id: String,
    pub fetched_at: i64,
    pub windows: Vec<UsageWindow>,
    pub raw: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Default)]
pub struct RequestRecord {
    pub client_id: Option<String>,
    pub provider: String,
    pub account_id: Option<String>,
    pub model: Option<String>,
    pub endpoint: String,
    pub status: u16,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cached_tokens: Option<i64>,
    pub latency_ms: i64,
    pub trace_id: Option<String>,
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct UsageSummary {
    pub key: Option<String>,
    pub requests: i64,
    pub errors: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_tokens: i64,
}

#[derive(Debug, Clone, Default)]
pub struct McpCallRecord {
    pub client_id: Option<String>,
    pub server: String,
    pub tool: String,
    pub status: String,
    pub error: Option<String>,
    pub latency_ms: i64,
    pub trace_id: Option<String>,
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct McpUsageSummary {
    pub key: Option<String>,
    pub calls: i64,
    pub errors: i64,
    pub avg_latency_ms: f64,
}

/// An MCP call log row, as listed by the admin API.
#[derive(Debug, Clone, FromRow, Serialize)]
pub struct LoggedMcpCall {
    pub ts: i64,
    pub client_id: Option<String>,
    pub server: String,
    pub tool: String,
    pub status: String,
    pub error: Option<String>,
    pub latency_ms: i64,
    pub trace_id: Option<String>,
}

/// A request log row, as listed by the admin API.
#[derive(Debug, Clone, FromRow, Serialize)]
pub struct LoggedRequest {
    pub ts: i64,
    pub client_id: Option<String>,
    pub provider: String,
    pub account_id: Option<String>,
    pub model: Option<String>,
    pub endpoint: String,
    pub status: i64,
    pub latency_ms: i64,
    pub trace_id: Option<String>,
}

impl Db {
    pub async fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .foreign_keys(true)
            .busy_timeout(std::time::Duration::from_secs(5));
        let pool = SqlitePoolOptions::new().max_connections(4).connect_with(opts).await?;
        Self::from_pool(pool).await
    }

    pub async fn in_memory() -> Result<Self> {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")?.foreign_keys(true);
        // A single connection keeps the in-memory database alive and shared.
        let pool = SqlitePoolOptions::new().max_connections(1).connect_with(opts).await?;
        Self::from_pool(pool).await
    }

    async fn from_pool(pool: SqlitePool) -> Result<Self> {
        sqlx::migrate!("./migrations").run(&pool).await.context("running migrations")?;
        Ok(Self { pool })
    }

    pub async fn ping(&self) -> Result<()> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }

    // ---- accounts ----

    pub async fn upsert_account(&self, a: &AccountRow) -> Result<()> {
        sqlx::query(
            "INSERT INTO accounts (id, provider, label, email, external_id, plan, enabled, priority, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
               label = excluded.label, email = excluded.email,
               external_id = excluded.external_id, plan = COALESCE(excluded.plan, accounts.plan)",
        )
        .bind(&a.id)
        .bind(&a.provider)
        .bind(&a.label)
        .bind(&a.email)
        .bind(&a.external_id)
        .bind(&a.plan)
        .bind(a.enabled)
        .bind(a.priority)
        .bind(a.created_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn list_accounts(&self, provider: Option<&str>) -> Result<Vec<AccountRow>> {
        let rows = match provider {
            Some(p) => {
                sqlx::query_as("SELECT * FROM accounts WHERE provider = ? ORDER BY priority DESC, created_at, id")
                    .bind(p)
                    .fetch_all(&self.pool)
                    .await?
            }
            None => sqlx::query_as("SELECT * FROM accounts ORDER BY provider, priority DESC, created_at, id").fetch_all(&self.pool).await?,
        };
        Ok(rows)
    }

    pub async fn get_account(&self, id: &str) -> Result<Option<AccountRow>> {
        Ok(sqlx::query_as("SELECT * FROM accounts WHERE id = ?").bind(id).fetch_optional(&self.pool).await?)
    }

    pub async fn delete_account(&self, id: &str) -> Result<bool> {
        let r = sqlx::query("DELETE FROM accounts WHERE id = ?").bind(id).execute(&self.pool).await?;
        Ok(r.rows_affected() > 0)
    }

    pub async fn set_account_enabled(&self, id: &str, enabled: bool) -> Result<bool> {
        let r = sqlx::query("UPDATE accounts SET enabled = ? WHERE id = ?").bind(enabled).bind(id).execute(&self.pool).await?;
        Ok(r.rows_affected() > 0)
    }

    pub async fn set_account_priority(&self, id: &str, priority: i64) -> Result<bool> {
        let r = sqlx::query("UPDATE accounts SET priority = ? WHERE id = ?").bind(priority).bind(id).execute(&self.pool).await?;
        Ok(r.rows_affected() > 0)
    }

    pub async fn set_cooldown(&self, id: &str, until: i64, reason: &str) -> Result<()> {
        // Never shorten an existing, longer cooldown.
        sqlx::query("UPDATE accounts SET cooldown_until = MAX(COALESCE(cooldown_until, 0), ?), cooldown_reason = ? WHERE id = ?")
            .bind(until)
            .bind(reason)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Clear cooldown and auth-failure state (T3 hubs `reset-quota`, manual reset).
    pub async fn clear_cooldown(&self, id: &str) -> Result<bool> {
        let r = sqlx::query("UPDATE accounts SET cooldown_until = NULL, cooldown_reason = NULL, last_error = NULL WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected() > 0)
    }

    pub async fn mark_auth_failed(&self, id: &str, failed: bool, error: Option<&str>) -> Result<()> {
        sqlx::query("UPDATE accounts SET auth_failed = ?, last_error = COALESCE(?, last_error) WHERE id = ?")
            .bind(failed)
            .bind(error)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn record_account_result(&self, id: &str, error: Option<&str>) -> Result<()> {
        sqlx::query("UPDATE accounts SET last_used_at = ?, last_error = ? WHERE id = ?")
            .bind(now())
            .bind(error)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ---- usage snapshots ----

    pub async fn put_usage(&self, u: &AccountUsage) -> Result<()> {
        sqlx::query(
            "INSERT INTO account_usage (account_id, fetched_at, windows, raw) VALUES (?, ?, ?, ?)
             ON CONFLICT(account_id) DO UPDATE SET fetched_at = excluded.fetched_at,
               windows = excluded.windows, raw = COALESCE(excluded.raw, account_usage.raw)",
        )
        .bind(&u.account_id)
        .bind(u.fetched_at)
        .bind(serde_json::to_string(&u.windows)?)
        .bind(u.raw.as_ref().map(|r| r.to_string()))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_usage(&self, account_id: &str) -> Result<Option<AccountUsage>> {
        let row: Option<(String, i64, String, Option<String>)> =
            sqlx::query_as("SELECT account_id, fetched_at, windows, raw FROM account_usage WHERE account_id = ?")
                .bind(account_id)
                .fetch_optional(&self.pool)
                .await?;
        row.map(|(account_id, fetched_at, windows, raw)| {
            Ok(AccountUsage {
                account_id,
                fetched_at,
                windows: serde_json::from_str(&windows)?,
                raw: raw.and_then(|r| serde_json::from_str(&r).ok()),
            })
        })
        .transpose()
    }

    // ---- request log ----

    pub async fn log_request(&self, r: &RequestRecord) -> Result<()> {
        sqlx::query(
            "INSERT INTO request_log (ts, client_id, provider, account_id, model, endpoint, status,
               input_tokens, output_tokens, cached_tokens, latency_ms, trace_id)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(now())
        .bind(&r.client_id)
        .bind(&r.provider)
        .bind(&r.account_id)
        .bind(&r.model)
        .bind(&r.endpoint)
        .bind(r.status as i64)
        .bind(r.input_tokens)
        .bind(r.output_tokens)
        .bind(r.cached_tokens)
        .bind(r.latency_ms)
        .bind(&r.trace_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The latest failed requests (status 400 and up), newest first.
    pub async fn recent_errors(&self, limit: i64) -> Result<Vec<LoggedRequest>> {
        Ok(sqlx::query_as(
            "SELECT ts, client_id, provider, account_id, model, endpoint, status, latency_ms, trace_id
             FROM request_log WHERE status >= 400 ORDER BY ts DESC, id DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Aggregate the request log since `since` grouped by `client`, `account`, `provider` or `model`.
    pub async fn usage_summary(&self, group_by: &str, since: i64) -> Result<Vec<UsageSummary>> {
        let column = match group_by {
            "client" => "client_id",
            "account" => "account_id",
            "provider" => "provider",
            "model" => "model",
            other => anyhow::bail!("cannot group usage by {other:?}"),
        };
        let sql = format!(
            "SELECT {column} AS key, COUNT(*) AS requests,
               SUM(CASE WHEN status >= 400 THEN 1 ELSE 0 END) AS errors,
               COALESCE(SUM(input_tokens), 0) AS input_tokens,
               COALESCE(SUM(output_tokens), 0) AS output_tokens,
               COALESCE(SUM(cached_tokens), 0) AS cached_tokens
             FROM request_log WHERE ts >= ? GROUP BY {column} ORDER BY requests DESC"
        );
        // `column` comes from the fixed whitelist above.
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(since).fetch_all(&self.pool).await?)
    }

    pub async fn prune(&self, retention_days: u32) -> Result<u64> {
        let cutoff = now() - i64::from(retention_days) * 86_400;
        let r = sqlx::query("DELETE FROM request_log WHERE ts < ?").bind(cutoff).execute(&self.pool).await?;
        let m = sqlx::query("DELETE FROM mcp_call_log WHERE ts < ?").bind(cutoff).execute(&self.pool).await?;
        Ok(r.rows_affected() + m.rows_affected())
    }

    // ---- MCP call log ----

    pub async fn log_mcp_call(&self, r: &McpCallRecord) -> Result<()> {
        sqlx::query(
            "INSERT INTO mcp_call_log (ts, client_id, server, tool, status, error, latency_ms, trace_id)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(now())
        .bind(&r.client_id)
        .bind(&r.server)
        .bind(&r.tool)
        .bind(&r.status)
        .bind(&r.error)
        .bind(r.latency_ms)
        .bind(&r.trace_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Aggregate MCP calls since `since` grouped by `client`, `server` or `tool` (`server/tool`).
    pub async fn mcp_usage_summary(&self, group_by: &str, since: i64) -> Result<Vec<McpUsageSummary>> {
        let column = match group_by {
            "client" => "client_id",
            "server" => "server",
            "tool" => "server || '/' || tool",
            other => anyhow::bail!("cannot group MCP calls by {other:?}"),
        };
        let sql = format!(
            "SELECT {column} AS key, COUNT(*) AS calls,
               SUM(CASE WHEN status != 'ok' THEN 1 ELSE 0 END) AS errors,
               CAST(AVG(latency_ms) AS REAL) AS avg_latency_ms
             FROM mcp_call_log WHERE ts >= ? GROUP BY {column} ORDER BY calls DESC"
        );
        // `column` comes from the fixed whitelist above.
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(since).fetch_all(&self.pool).await?)
    }

    /// The latest MCP calls, newest first.
    pub async fn recent_mcp_calls(&self, limit: i64) -> Result<Vec<LoggedMcpCall>> {
        Ok(sqlx::query_as(
            "SELECT ts, client_id, server, tool, status, error, latency_ms, trace_id
             FROM mcp_call_log ORDER BY ts DESC, id DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn account(id: &str, provider: &str) -> AccountRow {
        AccountRow {
            id: id.into(),
            provider: provider.into(),
            label: id.into(),
            email: None,
            external_id: None,
            plan: None,
            enabled: true,
            priority: 0,
            cooldown_until: None,
            cooldown_reason: None,
            auth_failed: false,
            last_error: None,
            last_used_at: None,
            created_at: now(),
        }
    }

    #[tokio::test]
    async fn account_lifecycle_and_cooldown() {
        let db = Db::in_memory().await.unwrap();
        db.upsert_account(&account("a", "codex")).await.unwrap();
        db.upsert_account(&account("b", "claude")).await.unwrap();
        assert_eq!(db.list_accounts(Some("codex")).await.unwrap().len(), 1);

        db.set_cooldown("a", now() + 100, "429").await.unwrap();
        db.set_cooldown("a", now() + 10, "shorter").await.unwrap();
        let a = db.get_account("a").await.unwrap().unwrap();
        assert!(a.cooldown_until.unwrap() >= now() + 99);
        assert!(a.cooling_down(now()));

        assert!(db.clear_cooldown("a").await.unwrap());
        assert!(!db.get_account("a").await.unwrap().unwrap().cooling_down(now()));

        db.put_usage(&AccountUsage {
            account_id: "a".into(),
            fetched_at: 1,
            windows: vec![UsageWindow { name: "5h".into(), used_percent: 12.5, resets_at: None, window_secs: None }],
            raw: None,
        })
        .await
        .unwrap();
        assert_eq!(db.get_usage("a").await.unwrap().unwrap().windows[0].used_percent, 12.5);

        assert!(db.delete_account("a").await.unwrap());
        assert!(db.get_usage("a").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn usage_summary_groups() {
        let db = Db::in_memory().await.unwrap();
        for status in [200, 200, 500] {
            db.log_request(&RequestRecord {
                client_id: Some("c1".into()),
                provider: "codex".into(),
                endpoint: "/v1/responses".into(),
                status,
                input_tokens: Some(10),
                output_tokens: Some(5),
                ..Default::default()
            })
            .await
            .unwrap();
        }
        let s = db.usage_summary("client", 0).await.unwrap();
        assert_eq!(s[0].requests, 3);
        assert_eq!(s[0].errors, 1);
        assert_eq!(s[0].input_tokens, 30);
        assert!(db.usage_summary("bogus", 0).await.is_err());
        let errors = db.recent_errors(10).await.unwrap();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].status, 500);
    }
}
