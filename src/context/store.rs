//! Session state and tool counts in SQLite.
// ---
// relationships:
//   implements: ../../docs/design/sessions.md
// ---
use anyhow::Result;
use serde::Serialize;
use sqlx::{FromRow, SqliteExecutor};

use super::Lock;
use crate::db::{Db, now};

/// One finished request of a session.
#[derive(Debug, Clone, Default)]
pub struct Turn {
    pub client_id: String,
    pub session_key: String,
    pub source: String,
    pub tokens: i64,
    /// `tokens` came from the upstream, not from the local estimate.
    pub reported: bool,
    /// A request for another name in a locked session: it does not change the
    /// session's context figures.
    pub side: bool,
    pub prompt_tokens: i64,
    pub cached_tokens: i64,
    pub window: Option<i64>,
    pub provider: String,
    pub model: String,
    pub account: Option<String>,
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct SessionRow {
    pub client_id: String,
    pub session_key: String,
    pub source: String,
    pub first_seen: i64,
    pub last_seen: i64,
    pub turns: i64,
    pub current_tokens: i64,
    pub peak_tokens: i64,
    pub prompt_tokens: i64,
    pub cached_tokens: i64,
    pub context_window: Option<i64>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub lock_name: Option<String>,
    pub lock_provider: Option<String>,
    pub lock_model: Option<String>,
    pub lock_account: Option<String>,
    pub locked_at: Option<i64>,
    pub lock_failures: i64,
    pub lock_breaks: i64,
    pub last_break_reason: Option<String>,
    pub last_break_at: Option<i64>,
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct ToolUsageRow {
    pub client_id: String,
    pub tool: String,
    /// `llm` for a call in an LLM response, `mcp` for a call through `/mcp`.
    pub source: String,
    pub calls: i64,
    pub last_used: i64,
}

/// Record a successful request and return the session's current size. A
/// success on the locked account ends its run of failures.
async fn record_turn(db: impl SqliteExecutor<'_>, t: &Turn) -> Result<i64> {
    let at = now();
    let side = i64::from(t.side);
    let (current,): (i64,) = sqlx::query_as(
        "INSERT INTO sessions (client_id, session_key, source, first_seen, last_seen, turns, current_tokens, peak_tokens,
           tokens_reported, prompt_tokens, cached_tokens, context_window, provider, model)
         VALUES (?1, ?2, ?3, ?4, ?4, 1, ?5, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
         ON CONFLICT (client_id, session_key) DO UPDATE SET
           last_seen = excluded.last_seen,
           turns = turns + 1,
           prompt_tokens = prompt_tokens + excluded.prompt_tokens,
           cached_tokens = cached_tokens + excluded.cached_tokens,
           current_tokens = CASE WHEN ?12 = 0 AND (excluded.tokens_reported = 1 OR tokens_reported = 0)
             THEN excluded.current_tokens ELSE current_tokens END,
           peak_tokens = CASE WHEN ?12 = 0 AND (excluded.tokens_reported = 1 OR tokens_reported = 0)
             THEN MAX(peak_tokens, excluded.peak_tokens) ELSE peak_tokens END,
           context_window = CASE WHEN ?12 = 0 THEN COALESCE(excluded.context_window, context_window) ELSE context_window END,
           provider = CASE WHEN ?12 = 0 THEN excluded.provider ELSE provider END,
           model = CASE WHEN ?12 = 0 THEN excluded.model ELSE model END,
           tokens_reported = MAX(tokens_reported, CASE WHEN ?12 = 0 THEN excluded.tokens_reported ELSE 0 END),
           lock_failures = CASE WHEN lock_provider = excluded.provider AND lock_account IS ?13 THEN 0 ELSE lock_failures END
         RETURNING current_tokens",
    )
    .bind(&t.client_id)
    .bind(&t.session_key)
    .bind(&t.source)
    .bind(at)
    .bind(t.tokens)
    .bind(i64::from(t.reported))
    .bind(t.prompt_tokens)
    .bind(t.cached_tokens)
    .bind(t.window)
    .bind(&t.provider)
    .bind(&t.model)
    .bind(side)
    .bind(&t.account)
    .fetch_one(db)
    .await?;
    Ok(current)
}

/// Pin the session to the target and account that just served it, unless it
/// holds a lock. True when this call formed the lock.
async fn form_lock(db: impl SqliteExecutor<'_>, client_id: &str, session_key: &str, lock: &Lock) -> Result<bool> {
    let r = sqlx::query(
        "UPDATE sessions SET lock_name = ?, lock_provider = ?, lock_model = ?, lock_account = ?, locked_at = ?, lock_failures = 0,
           lock_generation = lock_generation + 1
         WHERE client_id = ? AND session_key = ? AND lock_provider IS NULL",
    )
    .bind(&lock.name)
    .bind(&lock.provider)
    .bind(&lock.model)
    .bind(&lock.account)
    .bind(now())
    .bind(client_id)
    .bind(session_key)
    .execute(db)
    .await?;
    Ok(r.rows_affected() == 1)
}

type LockColumns = (i64, Option<String>, Option<String>, Option<String>, Option<String>);

impl Db {
    /// The session's lock, if it holds one.
    pub async fn session_lock(&self, client_id: &str, session_key: &str) -> Result<Option<Lock>> {
        let row: Option<LockColumns> = sqlx::query_as(
            "SELECT lock_generation, lock_name, lock_provider, lock_model, lock_account FROM sessions WHERE client_id = ? AND session_key = ?",
        )
        .bind(client_id)
        .bind(session_key)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(|(generation, name, provider, model, account)| {
            Some(Lock { generation, name: name?, provider: provider?, model: model?, account })
        }))
    }

    /// Record a successful request and, at `lock_tokens`, form `lock` on what
    /// served it, in one transaction, so that no request sees a session past
    /// the threshold without its lock. True when this call formed the lock. A
    /// `lock_tokens` of 0 forms none.
    pub async fn finish_turn(&self, t: &Turn, lock: &Lock, lock_tokens: i64) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let current = record_turn(&mut *tx, t).await?;
        let formed =
            lock_tokens > 0 && !t.side && current >= lock_tokens && form_lock(&mut *tx, &t.client_id, &t.session_key, lock).await?;
        tx.commit().await?;
        Ok(formed)
    }

