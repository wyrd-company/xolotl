//! Session identity, context accounting and the context lock: which
//! conversation an LLM request belongs to, how full that conversation is, and
//! which account and model it stays on once its prompt cache is worth keeping.
// ---
// relationships:
//   implements: ../../docs/design/sessions.md
// ---
mod store;
pub mod tools;

pub use store::{SessionRow, ToolUsageRow, Turn};

use axum::http::HeaderMap;
use opentelemetry::KeyValue;
use parking_lot::Mutex;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::db::Db;
use crate::subscription::{claude, codex, grok, opencode_go};
use crate::telemetry::metrics;

/// Why a lock was released.
pub mod reason {
    /// The locked account is cooling down.
    pub const COOLDOWN: &str = "cooldown";
    /// The locked account was removed, disabled, needs a new login, or its credential is unavailable.
    pub const ACCOUNT_LOST: &str = "account_lost";
    /// The key's policy no longer allows the locked account.
    pub const POLICY: &str = "policy";
    /// The locked account or provider is at a limit of the key's policy.
    pub const LIMIT: &str = "limit";
    /// `subscription.max_attempts` consecutive failed requests on the lock.
    pub const UPSTREAM_ERRORS: &str = "upstream_errors";
    /// The key's policy or the catalog no longer offers the locked target for the requested name.
    pub const TARGET_UNAVAILABLE: &str = "target_unavailable";
}

/// A conversation's identity. `key` is a digest, so neither the value a client
/// sent nor its prompt is stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionId {
    pub key: String,
    /// What the key came from: `claude-code`, `codex`, `grok`, `opencode`,
    /// `prompt-cache-key` or `content`.
    pub source: &'static str,
}

impl SessionId {
    fn new(source: &'static str, parts: &[&[u8]]) -> Self {
        let mut hash = Sha256::new();
        for part in parts {
            hash.update((part.len() as u64).to_be_bytes());
            hash.update(part);
        }
        Self { key: hex::encode(&hash.finalize()[..16]), source }
    }
}

/// The identity of an LLM request: what the client says, else a digest of the
/// system prompt and first user turn. `None` for a body with neither.
pub fn resolve(headers: &HeaderMap, body: &Value) -> Option<SessionId> {
    let sent = |source, value: String| Some(SessionId::new(source, &[source.as_bytes(), value.as_bytes()]));
    if let Some(value) = claude::session_key(headers, body).filter(|v| !v.is_empty()) {
        return sent("claude-code", value);
    }
    let header =
        |names: &[&str]| names.iter().find_map(|h| headers.get(*h)?.to_str().ok().filter(|v| !v.trim().is_empty()).map(str::to_string));
    for (source, names) in
        [("codex", codex::SESSION_HEADERS), ("grok", grok::SESSION_HEADERS), ("opencode", &[opencode_go::SESSION_HEADER])]
    {
        if let Some(value) = header(names) {
            return sent(source, value);
        }
    }
    if let Some(value) = body.get("prompt_cache_key").and_then(Value::as_str).filter(|v| !v.is_empty()) {
        return sent("prompt-cache-key", value.to_string());
    }
    let turns = body.get("messages").or_else(|| body.get("input"));
    let items = || turns.and_then(Value::as_array).into_iter().flatten();
    let role_is = |item: &&Value, roles: &[&str]| item.get("role").and_then(Value::as_str).is_some_and(|r| roles.contains(&r));
    let system = body.get("system").or_else(|| body.get("instructions")).or_else(|| items().find(|i| role_is(i, &["system", "developer"])));
    let first_user = turns.filter(|t| t.is_string()).or_else(|| items().find(|i| role_is(i, &["user"])));
    if system.is_none() && first_user.is_none() {
        return None;
    }
    let bytes = |v: Option<&Value>| v.map(|v| v.to_string().into_bytes()).unwrap_or_default();
    Some(SessionId::new("content", &[&bytes(system), &bytes(first_user)]))
}

/// The account and target a session is pinned to. A gateway target has no account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lock {
    /// The model name the client asked for. Another name is not pinned to the target.
    pub name: String,
    pub provider: String,
    pub model: String,
    pub account: Option<String>,
}

/// How a request relates to its session's locked account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountPin {
    pub account: String,
    /// The request is for the locked name and target, so losing the account
    /// breaks the lock. Otherwise the account is only preferred.
    pub held: bool,
}

/// The session state of one LLM request.
pub struct RequestSession {
    db: Db,
    client: String,
    id: SessionId,
    requested: String,
    estimate: i64,
    lock_tokens: u64,
    lockable: bool,
    max_failures: i64,
    lock: Mutex<Option<Lock>>,
}

impl RequestSession {
    /// Identify the request's session and read its lock. A session can't form
    /// a lock when `lockable` is false: nothing routes the request. A database
    /// failure costs the lock, not the request.
    pub async fn begin(
        db: &Db,
        cfg: &Config,
        client: &str,
        headers: &HeaderMap,
        body: &Value,
        requested: &str,
        lockable: bool,
    ) -> Option<Self> {
        let id = resolve(headers, body)?;
        let lock_tokens = cfg.sessions.lock_tokens;
        let lock = if lock_tokens == 0 || !lockable {
            None
        } else {
            db.session_lock(client, &id.key).await.unwrap_or_else(|e| {
                tracing::warn!(error = %e, "failed to read the session lock");
                None
            })
        };
        Some(Self {
            db: db.clone(),
            client: client.to_string(),
            id,
            requested: requested.to_string(),
            estimate: crate::metadata::estimate_tokens(body) as i64,
            lock_tokens,
            lockable,
            max_failures: cfg.subscription.max_attempts as i64,
            lock: Mutex::new(lock),
        })
    }

