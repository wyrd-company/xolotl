//! Subscription proxy: pools of OAuth (Codex, Claude, Grok) and API-key (OpenCode
//! Go) accounts with token refresh, load balancing, cooldowns on quota
//! exhaustion, failover, and usage tracking.

pub mod claude;
pub mod codex;
pub mod grok;
pub mod oauth;
pub mod opencode_go;

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use futures::StreamExt;
use futures::stream::BoxStream;
use opentelemetry::KeyValue;
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::config::{Strategy, SubscriptionConfig};
use crate::db::{AccountRow, AccountUsage, Db, UsageWindow, now};
use crate::keys::Principal;
use crate::limits::{Exceeded, Limiter, Meter};
use crate::proxy::{self, Accounting};
use crate::secrets::{Secrets, get_typed, put_typed};
use crate::telemetry::metrics;
use oauth::{FlowKind, FlowStart, FlowStatus, Flows, TokenResponse};

/// Names that gateway providers may not use; they address the pools.
pub const RESERVED_PROVIDER_NAMES: &[&str] = &["codex", "claude", "opencode-go", "grok"];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum SubProvider {
    #[default]
    Codex,
    Claude,
    OpencodeGo,
    Grok,
}

impl SubProvider {
    pub const ALL: [SubProvider; 4] = [SubProvider::Codex, SubProvider::Claude, SubProvider::OpencodeGo, SubProvider::Grok];

    pub fn as_str(self) -> &'static str {
        match self {
            SubProvider::Codex => "codex",
            SubProvider::Claude => "claude",
            SubProvider::OpencodeGo => "opencode-go",
            SubProvider::Grok => "grok",
        }
    }

    fn uses_oauth(self) -> bool {
        !matches!(self, SubProvider::OpencodeGo)
    }
}

impl fmt::Display for SubProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SubProvider {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        SubProvider::ALL.into_iter().find(|p| p.as_str() == s).ok_or_else(|| anyhow!("unknown subscription provider {s:?}"))
    }
}

/// Account credential as stored in the vault at `accounts/<provider>/<id>`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Credential {
    pub provider: SubProvider,
    pub access_token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
    /// Unix seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// ChatGPT account id / Anthropic account uuid / xAI `sub`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub organization_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// Stable per-account device id (Claude `metadata.user_id`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_refresh: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl Credential {
    pub fn bearer(&self) -> &str {
        self.api_key.as_deref().unwrap_or(&self.access_token)
    }

    /// Stable identity used to derive the account id, so logging in to the
    /// same upstream account again replaces it instead of duplicating it.
    fn external_identity(&self) -> Option<String> {
        match self.provider {
            SubProvider::OpencodeGo => self.api_key.clone(),
            _ => self.account_id.clone().or_else(|| self.email.clone()),
        }
    }
}

/// How a failed upstream attempt should be handled.
#[derive(Debug, Clone, PartialEq)]
pub enum Failure {
    /// Credential rejected: refresh and retry once, then mark the account failed.
    Auth,
    /// The account's quota is exhausted: cool it down and try another account.
    Exhausted { until: Option<i64> },
    /// Transient upstream problem: try another account.
    Transient { retry_after: Option<i64> },
    /// The request itself is bad: return the upstream response to the client.
    Fatal,
}

#[derive(Debug)]
pub enum RefreshError {
    /// The grant is dead; the account must log in again.
    Terminal(String),
    Transient(String),
    /// The request may have spent the refresh token, but no usable response arrived.
    Uncertain(String),
}

impl fmt::Display for RefreshError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RefreshError::Terminal(m) => write!(f, "refresh rejected: {m}"),
            RefreshError::Transient(m) => write!(f, "refresh failed: {m}"),
            RefreshError::Uncertain(m) => write!(f, "refresh outcome unknown: {m}"),
        }
    }
}

impl RefreshError {
    fn from_send_error(error: reqwest::Error) -> Self {
        // Only a connection/build failure proves the token request was not sent.
        // Do not log the request URL or provider body: either can contain secrets.
        if error.is_connect() || error.is_builder() {
            Self::Transient("token request was not sent (connection or request construction failed)".into())
        } else {
            Self::Uncertain("token response was lost; the provider may have rotated the refresh token".into())
        }
    }

    async fn from_response(resp: reqwest::Response) -> Result<TokenResponse, RefreshError> {
        let status = resp.status().as_u16();
        let text = resp.text().await.map_err(|_| Self::Uncertain("token response body was lost; the refresh token may be spent".into()))?;
        if (200..300).contains(&status) {
            return serde_json::from_str::<TokenResponse>(&text)
                .ok()
                .filter(|t| !t.access_token.is_empty())
                .ok_or_else(|| Self::Uncertain("unusable successful token response; the refresh token may be spent".into()));
        }
        // Provider error descriptions can echo tokens. Keep only known diagnostics.
        let msg = if oauth::is_reused_grant_error(&text) {
            "refresh token was reused; this login may also be in use by a local CLI or another instance. Stop sharing the login and log in again"
                .to_string()
        } else {
            format!("token endpoint returned HTTP {status}")
        };
        if oauth::is_terminal_grant_error(status, &text) { Err(Self::Terminal(msg)) } else { Err(Self::Transient(msg)) }
    }
}

enum SelectError {
    NoAccounts,
    /// The pool has accounts, but the caller's policy uses none of them.
    NotAllowed,
    AllCooling {
        until: i64,
    },
    /// Every usable account reached a limit of the caller's policy; the one
    /// that admits a request soonest.
    Limited(Exceeded),
}

/// Account view for the admin API / CLI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountView {
    #[serde(flatten)]
    pub row: AccountRow,
    pub auth_index: String,
    pub available: bool,
    pub expires_at: Option<i64>,
    /// The in-memory credential is newer than the vault copy (write pending).
    pub unsaved: bool,
    pub usage: Option<AccountUsage>,
}

pub struct Subscriptions {
    pub cfg: SubscriptionConfig,
    db: Db,
    secrets: Secrets,
    http: reqwest::Client,
    /// Each account's credential behind its own lock. Every write of an
    /// account's credential to the vault happens while holding that lock, and
    /// a lock that is no longer in this map (account removed or replaced) is
    /// stale: holders must not write through it.
    creds: RwLock<HashMap<String, CredLock>>,
    /// Serializes changes to the set of accounts (sync, save, remove).
    membership: tokio::sync::Mutex<()>,
    rr: Mutex<HashMap<SubProvider, usize>>,
    affinity: Mutex<HashMap<String, (String, Instant)>>,
    /// Accounts whose refreshed credential has not reached the vault yet.
    /// The old refresh token is already spent, so these are retried until saved.
    unsaved: Mutex<HashSet<String>>,
    refresh_retries: Mutex<HashMap<String, RefreshRetry>>,
    pub flows: Flows,
    /// Sent to Grok's chat proxy (`catalog.grok_client_version`).
    grok_client_version: String,
    limiter: Arc<Limiter>,
}

struct RefreshRetry {
    failures: u32,
    after: Instant,
    error: String,
}

type CredLock = Arc<tokio::sync::Mutex<Credential>>;

pub fn auth_index(account_id: &str) -> String {
    hex::encode(&Sha256::digest(format!("xolotl:{account_id}").as_bytes())[..8])
}

fn retry_after(h: &reqwest::header::HeaderMap) -> Option<i64> {
    h.get("retry-after").and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<f64>().ok()).map(|s| s.ceil() as i64)
}

