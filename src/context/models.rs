//! The models a session has used. Once a session has used a model, it can use
//! that model in any capacity after its key's policy excludes it: from any of
//! its conversations, after a switch away and back, across compactions.
// ---
// relationships:
//   implements: ../../docs/design/sessions.md
// ---
use anyhow::Result;
use axum::http::HeaderMap;
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::Value;

use super::resolve;
use crate::db::{Db, now};
use crate::policy::Target;

/// A request's session, as far as the models it has used go: the session and
/// the key issuance that sent it.
pub struct SessionModels {
    db: Db,
    client: String,
    issued: String,
    session: String,
    /// The client named the session, rather than xolotl reading it from the content.
    named: bool,
    continues: bool,
}

impl SessionModels {
    /// The session of a request of the client key `client`, issued at
    /// `issued_at`. `None` for a request with no session.
    pub fn of(db: &Db, client: &str, issued_at: Option<DateTime<Utc>>, headers: &HeaderMap, body: &Value) -> Option<Self> {
        let session = resolve(headers, body)?;
        Some(Self {
            db: db.clone(),
            client: client.to_string(),
            issued: issued_at.map(|t| t.to_rfc3339_opts(SecondsFormat::Nanos, true)).unwrap_or_default(),
            named: session.source != "content",
            session: session.key,
            continues: continues(body),
        })
    }

    /// The models the session has used, which it keeps past a policy that
    /// now excludes them. Every request of a session the client names reads
    /// them. A session xolotl reads from the content is only a guess, as two
    /// requests with the same system prompt and first message are one
    /// session, so there only a request that carries earlier output reads
    /// them: a script that repeats a one-shot request keeps nothing. When
    /// they can't be read, the policy decides alone.
    pub async fn kept(&self) -> Vec<Target> {
        if !self.named && !self.continues {
            return Vec::new();
        }
        self.db.session_models(self).await.unwrap_or_else(|e| {
            tracing::warn!(error = %e, "failed to read the models of a session");
            Vec::new()
        })
    }

    /// Record a completed turn of the session: it has used `target`, and is
    /// active, so its models stay past retention. `None` for a target it
    /// can't keep, one only an alias reaches, which keeps the session active
    /// without keeping that target. [`SessionModels::uses`] calls it once the
    /// answer has completed, before the client sees its end.
    pub async fn record(&self, target: Option<&Target>) {
        let recorded = match target {
            Some(target) => self.db.use_model(self, target).await,
            None => self.db.touch_session_models(self).await,
        };
        if let Err(e) = recorded {
            tracing::warn!(error = %e, "failed to record a model of a session");
        }
    }
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

impl Db {
    async fn session_models(&self, s: &SessionModels) -> Result<Vec<Target>> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT provider, model FROM session_models
             WHERE client_id = ? AND key_issued = ? AND session_key = ? ORDER BY provider, model",
        )
        .bind(&s.client)
        .bind(&s.issued)
        .bind(&s.session)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(provider, model)| Target { provider, model }).collect())
    }

    async fn touch_session_models(&self, s: &SessionModels) -> Result<()> {
        sqlx::query("UPDATE session_models SET last_used = ? WHERE client_id = ? AND key_issued = ? AND session_key = ?")
            .bind(now())
            .bind(&s.client)
            .bind(&s.issued)
            .bind(&s.session)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn use_model(&self, s: &SessionModels, target: &Target) -> Result<()> {
        sqlx::query(
            "INSERT INTO session_models (client_id, key_issued, session_key, provider, model, last_used) VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT (client_id, key_issued, session_key, provider, model) DO UPDATE SET last_used = excluded.last_used",
        )
        .bind(&s.client)
        .bind(&s.issued)
        .bind(&s.session)
        .bind(&target.provider)
        .bind(&target.model)
        .bind(now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

mod completion;
#[cfg(test)]
mod tests;