    /// The lock as the request sees it now.
    pub fn lock(&self) -> Option<Lock> {
        self.lock.lock().clone()
    }

    /// Whether the session holds a lock on the name this request asked for.
    pub fn locks_requested_name(&self) -> bool {
        self.lock.lock().as_ref().is_some_and(|l| l.name == self.requested)
    }

    /// Whether this request is pinned to the target by the session's lock.
    pub fn holds(&self, provider: &str, model: &str) -> bool {
        self.lock.lock().as_ref().is_some_and(|l| l.name == self.requested && l.provider == provider && l.model == model)
    }

    /// The account a request to this target stays on, if the session has one
    /// on its provider.
    pub fn account_pin(&self, provider: &str, model: &str) -> Option<AccountPin> {
        let lock = self.lock.lock();
        let lock = lock.as_ref().filter(|l| l.provider == provider)?;
        let account = lock.account.clone()?;
        let held = lock.name == self.requested && lock.model == model;
        // The locked name on another target isn't pinned; another name prefers the account.
        (held || lock.name != self.requested).then_some(AccountPin { account, held })
    }

    /// Release the lock for `reason` and record it.
    pub async fn break_lock(&self, reason: &'static str) {
        let Some(lock) = self.lock.lock().take() else { return };
        let released = self.db.break_lock(&self.client, &self.id.key, &lock, reason).await;
        match released {
            Ok(true) => {
                metrics().session_locks.add(
                    1,
                    &[KeyValue::new("event", "broken"), KeyValue::new("reason", reason), KeyValue::new("provider", lock.provider.clone())],
                );
                tracing::warn!(client = %self.client, session = %self.id.key, provider = %lock.provider, model = %lock.model, account = lock.account.as_deref().unwrap_or(""), reason, "context lock broken");
            }
            Ok(false) => {}
            Err(e) => tracing::warn!(error = %e, reason, "failed to record a broken context lock"),
        }
    }

    /// Count a failed request on the lock. At `subscription.max_attempts` in a
    /// row the lock breaks, and this returns false: the request may move on.
    pub async fn upstream_failure(&self) -> bool {
        let Some(lock) = self.lock() else { return false };
        match self.db.count_lock_failure(&self.client, &self.id.key, &lock).await {
            Ok(failures) if failures >= self.max_failures => {
                self.break_lock(reason::UPSTREAM_ERRORS).await;
                false
            }
            Ok(_) => true,
            Err(e) => {
                tracing::warn!(error = %e, "failed to count a context lock failure");
                true
            }
        }
    }

    /// What the request's accounting needs when its response finishes.
    pub fn track(&self, model: &str, window: Option<u64>) -> SessionTrack {
        SessionTrack {
            client: self.client.clone(),
            id: self.id.clone(),
            requested: self.requested.clone(),
            model: model.to_string(),
            window: window.map(|w| w as i64),
            estimate: self.estimate,
            lock_tokens: if self.lockable { self.lock_tokens as i64 } else { 0 },
            side: self.lock.lock().as_ref().is_some_and(|l| l.name != self.requested),
        }
    }
}

/// The session side of a request's accounting.
#[derive(Debug, Clone)]
pub struct SessionTrack {
    client: String,
    id: SessionId,
    requested: String,
    model: String,
    window: Option<i64>,
    estimate: i64,
    lock_tokens: i64,
    side: bool,
}

impl SessionTrack {
    pub fn key(&self) -> &str {
        &self.id.key
    }

    pub fn window(&self) -> Option<i64> {
        self.window
    }

    /// The prompt's size: what the upstream reported, else the estimate.
    pub fn tokens(&self, reported: Option<i64>) -> i64 {
        reported.unwrap_or(self.estimate)
    }

    /// Record a successful request. It can form the session's lock on what served it.
    pub async fn record(self, db: &Db, provider: &str, account: Option<&str>, input: Option<i64>, cached: Option<i64>) {
        let turn = Turn {
            client_id: self.client.clone(),
            session_key: self.id.key.clone(),
            source: self.id.source.to_string(),
            tokens: self.tokens(input),
            reported: input.is_some(),
            side: self.side,
            prompt_tokens: input.unwrap_or(0),
            cached_tokens: cached.unwrap_or(0),
            window: self.window,
            provider: provider.to_string(),
            model: self.model.clone(),
            account: account.map(str::to_string),
        };
        let lock = Lock { name: self.requested, provider: provider.to_string(), model: self.model, account: account.map(str::to_string) };
        match db.finish_turn(&turn, &lock, self.lock_tokens).await {
            Ok(true) => {
                metrics().session_locks.add(1, &[KeyValue::new("event", "formed"), KeyValue::new("provider", lock.provider.clone())])
            }
            Ok(false) => {}
            Err(e) => tracing::warn!(error = %e, "failed to record the session"),
        }
    }
}

#[cfg(test)]
mod tests;