impl Subscriptions {
    pub fn new(cfg: SubscriptionConfig, db: Db, secrets: Secrets, http: reqwest::Client) -> Self {
        Self {
            cfg,
            db,
            secrets,
            http,
            creds: RwLock::new(HashMap::new()),
            membership: tokio::sync::Mutex::new(()),
            rr: Mutex::new(HashMap::new()),
            affinity: Mutex::new(HashMap::new()),
            unsaved: Mutex::new(HashSet::new()),
            refresh_retries: Mutex::new(HashMap::new()),
            flows: Flows::default(),
            grok_client_version: crate::config::CatalogConfig::default().grok_client_version,
            limiter: Default::default(),
        }
    }

    /// Count response tokens against client rate limits.
    pub fn with_limiter(mut self, limiter: Arc<Limiter>) -> Self {
        self.limiter = limiter;
        self
    }

    pub fn with_grok_client_version(mut self, version: impl Into<String>) -> Self {
        self.grok_client_version = version.into();
        self
    }

    // ---------------------------------------------------------------- store

    /// Load every account credential from the vault and make sure each has a
    /// state row. Safe to call repeatedly.
    pub async fn sync(&self) -> Result<usize> {
        let _membership = self.membership.lock().await;
        let mut fresh = HashMap::new();
        for provider in SubProvider::ALL {
            for id in self.secrets.list(&format!("accounts/{provider}")).await? {
                if id.ends_with('/') {
                    continue;
                }
                let Some(cred) = get_typed::<Credential>(&*self.secrets, &format!("accounts/{provider}/{id}")).await? else {
                    continue;
                };
                self.db.upsert_account(&self.row_for(&id, &cred)).await?;
                // Keep in-memory credentials (they may be fresher than the vault
                // if a write failed) and only add new ones.
                let existing = self.creds.read().get(&id).cloned();
                fresh.insert(id, existing.unwrap_or_else(|| Arc::new(tokio::sync::Mutex::new(cred))));
            }
        }
        let n = fresh.len();
        *self.creds.write() = fresh;
        Ok(n)
    }