    /// Count a failed request on `lock` and return the run of failures. A lock
    /// the session no longer holds counts nothing and returns 0.
    pub async fn count_lock_failure(&self, client_id: &str, session_key: &str, lock: &Lock) -> Result<i64> {
        let row: Option<(i64,)> = sqlx::query_as(
            "UPDATE sessions SET lock_failures = lock_failures + 1
             WHERE client_id = ? AND session_key = ? AND lock_provider IS NOT NULL AND lock_generation = ?
             RETURNING lock_failures",
        )
        .bind(client_id)
        .bind(session_key)
        .bind(lock.generation)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map_or(0, |r| r.0))
    }

    /// Release `lock` and record why. A lock formed since `lock` was read is
    /// left alone, even on the same target. True when this call released it.
    pub async fn break_lock(&self, client_id: &str, session_key: &str, lock: &Lock, reason: &str) -> Result<bool> {
        let r = sqlx::query(
            "UPDATE sessions SET lock_name = NULL, lock_provider = NULL, lock_model = NULL, lock_account = NULL, locked_at = NULL,
               lock_failures = 0, lock_breaks = lock_breaks + 1, last_break_reason = ?, last_break_at = ?
             WHERE client_id = ? AND session_key = ? AND lock_provider IS NOT NULL AND lock_generation = ?",
        )
        .bind(reason)
        .bind(now())
        .bind(client_id)
        .bind(session_key)
        .bind(lock.generation)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected() == 1)
    }

    /// Sessions by most recent use, narrowed to one client key if given.
    pub async fn sessions(&self, client_id: Option<&str>, limit: i64) -> Result<Vec<SessionRow>> {
        Ok(sqlx::query_as(
            "SELECT client_id, session_key, source, first_seen, last_seen, turns, current_tokens, peak_tokens,
               prompt_tokens, cached_tokens, context_window, provider, model, lock_name, lock_provider, lock_model,
               lock_account, locked_at, lock_failures, lock_breaks, last_break_reason, last_break_at
             FROM sessions WHERE (?1 IS NULL OR client_id = ?1) ORDER BY last_seen DESC, session_key LIMIT ?2",
        )
        .bind(client_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Add one call per name to the client's tool counts.
    pub async fn count_tools(&self, client_id: &str, names: &[String]) -> Result<()> {
        let at = now();
        for name in names {
            sqlx::query(
                "INSERT INTO tool_usage (client_id, tool, calls, last_used) VALUES (?, ?, 1, ?)
                 ON CONFLICT (client_id, tool) DO UPDATE SET calls = calls + 1, last_used = excluded.last_used",
            )
            .bind(client_id)
            .bind(name)
            .bind(at)
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    /// Tool calls per client key: those in LLM responses and those through `/mcp`.
    pub async fn tool_usage(&self, client_id: Option<&str>) -> Result<Vec<ToolUsageRow>> {
        Ok(sqlx::query_as(
            "SELECT client_id, tool, 'llm' AS source, calls, last_used FROM tool_usage WHERE (?1 IS NULL OR client_id = ?1)
             UNION ALL
             SELECT client_id, server || '/' || tool AS tool, 'mcp' AS source, COUNT(*) AS calls, MAX(ts) AS last_used
             FROM mcp_call_log WHERE client_id IS NOT NULL AND (?1 IS NULL OR client_id = ?1) GROUP BY client_id, server, tool
             ORDER BY client_id, calls DESC, tool",
        )
        .bind(client_id)
        .fetch_all(&self.pool)
        .await?)
    }

    pub(crate) async fn prune_sessions(&self, cutoff: i64) -> Result<u64> {
        let s = sqlx::query("DELETE FROM sessions WHERE last_seen < ?").bind(cutoff).execute(&self.pool).await?;
        let t = sqlx::query("DELETE FROM tool_usage WHERE last_used < ?").bind(cutoff).execute(&self.pool).await?;
        Ok(s.rows_affected() + t.rows_affected())
    }
}

#[cfg(test)]
impl Db {
    pub async fn record_turn(&self, t: &Turn) -> Result<i64> {
        record_turn(&self.pool, t).await
    }

    pub async fn form_lock(&self, client_id: &str, session_key: &str, lock: &Lock) -> Result<bool> {
        form_lock(&self.pool, client_id, session_key, lock).await
    }
}
