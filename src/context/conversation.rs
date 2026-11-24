//! The conversations of a session, and the model each one is on. A session
//! keeps every model one of its conversations is on after its key's policy
//! excludes it. A conversation leaves a model when it switches to another one.
// ---
// relationships:
//   implements: ../../docs/design/sessions.md
// ---
use std::sync::atomic::{AtomicI64, Ordering};

use anyhow::Result;
use axum::http::HeaderMap;
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::Value;

use super::{digest, first_user, resolve};
use crate::db::{Db, now};
use crate::policy::Target;

/// One conversation of a session, as a request sees it: the session, the
/// conversation's opening, and the key issuance that sent it.
pub struct Conversation {
    db: Db,
    client: String,
    issued: String,
    session: String,
    opening: String,
    /// The client named the session, rather than xolotl reading it from the content.
    named: bool,
    continues: bool,
    sequence: i64,
}

impl Conversation {
    /// The conversation of a request of the client key `client`, issued at
    /// `issued_at`. `None` for a request with no session or no user turn.
    pub fn of(db: &Db, client: &str, issued_at: Option<DateTime<Utc>>, headers: &HeaderMap, body: &Value) -> Option<Self> {
        let session = resolve(headers, body)?;
        Some(Self {
            db: db.clone(),
            client: client.to_string(),
            issued: issued_at.map(|t| t.to_rfc3339_opts(SecondsFormat::Nanos, true)).unwrap_or_default(),
            named: session.source != "content",
            session: session.key,
            opening: opening(body)?,
            continues: continues(body),
            sequence: sequence(),
        })
    }

    /// The models the session keeps: those its conversations are on. Every
    /// request of a session the client names reads them, so that a new
    /// conversation of the session, such as a subagent or what follows a
    /// compaction, goes on with them. A session xolotl reads from the content
    /// is only a guess at a conversation, so there only a request that carries
    /// earlier output reads them: a repeated one-shot request keeps nothing.
    /// When they can't be read, the policy decides alone.
    pub async fn kept(&self) -> Vec<Target> {
        if !self.named && !self.continues {
            return Vec::new();
        }
        self.db.session_models(self).await.unwrap_or_else(|e| {
            tracing::warn!(error = %e, "failed to read the models of a session");
            Vec::new()
        })
    }

    /// Record that `target` served this request: the conversation is on it
    /// now. `None` for a target the conversation can't keep, one only an
    /// alias reaches. A request that started before the last recorded one
    /// changes nothing, so an older request finishing late can't move the
    /// conversation back. Called before the response reaches the client, so
    /// that the client's next request sees it.
    pub async fn served(&self, target: Option<&Target>) {
        if let Err(e) = self.db.move_conversation(self, target).await {
            tracing::warn!(error = %e, "failed to record the model of a conversation");
        }
    }
}

/// The opening of the conversation a request carries: a digest of its first
/// user turn. A switch of the conversation to another model carries the same
/// opening; a background request or a subagent has its own. Cache markers are
/// left out, as a client moves them while the conversation grows.
pub fn opening(body: &Value) -> Option<String> {
    fn unmarked(v: &Value) -> Value {
        match v {
            Value::Object(m) => {
                Value::Object(m.iter().filter(|(k, _)| *k != "cache_control").map(|(k, v)| (k.clone(), unmarked(v))).collect())
            }
            Value::Array(a) => Value::Array(a.iter().map(unmarked).collect()),
            v => v.clone(),
        }
    }
    Some(digest(&[unmarked(first_user(body)?).to_string().as_bytes()]))
}

/// Whether a request continues a conversation: it carries output of an
/// earlier turn, an assistant message or a Responses output item.
pub fn continues(body: &Value) -> bool {
    let turns = body.get("messages").or_else(|| body.get("input")).and_then(Value::as_array);
    turns.into_iter().flatten().any(|item| {
        let role = item.get("role").and_then(Value::as_str);
        let kind = item.get("type").and_then(Value::as_str).unwrap_or_default();
        role == Some("assistant") || kind.ends_with("_call") || kind == "reasoning" || kind.starts_with("compaction")
    })
}

/// Orders the requests of a conversation: microseconds since the epoch, and
/// past the last value, so that two requests never share one. A restart
/// starts from the clock again.
fn sequence() -> i64 {
    static NEXT: AtomicI64 = AtomicI64::new(0);
    NEXT.fetch_max(Utc::now().timestamp_micros(), Ordering::SeqCst);
    NEXT.fetch_add(1, Ordering::SeqCst)
}

impl Db {
    async fn session_models(&self, c: &Conversation) -> Result<Vec<Target>> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT DISTINCT provider, model FROM session_conversations
             WHERE client_id = ? AND key_issued = ? AND session_key = ? AND provider IS NOT NULL ORDER BY provider, model",
        )
        .bind(&c.client)
        .bind(&c.issued)
        .bind(&c.session)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(provider, model)| Target { provider, model }).collect())
    }

    async fn move_conversation(&self, c: &Conversation, target: Option<&Target>) -> Result<()> {
        sqlx::query(
            "INSERT INTO session_conversations (client_id, key_issued, session_key, opening, provider, model, sequence, last_used)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT (client_id, key_issued, session_key, opening) DO UPDATE SET
               provider = excluded.provider, model = excluded.model, sequence = excluded.sequence, last_used = excluded.last_used
             WHERE excluded.sequence > session_conversations.sequence",
        )
        .bind(&c.client)
        .bind(&c.issued)
        .bind(&c.session)
        .bind(&c.opening)
        .bind(target.map(|t| &t.provider))
        .bind(target.map(|t| &t.model))
        .bind(c.sequence)
        .bind(now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