    fn row_for(&self, id: &str, cred: &Credential) -> AccountRow {
        AccountRow {
            id: id.to_string(),
            provider: cred.provider.as_str().into(),
            label: cred.label.clone().or_else(|| cred.email.clone()).unwrap_or_else(|| id.to_string()),
            email: cred.email.clone(),
            external_id: cred.account_id.clone(),
            plan: cred.plan.clone(),
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

    async fn persist(&self, id: &str, cred: &Credential) -> Result<()> {
        put_typed(&*self.secrets, &format!("accounts/{}/{id}", cred.provider), cred).await
    }

    pub async fn save_account(&self, cred: Credential) -> Result<AccountRow> {
        let _membership = self.membership.lock().await;
        let mut id = match cred.external_identity() {
            Some(ext) => format!("{}-{}", cred.provider, &hex::encode(Sha256::digest(ext.as_bytes()))[..10]),
            None => format!("{}-{}", cred.provider, crate::keys::random_id(10)),
        };
        if cred.provider == SubProvider::OpencodeGo {
            let key = cred.api_key.as_deref().filter(|key| !key.is_empty()).context("API key is required")?;
            let stored = self.api_key_accounts().await?;
            if let Some((existing_id, _)) = stored.iter().find(|(_, existing)| existing.api_key.as_deref() == Some(key)) {
                id = existing_id.clone();
            } else {
                // Replacement keeps the original ID, so its hash slot may now
                // belong to a different key. Never overwrite that account.
                while stored.iter().any(|(existing_id, _)| existing_id == &id) {
                    id = format!("opencode-go-{}", crate::keys::random_id(10));
                }
            }
        }
        // Wait for any refresh of the previous login to finish, so it can't
        // overwrite this one; its lock then goes stale.
        let previous = self.creds.read().get(&id).cloned();
        let _old = match &previous {
            Some(l) => Some(l.lock().await),
            None => None,
        };
        self.persist(&id, &cred).await?;
        self.db.upsert_account(&self.row_for(&id, &cred)).await?;
        // A fresh login supersedes a previous auth failure.
        self.db.mark_auth_failed(&id, false, None).await?;
        self.db.record_account_result(&id, None).await?;
        self.creds.write().insert(id.clone(), Arc::new(tokio::sync::Mutex::new(cred)));
        self.refresh_retries.lock().remove(&id);
        if self.unsaved.lock().remove(&id) {
            self.report_unsaved();
        }
        self.db.get_account(&id).await?.context("account vanished after save")
    }

    /// Read the authoritative API-key records while holding the membership lock.
    /// Imports must also find accounts that have not reached the local sync yet.
    async fn api_key_accounts(&self) -> Result<Vec<(String, Credential)>> {
        let mut accounts = Vec::new();
        for id in self.secrets.list("accounts/opencode-go").await? {
            if id.ends_with('/') {
                continue;
            }
            if let Some(cred) = get_typed::<Credential>(&*self.secrets, &format!("accounts/opencode-go/{id}")).await? {
                accounts.push((id, cred));
            }
        }
        Ok(accounts)
    }

    /// Replace an API key without changing the account identity or usage history.
    pub async fn replace_key(&self, id: &str, api_key: &str) -> Result<AccountRow> {
        anyhow::ensure!(!api_key.trim().is_empty(), "API key must not be empty");
        let _membership = self.membership.lock().await;
        let row = self.db.get_account(id).await?.context("no such account")?;
        anyhow::ensure!(row.provider == SubProvider::OpencodeGo.as_str(), "account does not use an API key");
        let stored = self.api_key_accounts().await?;
        anyhow::ensure!(
            stored.iter().all(|(existing_id, existing)| existing_id == id || existing.api_key.as_deref() != Some(api_key.trim())),
            "API key is already stored on another account"
        );
        let lock = self.creds.read().get(id).cloned().context("account credentials not loaded")?;
        let mut cred = lock.lock().await;
        let mut replacement = cred.clone();
        replacement.api_key = Some(api_key.trim().to_string());
        self.persist(id, &replacement).await?;
        *cred = replacement;
        self.db.clear_cooldown(id).await?;
        self.db.mark_auth_failed(id, false, None).await?;
        self.db.get_account(id).await?.context("account vanished after replacement")
    }

    pub async fn remove_account(&self, id: &str) -> Result<bool> {
        let _membership = self.membership.lock().await;
        let Some(row) = self.db.get_account(id).await? else { return Ok(false) };
        // Wait for an in-flight refresh or retry to finish writing, then make
        // its lock stale so nothing writes the account back afterwards.
        let lock = self.creds.read().get(id).cloned();
        let _guard = match &lock {
            Some(l) => Some(l.lock().await),
            None => None,
        };
        self.creds.write().remove(id);
        self.refresh_retries.lock().remove(id);
        if self.unsaved.lock().remove(id) {
            self.report_unsaved();
        }
        self.secrets.delete(&format!("accounts/{}/{id}", row.provider)).await?;
        self.db.delete_account(id).await?;
        Ok(true)
    }

    /// Lock an account's credential. Fails if the account was removed or
    /// replaced while waiting, so a stale holder never writes to the vault.
    async fn lock_current(&self, id: &str) -> Result<tokio::sync::OwnedMutexGuard<Credential>> {
        let lock = self.creds.read().get(id).cloned().with_context(|| format!("no credential loaded for account {id}"))?;
        let guard = lock.clone().lock_owned().await;
        let current = self.creds.read().get(id).is_some_and(|l| Arc::ptr_eq(l, &lock));
        anyhow::ensure!(current, "account {id} was removed or replaced");
        Ok(guard)
    }

    pub async fn accounts(&self, provider: Option<SubProvider>) -> Result<Vec<AccountView>> {
        let rows = self.db.list_accounts(provider.map(|p| p.as_str())).await?;
        let t = now();
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let usage = self.db.get_usage(&row.id).await?;
            let cred = self.creds.read().get(&row.id).cloned();
            let expires_at = match &cred {
                Some(c) => c.lock().await.expires_at,
                None => None,
            };
            out.push(AccountView {
                auth_index: auth_index(&row.id),
                available: row.enabled && !row.auth_failed && !row.cooling_down(t) && cred.is_some(),
                expires_at,
                unsaved: self.unsaved.lock().contains(&row.id),
                usage,
                row,
            });
        }
        Ok(out)
    }

    pub async fn find_by_auth_index(&self, idx: &str) -> Result<Option<AccountRow>> {
        Ok(self.db.list_accounts(None).await?.into_iter().find(|r| auth_index(&r.id) == idx))
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    // --------------------------------------------------------------- tokens

    /// The account's credential with a valid access token. With
    /// `force_if_token`, refresh if the current token is still that one
    /// (i.e. nobody refreshed it since it was rejected upstream).
    pub async fn credential(&self, id: &str, force_if_token: Option<&str>) -> Result<Credential> {
        let mut cred = self.lock_current(id).await?;
        if !cred.provider.uses_oauth() {
            return Ok(cred.clone());
        }
        // Imported Claude tokens can have unknown age, including imports already
        // stored in OpenBao. Establish their lifetime on first use.
        let unknown_expiry = cred.provider == SubProvider::Claude
            && cred.expires_at.is_none()
            && cred.refresh_token.as_deref().is_some_and(|t| !t.is_empty());
        let expiring = unknown_expiry || cred.expires_at.is_some_and(|e| e - self.cfg.refresh_margin_secs <= now());
        let forced = force_if_token.is_some_and(|t| t == cred.access_token);
        if expiring || forced {
            let waiting = self.refresh_retries.lock().get(id).is_some_and(|r| r.after > Instant::now());
            if waiting {
                anyhow::ensure!(!forced, "token refresh retry pending for account {id}");
                return Ok(cred.clone());
            }
            self.refresh_locked(id, &mut cred, !forced).await?;
        }
        Ok(cred.clone())
    }

    /// Refresh while holding the account's lock, so a single-use refresh token
    /// is never sent twice.
    async fn refresh_locked(&self, id: &str, cred: &mut Credential, allow_current: bool) -> Result<()> {
        let ep = &self.cfg.endpoints;
        let result = match cred.provider {
            SubProvider::Codex => codex::refresh(&self.http, ep, cred).await,
            SubProvider::Claude => claude::refresh(&self.http, ep, cred).await,
            SubProvider::Grok => grok::refresh(&self.http, ep, cred).await,
            SubProvider::OpencodeGo => return Ok(()),
        };
        let provider = cred.provider.as_str();
        let attrs = |r: &'static str| [KeyValue::new("provider", provider), KeyValue::new("result", r)];
        match result {
            Ok(tr) => {
                match cred.provider {
                    SubProvider::Codex => codex::apply_tokens(cred, tr),
                    SubProvider::Claude => claude::apply_tokens(cred, tr),
                    SubProvider::Grok => grok::apply_tokens(cred, tr),
                    SubProvider::OpencodeGo => {}
                }
                metrics().token_refreshes.add(1, &attrs("ok"));
                self.refresh_retries.lock().remove(id);
                tracing::info!(account = id, "refreshed access token");
                // The old refresh token is now spent, so the new one must reach
                // the vault. On failure keep serving from memory and retry the
                // write in the background (`flush_unsaved`) until it lands.
                if let Err(e) = self.persist(id, cred).await {
                    tracing::error!(account = id, error = %e, "refreshed token could not be written to the vault; will retry");
                    self.mark_unsaved(id, Some(&e.to_string())).await;
                } else {
                    if self.unsaved.lock().remove(id) {
                        self.report_unsaved();
                    }
                    // A SQLite failure must not prevent saving a rotated token.
                    let _ = self.db.mark_auth_failed(id, false, None).await;
                    let _ = self.db.record_account_result(id, None).await;
                }
                Ok(())
            }
            Err(RefreshError::Terminal(msg)) => {
                self.refresh_retries.lock().remove(id);
                metrics().token_refreshes.add(1, &attrs("terminal"));
                tracing::warn!(account = id, error = %msg, "refresh token rejected; account needs to log in again");
                self.db.mark_auth_failed(id, true, Some(&format!("re-login required: {msg}"))).await?;
                Err(anyhow!("account {id} needs to log in again"))
            }
            Err(error) => {
                let uncertain = matches!(error, RefreshError::Uncertain(_));
                let msg = error.to_string();
                metrics().token_refreshes.add(1, &attrs(if uncertain { "uncertain" } else { "error" }));
                if uncertain {
                    tracing::error!(account = id, error = %msg, "OAuth refresh may have spent a single-use token; re-login may be required");
                } else {
                    tracing::warn!(account = id, error = %msg, "token refresh failed");
                }
                let error = {
                    let mut retries = self.refresh_retries.lock();
                    let failures = retries.get(id).map_or(1, |r| (r.failures + 1).min(7));
                    let delay = Duration::from_secs((5 * (1u64 << (failures - 1))).min(300));
                    let error = format!("{msg}; refresh retry in {}s", delay.as_secs());
                    retries.insert(id.to_string(), RefreshRetry { failures, after: Instant::now() + delay, error: error.clone() });
                    error
                };
                let _ = self.db.record_account_result(id, Some(&error)).await;
                // Only a forced refresh follows an access token rejected with 401.
                if allow_current { Ok(()) } else { Err(anyhow!("{msg}")) }
            }
        }
    }

    async fn mark_unsaved(&self, id: &str, error: Option<&str>) {
        self.unsaved.lock().insert(id.to_string());
        self.report_unsaved();
        let msg = format!("refreshed credential not yet saved to the vault: {}", error.unwrap_or("write failed"));
        let _ = self.db.record_account_result(id, Some(&msg)).await;
    }

    fn report_unsaved(&self) {
        metrics().unsaved_credentials.record(self.unsaved.lock().len() as u64, &[]);
    }

    pub fn unsaved_count(&self) -> usize {
        self.unsaved.lock().len()
    }

    /// Write every credential whose earlier vault write failed. Called
    /// periodically and on shutdown; returns how many are still unsaved.
    pub async fn flush_unsaved(&self) -> usize {
        let ids: Vec<String> = self.unsaved.lock().iter().cloned().collect();
        for id in ids {
            // Hold the account's lock through the write, so a concurrent
            // refresh can't save a newer credential that this one overwrites.
            let Ok(cred) = self.lock_current(&id).await else {
                // Removed or replaced meanwhile; nothing left to save.
                self.unsaved.lock().remove(&id);
                continue;
            };
            if !self.unsaved.lock().contains(&id) {
                // A refresh saved a newer credential while we waited.
                continue;
            }
            match self.persist(&id, &cred).await {
                Ok(()) => {
                    self.unsaved.lock().remove(&id);
                    let _ = self.record_account_result_locked(&id, None).await;
                    tracing::info!(account = %id, "refreshed credential saved to the vault");
                }
                Err(e) => tracing::warn!(account = %id, error = %e, "credential still not saved to the vault"),
            }
        }
        self.report_unsaved();
        self.unsaved_count()
    }

    // ------------------------------------------------------------ selection

    async fn select(
        &self,
        provider: SubProvider,
        principal: Option<&Principal>,
        session: Option<&str>,
        tried: &HashSet<String>,
    ) -> Result<AccountRow, SelectError> {
        let rows = self.db.list_accounts(Some(provider.as_str())).await.unwrap_or_default();
        let pooled = !rows.is_empty();
        let t = now();
        let loaded: HashSet<String> = self.creds.read().keys().cloned().collect();
        let mut earliest: Option<i64> = None;
        let mut limited: Option<Exceeded> = None;
        let mut allowed_any = false;
        let mut candidates = Vec::new();
        for r in rows {
            if principal.is_some_and(|p| !p.policy.allows_account(provider.as_str(), &r.id)) {
                continue;
            }
            allowed_any = true;
            if !r.enabled || r.auth_failed || !loaded.contains(&r.id) || tried.contains(&r.id) {
                continue;
            }
            if r.cooling_down(t) {
                let u = r.cooldown_until.unwrap_or(t);
                earliest = Some(earliest.map_or(u, |e| e.min(u)));
                continue;
            }
            if let Some(p) = principal
                && let Some(client) = p.client_id()
                && let Err(e) = self.limiter.check(client, &p.policy, Meter::Account(&r.id), t)
            {
                if limited.as_ref().is_none_or(|l| e.retry_after < l.retry_after) {
                    limited = Some(e);
                }
                continue;
            }
            let max_used = self
                .db
                .get_usage(&r.id)
                .await
                .ok()
                .flatten()
                .map(|u| u.windows.iter().map(|w| w.used_percent).fold(0.0, f64::max))
                .unwrap_or(0.0);
            candidates.push((r, max_used));
        }
        if candidates.is_empty() {
            return Err(match (limited, earliest) {
                (Some(e), Some(until)) if until - t < e.retry_after => SelectError::AllCooling { until },
                (Some(e), _) => SelectError::Limited(e),
                (None, Some(until)) => SelectError::AllCooling { until },
                (None, None) if !allowed_any && pooled => SelectError::NotAllowed,
                (None, None) => SelectError::NoAccounts,
            });
        }

        // Session affinity keeps prompt caches warm and avoids replaying
        // encrypted reasoning to a different account.
        if let Some(s) = session.filter(|_| self.cfg.session_affinity_secs > 0) {
            let ttl = Duration::from_secs(self.cfg.session_affinity_secs);
            let pinned = self.affinity.lock().get(s).filter(|(_, at)| at.elapsed() < ttl).map(|(id, _)| id.clone());
            if let Some(id) = pinned
                && let Some((r, used)) = candidates.iter().find(|(r, _)| r.id == id)
                && *used < self.cfg.soft_limit_percent
            {
                return Ok(r.clone());
            }
        }

        // Best tier: not soft-limited first, then highest priority.
        let soft = self.cfg.soft_limit_percent;
        let tier = |(r, used): &(AccountRow, f64)| (*used >= soft, -r.priority);
        let best = candidates.iter().map(tier).min().expect("non-empty");
        let mut group: Vec<(AccountRow, f64)> = candidates.into_iter().filter(|c| tier(c) == best).collect();
        let chosen = match self.cfg.strategy {
            Strategy::FillFirst => group.swap_remove(0).0,
            Strategy::LeastUsed => {
                group.sort_by(|a, b| a.1.total_cmp(&b.1));
                group.swap_remove(0).0
            }
            Strategy::RoundRobin => {
                let mut rr = self.rr.lock();
                let n = rr.entry(provider).or_insert(0);
                let idx = *n % group.len();
                *n = n.wrapping_add(1);
                group.swap_remove(idx).0
            }
        };
        Ok(chosen)
    }

    fn pin_session(&self, session: Option<&str>, account: &str) {
        let Some(s) = session.filter(|_| self.cfg.session_affinity_secs > 0) else { return };
        let mut m = self.affinity.lock();
        if m.len() > 10_000 {
            let ttl = Duration::from_secs(self.cfg.session_affinity_secs);
            m.retain(|_, (_, at)| at.elapsed() < ttl);
        }
        m.insert(s.to_string(), (account.to_string(), Instant::now()));
    }

    /// Keep a pending refresh diagnostic when recording a later proxy result.
    async fn record_account_result(&self, id: &str, error: Option<&str>) -> Result<()> {
        // Serialize the check and SQLite write with refresh and new logins, so
        // a delayed response cannot clear a diagnostic recorded in between.
        let _cred = self.lock_current(id).await?;
        self.record_account_result_locked(id, error).await
    }

    async fn record_account_result_locked(&self, id: &str, error: Option<&str>) -> Result<()> {
        let refresh_error = self.refresh_retries.lock().get(id).map(|r| r.error.clone());
        self.db.record_account_result(id, refresh_error.as_deref().or(error)).await
    }

    // ------------------------------------------------------------ execution

    fn error_for(&self, provider: SubProvider, path: &str, status: StatusCode, kind: &str, msg: &str) -> Response {
        if provider == SubProvider::Claude || path.starts_with("/v1/messages") {
            proxy::anthropic_error(status, kind, msg)
        } else {
            proxy::error_response(status, kind, msg)
        }
    }

    /// Proxy one request through the provider's account pool, failing over
    /// between accounts on quota, auth and transient errors.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute(
        &self,
        provider: SubProvider,
        method: Method,
        path: &str,
        headers: HeaderMap,
        body: Bytes,
        principal: Option<&Principal>,
        model: Option<&str>,
    ) -> Response {
        let client_id = principal.and_then(Principal::client_id);
        let ep = &self.cfg.endpoints;
        let url = match provider {
            SubProvider::Codex => codex::upstream_url(ep, path),
            SubProvider::Claude => claude::upstream_url(ep, path),
            SubProvider::OpencodeGo => opencode_go::upstream_url(ep, path),
            SubProvider::Grok => grok::upstream_url(ep, path),
        };
        let Some(url) = url else {
            return self.error_for(provider, path, StatusCode::NOT_FOUND, "not_found_error", &format!("{provider} does not serve {path}"));
        };
        let body_json: Option<Value> = if body.is_empty() { None } else { serde_json::from_slice(&body).ok() };
        let session = match provider {
            SubProvider::Codex => codex::session_key(&headers, body_json.as_ref().unwrap_or(&Value::Null)),
            SubProvider::Claude => claude::session_key(&headers, body_json.as_ref().unwrap_or(&Value::Null)),
            SubProvider::OpencodeGo => Some(opencode_go::session_key(&headers, client_id)),
            SubProvider::Grok => grok::session_key(&headers, body_json.as_ref().unwrap_or(&Value::Null)),
        };
        let mut accounting = Accounting::new(self.db.clone(), provider.as_str(), path, client_id, model).with_limiter(&self.limiter);
        let mut tried = HashSet::new();
        // Last failed upstream response, returned if every attempt fails.
        let mut last: Option<(u16, reqwest::header::HeaderMap, Bytes, String)> = None;

        'accounts: for _ in 0..self.cfg.max_attempts {
            let account = match self.select(provider, principal, session.as_deref(), &tried).await {
                Ok(a) => a,
                Err(_) if last.is_some() => break,
                Err(SelectError::NotAllowed) => {
                    accounting.finish(403, Default::default());
                    return self.error_for(
                        provider,
                        path,
                        StatusCode::FORBIDDEN,
                        "permission_error",
                        &format!("this key's policy uses no {provider} account"),
                    );
                }
                Err(SelectError::Limited(e)) => {
                    accounting.finish(429, Default::default());
                    let policy = principal.map(|p| p.policy.name.as_str()).unwrap_or_default();
                    let mut resp = self.error_for(provider, path, StatusCode::TOO_MANY_REQUESTS, "rate_limit_error", &e.message(policy));
                    resp.headers_mut().insert(axum::http::header::RETRY_AFTER, e.retry_after.into());
                    return resp;
                }
                Err(SelectError::NoAccounts) => {
                    accounting.finish(503, Default::default());
                    return self.error_for(
                        provider,
                        path,
                        StatusCode::SERVICE_UNAVAILABLE,
                        "api_error",
                        &format!("no usable {provider} accounts (log in with `xolotl auth login {provider}`)"),
                    );
                }
                Err(SelectError::AllCooling { until }) => {
                    accounting.finish(429, Default::default());
                    return self.all_cooling(provider, path, until);
                }
            };
            tried.insert(account.id.clone());
            // Count the request on the account; another request may have
            // reached its limit since `select` checked it.
            if let Some(p) = principal
                && let Some(client) = p.client_id()
                && self.limiter.admit(client, &p.policy, &[Meter::Account(&account.id)], now()).is_err()
            {
                continue 'accounts;
            }
            let mut force: Option<String> = None;
            loop {
                let cred = match self.credential(&account.id, force.as_deref()).await {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!(account = %account.id, error = %e, "account credential unavailable");
                        continue 'accounts;
                    }
                };
                let mut h = headers.clone();
                let mut b = body_json.clone().unwrap_or(Value::Null);
                match provider {
                    SubProvider::Codex => codex::prepare(&mut h, &mut b, &cred, path),
                    SubProvider::Claude => claude::prepare(&mut h, &mut b, &cred, path),
                    SubProvider::OpencodeGo => opencode_go::prepare(&mut h, &mut b, &cred, path, session.as_deref().unwrap_or_default()),
                    SubProvider::Grok => grok::prepare(&mut h, &mut b, &cred, &self.grok_client_version),
                }
                let mut req = self.http.request(method.clone(), &url).headers(h);
                if body_json.is_some() {
                    req = req.body(serde_json::to_vec(&b).unwrap_or_default());
                } else if !body.is_empty() {
                    req = req.body(body.clone());
                }
                let resp = match req.send().await {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!(account = %account.id, error = %e, "upstream request failed");
                        metrics()
                            .upstream_errors
                            .add(1, &[KeyValue::new("provider", provider.as_str()), KeyValue::new("kind", "transport")]);
                        let _ = self.record_account_result(&account.id, Some(&e.to_string())).await;
                        last = Some((
                            502,
                            Default::default(),
                            Bytes::from(json!({"error": {"type": "api_error", "message": "upstream request failed"}}).to_string()),
                            account.id.clone(),
                        ));
                        continue 'accounts;
                    }
                };
                let status = resp.status().as_u16();
                self.observe_headers(&account.id, provider, resp.headers()).await;

                if (200..300).contains(&status) {
                    // Codex always streams (`prepare` forces `stream: true`) except
                    // on /compact, and the backend may omit the Content-Type.
                    let codex_stream = provider == SubProvider::Codex && (!path.ends_with("/compact") || proxy::is_sse(resp.headers()));
                    if codex_stream {
                        // Quota errors can arrive as the first events of a 200 stream;
                        // peek so we can still fail over before the client sees bytes.
                        let resp_headers = resp.headers().clone();
                        let mut stream = resp.bytes_stream().boxed();
                        let (prefix, error_event) = peek_sse(&mut stream).await;
                        if let Some(ev) = error_event {
                            let failure = codex::classify(200, &ev, None);
                            if failure != Failure::Fatal {
                                self.handle_failure(&account, provider, &failure, status).await;
                                let body = Bytes::from(ev.to_string());
                                last = Some((
                                    if matches!(failure, Failure::Exhausted { .. }) { 429 } else { 502 },
                                    Default::default(),
                                    body,
                                    account.id.clone(),
                                ));
                                continue 'accounts;
                            }
                        }
                        let _ = self.record_account_result(&account.id, None).await;
                        self.pin_session(session.as_deref(), &account.id);
                        accounting.record.account_id = Some(account.id.clone());
                        return proxy::relay_sse(status, &resp_headers, prefix, stream, Some(accounting));
                    }
                    if provider == SubProvider::Grok && proxy::is_sse(resp.headers()) {
                        let _ = self.record_account_result(&account.id, None).await;
                        self.pin_session(session.as_deref(), &account.id);
                        accounting.record.account_id = Some(account.id.clone());
                        let resp_headers = resp.headers().clone();
                        let stream = grok::KeepaliveFilter::new(resp.bytes_stream().boxed()).boxed();
                        return proxy::relay_sse(status, &resp_headers, Vec::new(), stream, Some(accounting));
                    }
                    let _ = self.record_account_result(&account.id, None).await;
                    self.pin_session(session.as_deref(), &account.id);
                    accounting.record.account_id = Some(account.id.clone());
                    return proxy::relay(resp, Some(accounting));
                }

                let ra = retry_after(resp.headers());
                let resp_headers = resp.headers().clone();
                let bytes = resp.bytes().await.unwrap_or_default();
                let val: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
                let failure = match provider {
                    SubProvider::Codex => codex::classify(status, &val, ra),
                    SubProvider::Claude => claude::classify(status, &resp_headers, &val, ra),
                    SubProvider::OpencodeGo => opencode_go::classify(status, ra),
                    SubProvider::Grok => grok::classify(status, &bytes, ra),
                };
                metrics()
                    .upstream_errors
                    .add(1, &[KeyValue::new("provider", provider.as_str()), KeyValue::new("status", i64::from(status))]);
                let reason = (provider == SubProvider::OpencodeGo).then(|| opencode_go::error_reason(status, &bytes, &cred));
                tracing::info!(account = %account.id, status, ?failure, reason = reason.as_deref(), "upstream error");
                if failure == Failure::Auth && force.is_none() && provider.uses_oauth() {
                    force = Some(cred.access_token.clone());
                    continue;
                }
                if provider == SubProvider::Grok && status == 426 {
                    let version = grok::client_version(&headers, &self.grok_client_version);
                    let _ = self.record_account_result(&account.id, Some("upstream status 426")).await;
                    accounting.record.account_id = Some(account.id.clone());
                    accounting.finish(426, Default::default());
                    let msg = format!(
                        "xAI rejected Grok client version {version} as too old (426); raise catalog.grok_client_version or update the Grok CLI"
                    );
                    return self.error_for(provider, path, StatusCode::UPGRADE_REQUIRED, "invalid_request_error", &msg);
                }
                if failure == Failure::Fatal {
                    let _ = self.record_account_result(&account.id, reason.as_deref()).await;
                    accounting.record.account_id = Some(account.id.clone());
                    return proxy::relay_buffered(status, &resp_headers, bytes, Some(accounting));
                }
                self.handle_failure(&account, provider, &failure, status).await;
                if let Some(reason) = reason.as_deref() {
                    let _ = self.record_account_result(&account.id, Some(reason)).await;
                }
                last = Some((status, resp_headers, bytes, account.id.clone()));
                continue 'accounts;
            }
        }

        match last {
            Some((status, headers, bytes, account_id)) => {
                accounting.record.account_id = Some(account_id);
                proxy::relay_buffered(status, &headers, bytes, Some(accounting))
            }
            None => {
                accounting.finish(503, Default::default());
                self.error_for(provider, path, StatusCode::SERVICE_UNAVAILABLE, "api_error", "no account could serve the request")
            }
        }
    }

    fn all_cooling(&self, provider: SubProvider, path: &str, until: i64) -> Response {
        let wait = (until - now()).max(1);
        let msg = format!("all {provider} accounts are rate limited; next reset in {wait}s");
        let mut resp = if provider == SubProvider::Codex {
            let body = json!({"error": {"type": "usage_limit_reached", "message": msg, "resets_at": until, "resets_in_seconds": wait}});
            (StatusCode::TOO_MANY_REQUESTS, [(axum::http::header::CONTENT_TYPE, "application/json")], body.to_string())
                .into_response_compat()
        } else {
            self.error_for(provider, path, StatusCode::TOO_MANY_REQUESTS, "rate_limit_error", &msg)
        };
        if let Ok(v) = wait.to_string().parse() {
            resp.headers_mut().insert(axum::http::header::RETRY_AFTER, v);
        }
        resp
    }

    async fn handle_failure(&self, account: &AccountRow, provider: SubProvider, failure: &Failure, status: u16) {
        let t = now();
        let res = match failure {
            Failure::Auth => self.db.mark_auth_failed(&account.id, true, Some(&format!("upstream rejected credentials ({status})"))).await,
            Failure::Exhausted { until } => {
                let until = until.unwrap_or(t + self.cfg.default_cooldown_secs as i64);
                tracing::info!(account = %account.id, %provider, cooldown_secs = until - t, "account quota exhausted");
                self.db.set_cooldown(&account.id, until, "quota exhausted").await
            }
            Failure::Transient { retry_after: Some(s) } if status == 429 => {
                self.db.set_cooldown(&account.id, t + (*s).clamp(1, 300), "rate limited").await
            }
            _ => Ok(()),
        };
        if let Err(e) = res {
            tracing::warn!(error = %e, "failed to record account failure");
        }
        let _ = self.record_account_result(&account.id, Some(&format!("upstream status {status}"))).await;
    }

    /// Merge usage windows observed on a response into the stored snapshot.
    async fn observe_headers(&self, id: &str, provider: SubProvider, h: &reqwest::header::HeaderMap) {
        let windows = match provider {
            SubProvider::Codex => codex::windows_from_headers(h),
            SubProvider::Claude => claude::windows_from_headers(h),
            SubProvider::OpencodeGo | SubProvider::Grok => Vec::new(),
        };
        if windows.is_empty() {
            return;
        }
        let mut merged = self.db.get_usage(id).await.ok().flatten().map(|u| u.windows).unwrap_or_default();
        for w in windows {
            match merged.iter_mut().find(|m| m.name == w.name) {
                Some(m) => *m = w,
                None => merged.push(w),
            }
        }
        self.record_usage(id, provider, merged, None).await;
    }

    async fn record_usage(&self, id: &str, provider: SubProvider, windows: Vec<UsageWindow>, raw: Option<Value>) {
        let m = metrics();
        for w in &windows {
            m.account_usage.record(
                w.used_percent,
                &[
                    KeyValue::new("account", id.to_string()),
                    KeyValue::new("provider", provider.as_str()),
                    KeyValue::new("window", w.name.clone()),
                ],
            );
        }
        let usage = AccountUsage { account_id: id.to_string(), fetched_at: now(), windows, raw };
        if let Err(e) = self.db.put_usage(&usage).await {
            tracing::warn!(account = id, error = %e, "failed to store usage");
        }
    }

    // ---------------------------------------------------------------- usage

    /// Poll one account's usage endpoint and store the result.
    pub async fn poll_usage(&self, id: &str) -> Result<AccountUsage> {
        let row = self.db.get_account(id).await?.with_context(|| format!("unknown account {id}"))?;
        let provider: SubProvider = row.provider.parse()?;
        let mut cred = self.credential(id, None).await?;
        if provider == SubProvider::Grok {
            // No known usage endpoint; the credential check above keeps the token fresh.
            return Ok(AccountUsage { account_id: id.to_string(), fetched_at: now(), windows: Vec::new(), raw: None });
        }
        let ep = &self.cfg.endpoints;
        let mut attempt = 0;
        let mut body: Value = loop {
            let req = match provider {
                SubProvider::Codex => codex::usage_request(&self.http, ep, &cred),
                SubProvider::Claude => claude::usage_request(&self.http, ep, &cred),
                SubProvider::OpencodeGo => opencode_go::usage_request(&self.http, ep, &cred),
                SubProvider::Grok => unreachable!("returned above"),
            };
            let resp = req.timeout(Duration::from_secs(15)).send().await?;
            let status = resp.status();
            if status == StatusCode::UNAUTHORIZED && attempt == 0 && provider.uses_oauth() {
                attempt += 1;
                cred = self.credential(id, Some(&cred.access_token)).await?;
                continue;
            }
            let text = resp.text().await.unwrap_or_default();
            anyhow::ensure!(status.is_success(), "usage request failed: {status}: {}", text.chars().take(200).collect::<String>());
            break serde_json::from_str(&text).context("usage response is not JSON")?;
        };
        let (windows, plan) = match provider {
            SubProvider::Codex => codex::parse_usage(&body),
            SubProvider::Claude => (claude::parse_usage(&body), None),
            SubProvider::OpencodeGo => (opencode_go::parse_usage(&body), None),
            SubProvider::Grok => (Vec::new(), None),
        };
        if let Some(plan) = plan.filter(|p| row.plan.as_deref() != Some(p)) {
            let mut updated = row.clone();
            updated.plan = Some(plan);
            self.db.upsert_account(&updated).await?;
        }
        // An exhausted account-wide window benches the account until it resets.
        let t = now();
        for w in &windows {
            let account_wide = match provider {
                SubProvider::Claude => claude::is_account_window(&w.name),
                _ => true,
            };
            if account_wide && w.used_percent >= 100.0 && w.resets_at.is_some_and(|r| r > t) {
                self.db.set_cooldown(id, w.resets_at.unwrap_or(t), &format!("{} window exhausted", w.name)).await?;
            }
        }
        if provider == SubProvider::Codex {
            anyhow::ensure!(body.is_object(), "Codex usage response is not an object");
            // Credit lookup failures must not discard quota windows or retain an old count.
            let credits = match self.poll_reset_credits(id, cred).await {
                Ok(count) => Some(count),
                Err(e) => {
                    tracing::warn!(account = id, error = %e, "reset credit poll failed");
                    None
                }
            };
            body["reset_credits"] = serde_json::to_value(credits)?;
        }
        self.record_usage(id, provider, windows, Some(body)).await;
        self.db.get_usage(id).await?.context("usage vanished")
    }

    async fn poll_reset_credits(&self, id: &str, mut cred: Credential) -> Result<usize> {
        for attempt in 0..2 {
            let resp = codex::reset_credits_request(&self.http, &self.cfg.endpoints, &cred).timeout(Duration::from_secs(15)).send().await?;
            if resp.status() == StatusCode::UNAUTHORIZED && attempt == 0 {
                cred = self.credential(id, Some(&cred.access_token)).await?;
                continue;
            }
            let body: Value = resp.error_for_status()?.json().await?;
            return codex::parse_reset_credits(&body, now());
        }
        unreachable!("second attempt returns")
    }

    pub async fn poll_all(&self) {
        let rows = match self.db.list_accounts(None).await {
            Ok(r) => r,
            Err(e) => return tracing::warn!(error = %e, "listing accounts for usage poll"),
        };
        let t = now();
        for r in rows.into_iter().filter(|r| r.enabled && !r.auth_failed) {
            if let Err(e) = self.poll_usage(&r.id).await {
                tracing::warn!(account = %r.id, error = %e, "usage poll failed");
            }
            let available = !self.db.get_account(&r.id).await.ok().flatten().is_none_or(|a| a.cooling_down(t));
            metrics()
                .account_available
                .record(u64::from(available), &[KeyValue::new("account", r.id.clone()), KeyValue::new("provider", r.provider.clone())]);
        }
    }

    // --------------------------------------------------------------- models

    /// The models the provider serves, asked with the first account that answers.
    /// Empty when the pool has no usable accounts.
    pub async fn fetch_models(&self, provider: SubProvider, codex_client_version: &str) -> Result<Vec<String>> {
        let rows = self.db.list_accounts(Some(provider.as_str())).await?;
        let ep = &self.cfg.endpoints;
        let mut last_err = None;
        for row in rows.into_iter().filter(|r| r.enabled && !r.auth_failed) {
            let cred = match self.credential(&row.id, None).await {
                Ok(c) => c,
                Err(e) => {
                    last_err = Some(e);
                    continue;
                }
            };
            let req = match provider {
                SubProvider::Claude => claude::models_request(&self.http, ep, &cred),
                SubProvider::Codex => codex::models_request(&self.http, ep, &cred, codex_client_version),
                SubProvider::Grok => grok::models_request(&self.http, ep, &cred, &self.grok_client_version),
                SubProvider::OpencodeGo => {
                    self.http.get(format!("{}/v1/models", ep.opencode_go.trim_end_matches('/'))).bearer_auth(cred.bearer())
                }
            };
            let result: Result<Value> = async {
                let resp = req.timeout(Duration::from_secs(15)).send().await?;
                anyhow::ensure!(resp.status().is_success(), "model list returned {}", resp.status());
                Ok(resp.json().await?)
            }
            .await;
            match result {
                Ok(body) => {
                    return Ok(match provider {
                        SubProvider::Codex => codex::parse_models(&body),
                        _ => body
                            .get("data")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                            .filter_map(|m| m.get("id").and_then(Value::as_str).map(str::to_string))
                            .collect(),
                    });
                }
                Err(e) => last_err = Some(e),
            }
        }
        match last_err {
            Some(e) => Err(e),
            None => anyhow::bail!("no available accounts"),
        }
    }

    // ------------------------------------------------------ management call

    /// T3Code hubs `api-call`: send a request to the provider with the
    /// account's token substituted for `$TOKEN$`. Only the provider's own
    /// hosts are allowed, so the management key can't exfiltrate tokens.
    pub async fn api_call(
        &self,
        account: &AccountRow,
        method: &str,
        url: &str,
        headers: &HashMap<String, String>,
        data: Option<&str>,
    ) -> Result<(u16, reqwest::header::HeaderMap, String)> {
        let provider: SubProvider = account.provider.parse()?;
        let parsed = url::Url::parse(url).context("invalid url")?;
        let ep = &self.cfg.endpoints;
        let allowed: Vec<String> = match provider {
            SubProvider::Codex => vec![ep.codex_backend.clone()],
            SubProvider::Claude => vec![ep.claude_api.clone()],
            SubProvider::OpencodeGo => vec![ep.opencode_go.clone()],
            SubProvider::Grok => vec![ep.grok_api.clone()],
        }
        .into_iter()
        // Scheme, host and port must all match a configured provider endpoint.
        .filter_map(|b| url::Url::parse(&b).ok().and_then(|u| origin(&u)))
        .collect();
        let target = origin(&parsed).context("invalid url")?;
        anyhow::ensure!(allowed.contains(&target), "url origin {target} is not allowed for {provider} accounts");
        let method = Method::from_bytes(method.to_ascii_uppercase().as_bytes()).context("invalid method")?;

        let mut cred = self.credential(&account.id, None).await?;
        for attempt in 0..2 {
            let token = cred.bearer().to_string();
            let mut req = self.http.request(method.clone(), parsed.clone()).timeout(Duration::from_secs(60));
            for (k, v) in headers {
                if k.eq_ignore_ascii_case("host") {
                    continue;
                }
                req = req.header(k, v.replace("$TOKEN$", &token));
            }
            if let Some(d) = data {
                req = req.body(d.replace("$TOKEN$", &token));
            }
            let resp = req.send().await.context("request failed")?;
            let status = resp.status().as_u16();
            if status == 401 && attempt == 0 && provider.uses_oauth() {
                cred = self.credential(&account.id, Some(&cred.access_token)).await?;
                continue;
            }
            let h = resp.headers().clone();
            let body = resp.text().await.context("failed to read response")?;
            return Ok((status, h, body));
        }
        unreachable!()
    }

    // ---------------------------------------------------------------- login

    pub async fn start_login(self: &Arc<Self>, provider: SubProvider, device: bool) -> Result<FlowStart> {
        let ep = &self.cfg.endpoints;
        match (provider, device) {
            (SubProvider::Codex, true) => {
                let dc = codex::start_device(&self.http, ep).await?;
                let flow_id = self.flows.insert(provider, FlowKind::Device);
                let url = codex::device_verification_url(ep);
                let start = FlowStart {
                    flow_id: flow_id.clone(),
                    provider,
                    url,
                    user_code: Some(dc.user_code.clone()),
                    needs_code: false,
                    instructions: "Open the URL, sign in to ChatGPT and enter the code.".into(),
                };
                let this = self.clone();
                tokio::spawn(async move { this.run_device_flow(flow_id, dc).await });
                Ok(start)
            }
            (SubProvider::Codex, false) | (SubProvider::Claude, _) => {
                let p = oauth::pkce();
                let state = oauth::random_hex(16);
                let (url, redirect_uri, instructions) = if provider == SubProvider::Codex {
                    (
                        codex::authorize_url(ep, &p.challenge, &state),
                        codex::BROWSER_REDIRECT.to_string(),
                        "Open the URL and sign in. The browser then fails to load a localhost page: copy that full URL and paste it here.",
                    )
                } else {
                    (
                        claude::authorize_url(ep, &p.challenge, &state),
                        ep.claude_manual_redirect.clone(),
                        "Open the URL, authorize, and paste the code shown (CODE#STATE).",
                    )
                };
                let flow_id = self.flows.insert(provider, FlowKind::Pkce { verifier: p.verifier, state, redirect_uri });
                Ok(FlowStart { flow_id, provider, url, user_code: None, needs_code: true, instructions: instructions.into() })
            }
            (SubProvider::Grok, _) => {
                let dc = grok::start_device(&self.http, ep).await?;
                let flow_id = self.flows.insert(provider, FlowKind::Device);
                let start = FlowStart {
                    flow_id: flow_id.clone(),
                    provider,
                    url: dc.verification_url.clone(),
                    user_code: Some(dc.user_code.clone()),
                    needs_code: false,
                    instructions: "Open the URL, sign in to your xAI account and confirm the code.".into(),
                };
                let this = self.clone();
                tokio::spawn(async move { this.run_grok_device_flow(flow_id, dc).await });
                Ok(start)
            }
            (SubProvider::OpencodeGo, _) => anyhow::bail!("opencode-go uses API keys; add one with `xolotl accounts add-key`"),
        }
    }

    async fn run_grok_device_flow(self: Arc<Self>, flow_id: String, dc: grok::DeviceCode) {
        let deadline = Instant::now() + Duration::from_secs(dc.expires_in.clamp(60, 15 * 60));
        let mut interval = dc.interval;
        let outcome: Result<AccountRow> = async {
            loop {
                anyhow::ensure!(Instant::now() < deadline, "device login timed out");
                tokio::time::sleep(Duration::from_secs(interval)).await;
                match grok::poll_device(&self.http, &dc).await? {
                    poll @ (grok::DevicePoll::Pending | grok::DevicePoll::SlowDown) => interval = grok::next_interval(interval, &poll),
                    grok::DevicePoll::Done(tr) => {
                        let mut cred = Credential::default();
                        grok::apply_tokens(&mut cred, tr);
                        return self.save_account(cred).await;
                    }
                }
            }
        }
        .await;
        self.flows.set_status(&flow_id, flow_outcome(outcome));
    }

    async fn run_device_flow(self: Arc<Self>, flow_id: String, dc: codex::DeviceCode) {
        let deadline = Instant::now() + Duration::from_secs(15 * 60);
        let ep = self.cfg.endpoints.clone();
        let outcome: Result<AccountRow> = async {
            loop {
                anyhow::ensure!(Instant::now() < deadline, "device login timed out");
                tokio::time::sleep(Duration::from_secs(dc.interval)).await;
                if let Some((code, verifier)) = codex::poll_device(&self.http, &ep, &dc).await? {
                    let tr = codex::exchange_code(&self.http, &ep, &code, &verifier, &codex::device_redirect(&ep)).await?;
                    let mut cred = Credential::default();
                    codex::apply_tokens(&mut cred, tr);
                    return self.save_account(cred).await;
                }
            }
        }
        .await;
        self.flows.set_status(&flow_id, flow_outcome(outcome));
    }

    pub async fn complete_login(&self, flow_id: &str, pasted: &str) -> Result<AccountRow> {
        let flow = self.flows.get(flow_id).context("unknown or expired login flow")?;
        let FlowKind::Pkce { verifier, state, redirect_uri } = &flow.kind else {
            anyhow::bail!("this flow completes on its own; poll its status instead");
        };
        let (code, pasted_state) = oauth::parse_pasted_code(pasted).context("no authorization code found in input")?;
        if let Some(s) = &pasted_state {
            anyhow::ensure!(s == state, "state mismatch: the code belongs to a different login attempt");
        }
        let ep = &self.cfg.endpoints;
        let outcome = async {
            let mut cred = Credential::default();
            match flow.provider {
                SubProvider::Codex => {
                    let tr = codex::exchange_code(&self.http, ep, &code, verifier, redirect_uri).await?;
                    codex::apply_tokens(&mut cred, tr);
                }
                SubProvider::Claude => {
                    let tr = claude::exchange_code(&self.http, ep, &code, verifier, state).await?;
                    claude::apply_tokens(&mut cred, tr);
                    if cred.account_id.is_none() {
                        let _ = claude::fetch_profile(&self.http, ep, &mut cred).await;
                    }
                }
                SubProvider::OpencodeGo => anyhow::bail!("not an OAuth provider"),
                SubProvider::Grok => anyhow::bail!("Grok logins use the device code; poll the flow status instead"),
            }
            self.save_account(cred).await
        }
        .await;
        let status = flow_outcome(outcome.as_ref().map(Clone::clone).map_err(|e| anyhow!("{e:#}")));
        self.flows.set_status(flow_id, status);
        outcome
    }

    pub fn flow_status(&self, flow_id: &str) -> Option<FlowStatus> {
        self.flows.get(flow_id).map(|f| f.status)
    }

    /// Import a credential file (Codex `auth.json`, Claude `.credentials.json`)
    /// or `{ "api_key": ... }` for OpenCode Go.
    pub async fn import(&self, provider: SubProvider, data: &Value, label: Option<String>) -> Result<AccountRow> {
        let mut cred = match provider {
            SubProvider::Codex => codex::import(data)?,
            SubProvider::Claude => {
                let mut c = claude::import(data)?;
                if let Err(e) = claude::fetch_profile(&self.http, &self.cfg.endpoints, &mut c).await {
                    tracing::warn!(error = %e, "could not fetch Claude profile for imported token");
                }
                c
            }
            SubProvider::Grok => anyhow::bail!("importing Grok credentials is not supported; use `xolotl auth login grok`"),
            SubProvider::OpencodeGo => {
                let key = data.get("api_key").and_then(Value::as_str).filter(|k| !k.is_empty()).context("api_key is required")?;
                Credential { provider, api_key: Some(key.to_string()), ..Default::default() }
            }
        };
        cred.label = label.or(cred.label);
        self.save_account(cred).await
    }
}

fn origin(u: &url::Url) -> Option<String> {
    Some(format!("{}://{}:{}", u.scheme(), u.host_str()?, u.port_or_known_default()?))
}

fn flow_outcome(r: Result<AccountRow>) -> FlowStatus {
    match r {
        Ok(a) => FlowStatus::Complete { account_id: a.id, email: a.email },
        Err(e) => FlowStatus::Failed { error: format!("{e:#}") },
    }
}

/// Read the first SSE events of a stream until one is decisive: a content
/// event (stream is healthy) or an error event (returned for classification).
/// Handshake events (`response.created` / `in_progress`) are buffered.
async fn peek_sse(stream: &mut BoxStream<'static, reqwest::Result<Bytes>>) -> (Vec<Bytes>, Option<Value>) {
    const LIMIT: usize = 256 * 1024;
    let mut chunks = Vec::new();
    let mut buf: Vec<u8> = Vec::new();
    let mut scanned = 0;
    while buf.len() < LIMIT {
        let Some(Ok(chunk)) = stream.next().await else { break };
        buf.extend_from_slice(&chunk);
        chunks.push(chunk);
        while let Some(rel) = buf[scanned..].iter().position(|b| *b == b'\n') {
            let line = &buf[scanned..scanned + rel];
            scanned += rel + 1;
            let Some(data) = line.strip_prefix(b"data:") else { continue };
            let Ok(v) = serde_json::from_slice::<Value>(data.trim_ascii()) else { continue };
            match v.get("type").and_then(Value::as_str) {
                Some("response.created" | "response.in_progress") => continue,
                Some("error" | "response.failed") => return (chunks, Some(v)),
                _ => return (chunks, None),
            }
        }
    }
    (chunks, None)
}

/// `IntoResponse` without importing the trait at every call site.
trait IntoResponseCompat {
    fn into_response_compat(self) -> Response;
}

impl<T: axum::response::IntoResponse> IntoResponseCompat for T {
    fn into_response_compat(self) -> Response {
        self.into_response()
    }
}

#[cfg(test)]
mod tests;
