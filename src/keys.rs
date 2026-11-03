//! Client keys and the policies they belong to, stored in OpenBao:
//!
//! - `policies/<policy>`        policy definition ([`crate::policy::Policy`])
//! - `keys/<policy>/<client>`   key record, including the plaintext `api_key`,
//!   so the client can read its own key with its own Vault policy. No
//!   operator ever needs to see it.
//!
//! xolotl keeps only SHA-256 hashes of keys in memory. Writes use KV v2
//! check-and-set so a stale instance can't overwrite a key issued, rotated or
//! revoked elsewhere. Revocation writes a marker as the latest version and
//! destroys every earlier version's data.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use parking_lot::{Mutex, RwLock};
use rand::RngExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::policy::Policy;
use crate::secrets::{Conflict, Secrets};

pub const KEY_PREFIX: &str = "xol_";
const ID_ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";
/// Minimum time between reloads triggered by unknown keys.
const MISS_RELOAD_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// Use the LLM proxy / gateway endpoints.
    Proxy,
    /// CLIProxyAPI-compatible management API (T3Code hubs).
    Management,
    /// The MCP gateway (`/mcp`), limited to the policy's MCP servers and tools.
    Mcp,
    /// Full administration. Implies every other scope.
    Admin,
}

/// Key record as stored in the vault (and read by the client).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct KeyRecord {
    /// Absent in a revocation marker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    pub client_id: String,
    pub policy: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotated_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub revoked: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<DateTime<Utc>>,
    /// After a rotation with grace, the old key's hash and when it stops working.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_key_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_expires_at: Option<DateTime<Utc>>,
}

/// Key metadata safe to show (no key material).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KeyInfo {
    pub client_id: String,
    pub policy: String,
    pub name: String,
    pub description: String,
    pub vault_path: String,
    pub created_at: Option<DateTime<Utc>>,
    pub rotated_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub disabled: bool,
    /// After a rotation with grace: when the old key stops working.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_expires_at: Option<DateTime<Utc>>,
}

/// The authenticated caller.
#[derive(Debug, Clone)]
pub struct Principal {
    /// `None` for the bootstrap admin token.
    pub client_id: Option<String>,
    pub policy: Arc<Policy>,
}

impl Principal {
    pub fn has_scope(&self, scope: Scope) -> bool {
        self.policy.has_scope(scope)
    }
    pub fn client_id(&self) -> Option<&str> {
        self.client_id.as_deref()
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct IssueRequest {
    pub policy: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Lifetime in seconds; none = no expiry.
    #[serde(default)]
    pub ttl_secs: Option<i64>,
}

/// A request to issue a key that can never succeed as sent.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct InvalidIssue(String);

/// What a request to issue a key did. Neither answer holds key material.
#[derive(Debug, Clone, PartialEq)]
pub enum Issued {
    Created(KeyInfo),
    /// A live key has this name. It is unchanged, so this is what the record holds.
    Exists(KeyInfo),
}

impl Issued {
    pub fn info(&self) -> &KeyInfo {
        match self {
            Issued::Created(info) | Issued::Exists(info) => info,
        }
    }
}

/// What a request to revoke a key found. After each answer, no version in the
/// vault holds the key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Revoked {
    Revoked,
    AlreadyRevoked,
    /// No record exists, or its latest version has no data.
    Absent,
}

struct Entry {
    info: KeyInfo,
    revoked: bool,
    hash: Option<String>,
    previous: Option<(String, DateTime<Utc>)>,
}

/// A stored policy that did not load, and why. Its keys get 401 until a
/// valid version is saved.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UnloadedPolicy {
    pub name: String,
    pub error: String,
}

#[derive(Default)]
struct State {
    policies: HashMap<String, Arc<Policy>>,
    /// Stored policies that did not load: name -> why.
    unloaded: BTreeMap<String, String>,
    /// Stored policies in use after migrating them in memory; each is logged once.
    migrated: BTreeSet<String>,
    keys: HashMap<String, Entry>,
    /// key hash -> client id (current and in-grace previous hashes).
    by_hash: HashMap<String, String>,
    bootstrap_hash: Option<String>,
}

pub struct KeyManager {
    secrets: Secrets,
    state: RwLock<State>,
    last_miss_reload: Mutex<Option<Instant>>,
    /// Held by reloads and by every key or policy change, from the vault read
    /// to the cache update, so a reload never publishes a snapshot read before
    /// a change that finished while it was running (e.g. bringing back a
    /// revoked key).
    writes: tokio::sync::Mutex<()>,
}

fn key_path(client_id: &str) -> String {
    format!("keys/{client_id}")
}

/// A policy or key name. It becomes one segment of a vault path, so this also
/// keeps `..`, `/` and `?` out of that path.
pub fn valid_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= 63
        && b[0].is_ascii_alphanumeric()
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-' || *c == b'_')
}

fn is_revocation_marker(record: &serde_json::Map<String, Value>) -> bool {
    record.get("revoked").and_then(Value::as_bool).unwrap_or(false)
}

/// When a key with this lifetime expires. A lifetime too long to compute is
/// refused, since the arithmetic would panic.
fn expiry(now: DateTime<Utc>, ttl_secs: Option<i64>) -> Result<Option<DateTime<Utc>>> {
    let Some(secs) = ttl_secs else { return Ok(None) };
    let at = chrono::TimeDelta::try_seconds(secs).and_then(|ttl| now.checked_add_signed(ttl));
    Ok(Some(at.ok_or_else(|| InvalidIssue(format!("ttl_secs {secs} is out of range")))?))
}

impl KeyManager {
    pub fn new(secrets: Secrets) -> Self {
        Self { secrets, state: RwLock::new(State::default()), last_miss_reload: Mutex::new(None), writes: tokio::sync::Mutex::new(()) }
    }

    /// Reload policies, key records and the bootstrap token from the vault.
    pub async fn reload(&self) -> Result<(usize, usize)> {
        let _writes = self.writes.lock().await;
        let mut fresh = State::default();
        for name in self.secrets.list("policies").await? {
            if name.ends_with('/') {
                continue;
            }
            match self.secrets.get(&format!("policies/{name}")).await {
                Ok(Some(m)) => match crate::policy::from_stored(m) {
                    Ok((p, upgraded)) => {
                        if upgraded {
                            if !self.state.read().migrated.contains(&name) {
                                tracing::warn!(
                                    policy = %name,
                                    rules = p.rules.len(),
                                    accounts = ?p.accounts,
                                    "using a policy from before 0.4.0, migrated in memory: allow_models and deny_models are now rules, with accounts for its subscription providers. Save the policy to store this form"
                                );
                            }
                            fresh.migrated.insert(name.clone());
                        }
                        fresh.policies.insert(p.name.clone(), Arc::new(p));
                    }
                    Err(e) => {
                        tracing::warn!(policy = %name, error = %e, "skipping invalid policy");
                        fresh.unloaded.insert(name, format!("{e:#}"));
                    }
                },
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(policy = %name, error = %e, "skipping unreadable policy");
                    fresh.unloaded.insert(name, format!("{e:#}"));
                }
            }
        }
        for dir in self.secrets.list("keys").await? {
            let Some(policy) = dir.strip_suffix('/') else { continue };
            for client in self.secrets.list(&format!("keys/{policy}")).await? {
                if client.ends_with('/') {
                    continue;
                }
                let id = format!("{policy}/{client}");
                match self.secrets.get(&key_path(&id)).await {
                    Ok(Some(m)) => match serde_json::from_value::<KeyRecord>(Value::Object(m)) {
                        Ok(r) => insert_entry(&mut fresh, &id, &r),
                        Err(e) => tracing::warn!(client = %id, error = %e, "skipping invalid key record"),
                    },
                    Ok(None) => {}
                    Err(e) => tracing::warn!(client = %id, error = %e, "skipping unreadable key record"),
                }
            }
        }
        fresh.bootstrap_hash =
            self.secrets.get("admin").await?.and_then(|m| m.get("token").and_then(Value::as_str).filter(|t| !t.is_empty()).map(sha256_hex));
        let counts = (fresh.policies.len(), fresh.keys.values().filter(|e| !e.revoked).count());
        *self.state.write() = fresh;
        Ok(counts)
    }

    /// Authenticate a presented key. An unknown key triggers a reload (at
    /// most every 10 s) so keys issued by another instance work quickly.
    pub async fn authenticate(&self, presented: &str) -> Option<Principal> {
        let presented = presented.trim();
        if presented.is_empty() {
            return None;
        }
        let hash = sha256_hex(presented);
        if let Some(p) = self.lookup(&hash) {
            return Some(p);
        }
        if !presented.starts_with(KEY_PREFIX) {
            return None;
        }
        let due = {
            let mut last = self.last_miss_reload.lock();
            let due = last.is_none_or(|t| t.elapsed() >= MISS_RELOAD_INTERVAL);
            if due {
                *last = Some(Instant::now());
            }
            due
        };
        if due {
            if let Err(e) = self.reload().await {
                tracing::warn!(error = %e, "reload after unknown key failed");
            }
            return self.lookup(&hash);
        }
        None
    }

    fn lookup(&self, hash: &str) -> Option<Principal> {
        let st = self.state.read();
        if st.bootstrap_hash.as_deref().is_some_and(|b| ct_eq(b.as_bytes(), hash.as_bytes())) {
            return Some(Principal { client_id: None, policy: Arc::new(Policy::unrestricted()) });
        }
        let id = st.by_hash.get(hash)?;
        let e = st.keys.get(id)?;
        let now = Utc::now();
        if e.revoked || e.info.disabled || e.info.expires_at.is_some_and(|x| x <= now) {
            return None;
        }
        let current = e.hash.as_deref().is_some_and(|h| ct_eq(h.as_bytes(), hash.as_bytes()));
        let previous = e.previous.as_ref().is_some_and(|(h, until)| *until > now && ct_eq(h.as_bytes(), hash.as_bytes()));
        if !(current || previous) {
            return None;
        }
        // Keys whose policy is missing fail closed.
        let Some(policy) = st.policies.get(&e.info.policy).cloned() else {
            tracing::warn!(client = %id, policy = %e.info.policy, "refusing a valid key: its policy is not loaded");
            return None;
        };
        Some(Principal { client_id: Some(id.clone()), policy })
    }

    // ---- policies

    pub fn policies(&self) -> Vec<Arc<Policy>> {
        let mut v: Vec<_> = self.state.read().policies.values().cloned().collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    pub fn policy(&self, name: &str) -> Option<Arc<Policy>> {
        self.state.read().policies.get(name).cloned()
    }

    /// Stored policies that did not load, by name.
    pub fn unloaded_policies(&self) -> Vec<UnloadedPolicy> {
        self.state.read().unloaded.iter().map(|(name, error)| UnloadedPolicy { name: name.clone(), error: error.clone() }).collect()
    }

    /// Why a stored policy did not load.
    pub fn unloaded_policy(&self, name: &str) -> Option<String> {
        self.state.read().unloaded.get(name).cloned()
    }

    pub async fn put_policy(&self, mut policy: Policy, known: &BTreeSet<String>, mcp: &BTreeSet<String>) -> Result<Arc<Policy>> {
        policy.normalize();
        policy.validate(known, mcp)?;
        let _writes = self.writes.lock().await;
        let data = match serde_json::to_value(&policy)? {
            Value::Object(m) => m,
            _ => unreachable!("policy serializes to an object"),
        };
        self.secrets.put(&format!("policies/{}", policy.name), data).await?;
        let p = Arc::new(policy);
        let mut st = self.state.write();
        st.unloaded.remove(&p.name);
        st.policies.insert(p.name.clone(), p.clone());
        Ok(p)
    }

    pub async fn delete_policy(&self, name: &str) -> Result<()> {
        let _writes = self.writes.lock().await;
        let in_use = self.state.read().keys.values().any(|e| !e.revoked && e.info.policy == name);
        anyhow::ensure!(!in_use, "policy {name} still has keys; revoke them first");
        self.secrets.delete(&format!("policies/{name}")).await?;
        let mut st = self.state.write();
        st.unloaded.remove(name);
        st.policies.remove(name);
        Ok(())
    }

    // ---- keys

    pub fn list(&self) -> Vec<KeyInfo> {
        let mut v: Vec<_> = self.state.read().keys.values().filter(|e| !e.revoked).map(|e| e.info.clone()).collect();
        v.sort_by(|a, b| a.client_id.cmp(&b.client_id));
        v
    }

    pub fn get(&self, client_id: &str) -> Option<KeyInfo> {
        self.state.read().keys.get(client_id).filter(|e| !e.revoked).map(|e| e.info.clone())
    }

    /// Issue a key. The key is written to the vault only; the response shows
    /// where the client reads it. A repeat finds the live key and changes
    /// nothing, also when it asks for another description or lifetime, so a
    /// caller that lost the answer can safely send it again.
    pub async fn issue(&self, req: IssueRequest) -> Result<Issued> {
        if !valid_name(&req.name) {
            return Err(InvalidIssue("key name must be lowercase letters, digits, '-' or '_' (max 63)".into()).into());
        }
        let now = Utc::now();
        let expires_at = expiry(now, req.ttl_secs)?;
        let _writes = self.writes.lock().await;
        // Checked under the lock, so a concurrent delete_policy can't slip in between.
        if self.policy(&req.policy).is_none() {
            return Err(InvalidIssue(format!("policy {:?} does not exist", req.policy)).into());
        }
        let record = KeyRecord {
            api_key: None,
            client_id: format!("{}/{}", req.policy, req.name),
            policy: req.policy,
            name: req.name,
            description: req.description.trim().to_string(),
            created_at: Some(now),
            expires_at,
            ..Default::default()
        };
        match self.issue_locked(&record).await {
            // Another instance wrote the name after the read: see what it wrote.
            Err(e) if e.is::<Conflict>() => self.issue_locked(&record).await,
            result => result,
        }
    }

    async fn issue_locked(&self, template: &KeyRecord) -> Result<Issued> {
        let id = &template.client_id;
        let (existing, version) = self.secrets.get_versioned(&key_path(id)).await?;
        if let Some(existing) = existing.filter(|m| !is_revocation_marker(m)) {
            let record: KeyRecord = serde_json::from_value(Value::Object(existing)).with_context(|| format!("reading key {id}"))?;
            self.cache(id, &record);
            return Ok(Issued::Exists(self.get(id).context("existing key missing from cache")?));
        }
        let record = KeyRecord { api_key: Some(new_key()), ..template.clone() };
        // CAS against the version we saw (0 = new, or the revocation marker).
        self.write(id, &record, version).await?;
        Ok(Issued::Created(self.get(id).context("issued key missing from cache")?))
    }

    /// Replace a key. The old key keeps working for `grace_secs`.
    pub async fn rotate(&self, client_id: &str, grace_secs: i64) -> Result<KeyInfo> {
        let _writes = self.writes.lock().await;
        let (mut record, version) = self.fetch(client_id).await?;
        let now = Utc::now();
        let old = record.api_key.replace(new_key());
        if grace_secs > 0 {
            record.previous_key_sha256 = old.as_deref().map(sha256_hex);
            record.previous_expires_at = Some(now + chrono::Duration::seconds(grace_secs));
        } else {
            record.previous_key_sha256 = None;
            record.previous_expires_at = None;
        }
        record.rotated_at = Some(now);
        self.write(client_id, &record, version).await?;
        self.get(client_id).context("rotated key missing from cache")
    }

    pub async fn set_disabled(&self, client_id: &str, disabled: bool) -> Result<KeyInfo> {
        let _writes = self.writes.lock().await;
        let (mut record, version) = self.fetch(client_id).await?;
        record.disabled = disabled;
        self.write(client_id, &record, version).await?;
        self.get(client_id).context("key missing from cache")
    }

    /// Write a revocation marker as the latest version and destroy the data
    /// of every earlier version. Metadata stays so versions never repeat.
    ///
    /// A key that is already revoked or gone answers without an error, and
    /// the destroy runs again: an earlier attempt may have written the marker
    /// and failed before it. The answer is an error whenever a version may
    /// still hold the key.
    pub async fn revoke(&self, client_id: &str) -> Result<Revoked> {
        let (policy, name) = client_id.split_once('/').with_context(|| format!("key id {client_id:?} must be <policy>/<name>"))?;
        let marker = KeyRecord {
            client_id: client_id.to_string(),
            policy: policy.to_string(),
            name: name.to_string(),
            revoked: true,
            revoked_at: Some(Utc::now()),
            ..Default::default()
        };
        let _writes = self.writes.lock().await;
        match self.revoke_locked(&marker).await {
            // Another instance changed the record after the read: look again.
            Err(e) if e.is::<Conflict>() => self.revoke_locked(&marker).await,
            result => result,
        }
    }

    async fn revoke_locked(&self, marker: &KeyRecord) -> Result<Revoked> {
        let id = &marker.client_id;
        let path = key_path(id);
        let (data, version) = self.secrets.get_versioned(&path).await?;
        let Some(data) = data else {
            // A deleted latest version keeps its data until destroyed, and so
            // do the versions before it.
            self.forget(id);
            let all: Vec<u64> = (1..=version).collect();
            self.secrets.destroy_versions(&path, &all).await?;
            return Ok(Revoked::Absent);
        };
        let (answer, newest) = if is_revocation_marker(&data) {
            self.cache(id, marker);
            (Revoked::AlreadyRevoked, version)
        } else {
            (Revoked::Revoked, self.write(id, marker, version).await?)
        };
        let old: Vec<u64> = (1..newest).collect();
        self.secrets.destroy_versions(&path, &old).await?;
        Ok(answer)
    }

    async fn fetch(&self, client_id: &str) -> Result<(KeyRecord, u64)> {
        let (data, version) = self.secrets.get_versioned(&key_path(client_id)).await?;
        let record: KeyRecord = serde_json::from_value(Value::Object(data.with_context(|| format!("key {client_id} not found"))?))?;
        anyhow::ensure!(!record.revoked, "key {client_id} is revoked");
        Ok((record, version))
    }

    async fn write(&self, client_id: &str, record: &KeyRecord, cas: u64) -> Result<u64> {
        let data = match serde_json::to_value(record)? {
            Value::Object(m) => m,
            _ => unreachable!("record serializes to an object"),
        };
        let version = self.secrets.put_cas(&key_path(client_id), data, cas).await?;
        self.cache(client_id, record);
        Ok(version)
    }

    /// Replace what the cache knows about a key with what the vault holds.
    fn cache(&self, client_id: &str, record: &KeyRecord) {
        let mut st = self.state.write();
        remove_entry(&mut st, client_id);
        insert_entry(&mut st, client_id, record);
    }

    fn forget(&self, client_id: &str) {
        remove_entry(&mut self.state.write(), client_id);
    }
}

fn remove_entry(st: &mut State, client_id: &str) {
    if st.keys.remove(client_id).is_some() {
        st.by_hash.retain(|_, id| id != client_id);
    }
}

fn insert_entry(st: &mut State, id: &str, r: &KeyRecord) {
    let hash = r.api_key.as_deref().map(sha256_hex);
    let previous = match (&r.previous_key_sha256, r.previous_expires_at) {
        (Some(h), Some(until)) => Some((h.clone(), until)),
        _ => None,
    };
    if !r.revoked {
        for h in hash.iter().chain(previous.as_ref().map(|(h, _)| h)) {
            st.by_hash.insert(h.clone(), id.to_string());
        }
    }
    st.keys.insert(
        id.to_string(),
        Entry {
            info: KeyInfo {
                client_id: id.to_string(),
                policy: r.policy.clone(),
                name: r.name.clone(),
                description: r.description.clone(),
                vault_path: key_path(id),
                created_at: r.created_at,
                rotated_at: r.rotated_at,
                expires_at: r.expires_at,
                disabled: r.disabled,
                previous_expires_at: previous.as_ref().map(|(_, until)| *until),
            },
            revoked: r.revoked,
            hash,
            previous,
        },
    );
}

fn new_key() -> String {
    let bytes: [u8; 32] = rand::rng().random();
    format!("{KEY_PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes))
}

pub fn random_id(len: usize) -> String {
    let mut rng = rand::rng();
    (0..len).map(|_| ID_ALPHABET[rng.random_range(0..ID_ALPHABET.len())] as char).collect()
}

pub fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

/// Constant-time comparison of equal-length secrets.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// `*` matches any run of characters (including `/`).
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == text;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !text.starts_with(first) || text.len() < first.len() + last.len() || !text.ends_with(last) {
        return false;
    }
    let mut rest = &text[first.len()..text.len() - last.len()];
    for mid in &parts[1..parts.len() - 1] {
        match rest.find(mid) {
            Some(i) => rest = &rest[i + mid.len()..],
            None => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::MemoryStore;
    use serde_json::json;

    fn known() -> BTreeSet<String> {
        ["claude", "codex"].iter().map(|s| s.to_string()).collect()
    }

    async fn manager() -> (KeyManager, Secrets) {
        let s: Secrets = Arc::new(MemoryStore::default());
        let km = KeyManager::new(s.clone());
        km.put_policy(Policy { name: "dev".into(), providers: vec!["claude".into()], ..Default::default() }, &known(), &BTreeSet::new())
            .await
            .unwrap();
        (km, s)
    }

    async fn vault_key(s: &Secrets, id: &str) -> String {
        s.get(&format!("keys/{id}")).await.unwrap().unwrap()["api_key"].as_str().unwrap().to_string()
    }

    #[test]
    fn globs() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("claude-*", "claude-sonnet-4-5"));
        assert!(glob_match("vercel/*/gpt-*", "vercel/openai/gpt-5"));
        assert!(!glob_match("vercel/*", "mistral/x"));
        assert!(glob_match("a*b*c", "aXXbYYc"));
        assert!(!glob_match("a*b*c", "aXXcYYb"));
        assert!(!glob_match("ab*ba", "aba"));
    }

    #[tokio::test]
    async fn issue_authenticate_rotate_revoke() {
        let (km, s) = manager().await;
        assert!(km.issue(IssueRequest { policy: "nope".into(), name: "a".into(), ..Default::default() }).await.is_err());
        let info = km.issue(IssueRequest { policy: "dev".into(), name: "devcontainer".into(), ..Default::default() }).await.unwrap();
        assert!(matches!(info, Issued::Created(_)));
        assert_eq!(info.info().client_id, "dev/devcontainer");
        assert_eq!(info.info().vault_path, "keys/dev/devcontainer");

        let key = vault_key(&s, "dev/devcontainer").await;
        assert!(key.starts_with("xol_"));
        let p = km.authenticate(&key).await.expect("valid key");
        assert_eq!(p.client_id(), Some("dev/devcontainer"));
        assert!(p.has_scope(Scope::Proxy) && !p.has_scope(Scope::Admin));
        assert!(km.authenticate(&format!("{key}x")).await.is_none());

        km.rotate("dev/devcontainer", 60).await.unwrap();
        let key2 = vault_key(&s, "dev/devcontainer").await;
        assert_ne!(key, key2);
        assert!(km.authenticate(&key2).await.is_some());
        assert!(km.authenticate(&key).await.is_some(), "old key valid during grace");
        km.rotate("dev/devcontainer", 0).await.unwrap();
        assert!(km.authenticate(&key2).await.is_none());

        let key3 = vault_key(&s, "dev/devcontainer").await;
        km.set_disabled("dev/devcontainer", true).await.unwrap();
        assert!(km.authenticate(&key3).await.is_none());
        km.set_disabled("dev/devcontainer", false).await.unwrap();

        km.revoke("dev/devcontainer").await.unwrap();
        assert!(km.authenticate(&key3).await.is_none());
        let marker = s.get("keys/dev/devcontainer").await.unwrap().unwrap();
        assert_eq!(marker["revoked"], true);
        assert!(marker.get("api_key").is_none());
        assert!(km.list().is_empty());
        assert_eq!(km.revoke("dev/devcontainer").await.unwrap(), Revoked::AlreadyRevoked);
        // Re-issuing after revocation is allowed.
        km.issue(IssueRequest { policy: "dev".into(), name: "devcontainer".into(), ..Default::default() }).await.unwrap();
        assert!(km.delete_policy("dev").await.is_err(), "policy with keys can't be deleted");
    }

    fn req(name: &str) -> IssueRequest {
        IssueRequest { policy: "dev".into(), name: name.into(), ..Default::default() }
    }

    async fn gated() -> (Arc<crate::secrets::testing::GatedStore>, Secrets, Arc<KeyManager>) {
        let store = Arc::new(crate::secrets::testing::GatedStore::default());
        let s: Secrets = store.clone();
        let km = Arc::new(KeyManager::new(s.clone()));
        km.put_policy(Policy { name: "dev".into(), providers: vec!["claude".into()], ..Default::default() }, &known(), &BTreeSet::new())
            .await
            .unwrap();
        (store, s, km)
    }

    #[tokio::test]
    async fn repeated_issue_reports_the_key_and_never_changes_it() {
        let (km, s) = manager().await;
        let first = km.issue(IssueRequest { description: "first".into(), ..req("a") }).await.unwrap();
        let key = vault_key(&s, "dev/a").await;
        let (record, version) = s.get_versioned("keys/dev/a").await.unwrap();

        let again = km.issue(IssueRequest { description: "second".into(), ttl_secs: Some(60), ..req("a") }).await.unwrap();

        assert_eq!(again, Issued::Exists(first.info().clone()), "reports the stored key, not the request");
        assert_eq!(s.get_versioned("keys/dev/a").await.unwrap(), (record, version), "nothing written");
        assert_eq!(vault_key(&s, "dev/a").await, key);
        assert!(km.authenticate(&key).await.is_some());
    }

    #[tokio::test]
    async fn repeated_issue_reports_a_disabled_or_expired_key_as_it_is() {
        let (km, s) = manager().await;
        km.issue(req("a")).await.unwrap();
        km.set_disabled("dev/a", true).await.unwrap();
        let Issued::Exists(info) = km.issue(req("a")).await.unwrap() else { panic!("disabled key is not new") };
        assert!(info.disabled);

        km.issue(req("b")).await.unwrap();
        let mut record = s.get("keys/dev/b").await.unwrap().unwrap();
        record.insert("expires_at".into(), json!("2000-01-01T00:00:00Z"));
        s.put("keys/dev/b", record).await.unwrap();
        let Issued::Exists(info) = km.issue(req("b")).await.unwrap() else { panic!("expired key is not new") };
        assert!(info.expires_at.is_some_and(|at| at < Utc::now()));
    }

    #[tokio::test]
    async fn repeated_issue_refuses_a_record_it_cannot_read_and_leaves_it() {
        let (km, s) = manager().await;
        s.put("keys/dev/a", json!({"api_key": "xol_old"}).as_object().unwrap().clone()).await.unwrap();
        assert!(km.issue(req("a")).await.is_err());
        assert_eq!(vault_key(&s, "dev/a").await, "xol_old");
    }

    #[tokio::test]
    async fn issue_after_a_lost_answer_finds_the_key_it_wrote() {
        let (store, s, km) = gated().await;
        store.lose_put_answer.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(km.issue(req("a")).await.is_err());
        let key = vault_key(&s, "dev/a").await;

        assert!(matches!(km.issue(req("a")).await.unwrap(), Issued::Exists(_)));
        assert_eq!(vault_key(&s, "dev/a").await, key, "not rotated");
        assert!(km.authenticate(&key).await.is_some(), "the answer also fills the cache");
    }

    #[tokio::test]
    async fn issue_that_loses_a_race_with_another_instance_reports_exists() {
        let (store, s, km) = gated().await;
        let other = KeyManager::new(s.clone());
        other.reload().await.unwrap();

        // This instance has read version 0, and pauses before its write.
        store.arm("put", "keys/dev/a");
        let held = tokio::spawn({
            let km = km.clone();
            async move { km.issue(req("a")).await }
        });
        store.reached.notified().await;
        assert!(matches!(other.issue(req("a")).await.unwrap(), Issued::Created(_)));
        let winner = vault_key(&s, "dev/a").await;
        store.release.notify_one();

        assert!(matches!(held.await.unwrap().unwrap(), Issued::Exists(_)));
        assert_eq!(vault_key(&s, "dev/a").await, winner, "the winner's key stays");
        assert!(km.authenticate(&winner).await.is_some());
    }

    #[tokio::test]
    async fn issue_refuses_a_lifetime_it_cannot_compute() {
        let (km, s) = manager().await;
        for ttl in [i64::MAX, i64::MIN, i64::MAX / 1000 + 1] {
            let e = km.issue(IssueRequest { ttl_secs: Some(ttl), ..req("a") }).await.unwrap_err();
            assert!(e.is::<InvalidIssue>(), "{ttl}: {e:#}");
        }
        assert!(s.get("keys/dev/a").await.unwrap().is_none());
        let issued = km.issue(req("a")).await.unwrap();
        assert_eq!(issued.info().expires_at, None, "no lifetime means no expiry");
    }

    #[tokio::test]
    async fn revoke_of_a_missing_key_is_safe_and_writes_nothing() {
        let (store, s, km) = gated().await;
        assert_eq!(km.revoke("dev/never").await.unwrap(), Revoked::Absent);
        assert_eq!(s.get_versioned("keys/dev/never").await.unwrap(), (None, 0));
        assert_eq!(*store.destroyed.lock(), vec![("keys/dev/never".to_string(), vec![])]);
    }

    #[tokio::test]
    async fn revoke_of_a_deleted_record_destroys_what_it_kept() {
        let (store, _s, km) = gated().await;
        km.issue(req("a")).await.unwrap();
        km.rotate("dev/a", 0).await.unwrap();
        let key = vault_key(&_s, "dev/a").await;
        // Deleted behind this instance's back: the cache still has the key.
        store.soft_delete("keys/dev/a");
        assert!(km.authenticate(&key).await.is_some());

        assert_eq!(km.revoke("dev/a").await.unwrap(), Revoked::Absent);
        assert_eq!(*store.destroyed.lock(), vec![("keys/dev/a".to_string(), vec![1, 2])]);
        assert!(km.authenticate(&key).await.is_none(), "stops at once");
    }

    #[tokio::test]
    async fn revoke_again_finishes_a_destroy_that_failed() {
        let (store, s, km) = gated().await;
        km.issue(req("a")).await.unwrap();
        km.rotate("dev/a", 0).await.unwrap();
        let key = vault_key(&s, "dev/a").await;

        store.fail_destroys.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(km.revoke("dev/a").await.is_err(), "versions may still hold the key");
        assert!(km.authenticate(&key).await.is_none(), "the marker already stopped the key");

        store.fail_destroys.store(false, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(km.revoke("dev/a").await.unwrap(), Revoked::AlreadyRevoked);
        assert_eq!(*store.destroyed.lock(), vec![("keys/dev/a".to_string(), vec![1, 2])], "every version before the marker");

        assert_eq!(km.revoke("dev/a").await.unwrap(), Revoked::AlreadyRevoked);
        assert!(s.get("keys/dev/a").await.unwrap().unwrap().get("api_key").is_none());
    }

    #[tokio::test]
    async fn revoke_after_a_lost_answer_stops_the_key_here() {
        let (store, s, km) = gated().await;
        km.issue(req("a")).await.unwrap();
        let key = vault_key(&s, "dev/a").await;

        store.lose_put_answer.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(km.revoke("dev/a").await.is_err());
        assert!(km.authenticate(&key).await.is_some(), "this instance has not seen the marker yet");

        assert_eq!(km.revoke("dev/a").await.unwrap(), Revoked::AlreadyRevoked);
        assert!(km.authenticate(&key).await.is_none());
    }

    #[tokio::test]
    async fn revoke_stops_a_key_another_instance_already_revoked() {
        let (_store, s, km) = gated().await;
        km.issue(req("a")).await.unwrap();
        let key = vault_key(&s, "dev/a").await;
        let other = KeyManager::new(s.clone());
        other.reload().await.unwrap();
        other.revoke("dev/a").await.unwrap();
        assert!(km.authenticate(&key).await.is_some(), "not reloaded yet");

        assert_eq!(km.revoke("dev/a").await.unwrap(), Revoked::AlreadyRevoked);
        assert!(km.authenticate(&key).await.is_none());
    }

    #[tokio::test]
    async fn revoke_that_loses_a_race_looks_again() {
        let (store, s, km) = gated().await;
        km.issue(req("a")).await.unwrap();
        let other = KeyManager::new(s.clone());
        other.reload().await.unwrap();

        store.arm("put", "keys/dev/a");
        let held = tokio::spawn({
            let km = km.clone();
            async move { km.revoke("dev/a").await }
        });
        store.reached.notified().await;
        other.rotate("dev/a", 0).await.unwrap();
        store.release.notify_one();

        assert_eq!(held.await.unwrap().unwrap(), Revoked::Revoked, "the rotated key is revoked too");
        assert!(s.get("keys/dev/a").await.unwrap().unwrap().get("api_key").is_none());
    }

    #[tokio::test]
    async fn revoke_removes_a_record_that_does_not_parse() {
        let (km, s) = manager().await;
        s.put("keys/dev/a", json!({"api_key": "xol_old"}).as_object().unwrap().clone()).await.unwrap();
        assert_eq!(km.revoke("dev/a").await.unwrap(), Revoked::Revoked);
        assert!(s.get("keys/dev/a").await.unwrap().unwrap().get("api_key").is_none());
    }

    #[tokio::test]
    async fn cas_rejects_stale_writers() {
        let (km, s) = manager().await;
        km.issue(IssueRequest { policy: "dev".into(), name: "a".into(), ..Default::default() }).await.unwrap();
        // Another instance rotates behind our back.
        let other = KeyManager::new(s.clone());
        other.reload().await.unwrap();
        other.rotate("dev/a", 0).await.unwrap();
        // Our fetch reads the latest version, so rotate succeeds; a raw stale CAS fails.
        let (_, v) = s.get_versioned("keys/dev/a").await.unwrap();
        assert!(s.put_cas("keys/dev/a", json!({}).as_object().unwrap().clone(), v - 1).await.is_err());
    }

    #[tokio::test]
    async fn unknown_keys_trigger_reload_and_bootstrap_works() {
        let (km, s) = manager().await;
        let other = KeyManager::new(s.clone());
        other.reload().await.unwrap();
        other.issue(IssueRequest { policy: "dev".into(), name: "b".into(), ..Default::default() }).await.unwrap();
        let key = vault_key(&s, "dev/b").await;
        assert!(km.authenticate(&key).await.is_some(), "picked up via reload-on-miss");
        s.put("admin", json!({"token": "boot"}).as_object().unwrap().clone()).await.unwrap();
        km.reload().await.unwrap();
        let p = km.authenticate("boot").await.unwrap();
        assert!(p.client_id().is_none() && p.has_scope(Scope::Admin));
    }

    #[tokio::test]
    async fn reload_does_not_restore_a_revoked_key() {
        let store = Arc::new(crate::secrets::testing::GatedStore::default());
        let s: Secrets = store.clone();
        let km = Arc::new(KeyManager::new(s.clone()));
        km.put_policy(Policy { name: "dev".into(), providers: vec!["claude".into()], ..Default::default() }, &known(), &BTreeSet::new())
            .await
            .unwrap();
        km.issue(IssueRequest { policy: "dev".into(), name: "a".into(), ..Default::default() }).await.unwrap();
        let key = vault_key(&s, "dev/a").await;

        // A reload reads the live key, then pauses on its last read.
        store.arm("get", "admin");
        let reload = tokio::spawn({
            let km = km.clone();
            async move { km.reload().await.unwrap() }
        });
        store.reached.notified().await;
        // Revoke while the reload holds its stale snapshot.
        let revoke = tokio::spawn({
            let km = km.clone();
            async move { km.revoke("dev/a").await.unwrap() }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        store.release.notify_one();
        reload.await.unwrap();
        revoke.await.unwrap();
        assert!(km.authenticate(&key).await.is_none(), "revoked key must stay revoked");
    }

    #[tokio::test]
    async fn issue_waits_for_a_queued_policy_delete() {
        let store = Arc::new(crate::secrets::testing::GatedStore::default());
        let s: Secrets = store.clone();
        let km = Arc::new(KeyManager::new(s.clone()));
        km.put_policy(Policy { name: "dev".into(), providers: vec!["claude".into()], ..Default::default() }, &known(), &BTreeSet::new())
            .await
            .unwrap();

        // A paused reload holds the lock; a policy delete queues, then an issue.
        store.arm("get", "admin");
        let reload = tokio::spawn({
            let km = km.clone();
            async move { km.reload().await.unwrap() }
        });
        store.reached.notified().await;
        let delete = tokio::spawn({
            let km = km.clone();
            async move { km.delete_policy("dev").await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        let issue = tokio::spawn({
            let km = km.clone();
            async move { km.issue(IssueRequest { policy: "dev".into(), name: "a".into(), ..Default::default() }).await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        store.release.notify_one();
        reload.await.unwrap();
        delete.await.unwrap().unwrap();
        assert!(issue.await.unwrap().is_err(), "no key is issued under a deleted policy");
        assert!(s.get("keys/dev/a").await.unwrap().is_none());
    }

    /// A policy record exactly as a version before 0.4.0 stored it.
    fn legacy_record(name: &str) -> serde_json::Map<String, Value> {
        json!({
            "name": name,
            "providers": ["claude", "codex", "vercel"],
            "allow_models": ["claude-*", "gpt-*"],
            "deny_models": ["claude-opus-*"],
            "scopes": ["proxy"],
        })
        .as_object()
        .unwrap()
        .clone()
    }

    /// A key under `dev`, whose stored policy is then replaced by `record`.
    async fn key_under_stored(km: &KeyManager, s: &Secrets, record: serde_json::Map<String, Value>) -> String {
        km.issue(IssueRequest { policy: "dev".into(), name: "a".into(), ..Default::default() }).await.unwrap();
        s.put("policies/dev", record).await.unwrap();
        vault_key(s, "dev/a").await
    }

    #[tokio::test]
    async fn reload_migrates_an_old_policy_without_writing_it() {
        let (km, s) = manager().await;
        let record = legacy_record("dev");
        let key = key_under_stored(&km, &s, record.clone()).await;
        let version = s.get_versioned("policies/dev").await.unwrap().1;
        km.reload().await.unwrap();

        let p = km.authenticate(&key).await.expect("the key keeps working").policy;
        assert!(p.allows_model("claude", "claude-haiku-5-5"));
        assert!(!p.allows_model("claude", "claude-opus-5"), "deny still wins");
        assert!(!p.allows_model("vercel", "kimi-k3"), "still outside the allow list");
        assert!(p.allows_account("claude", "claude-a1") && p.allows_account("codex", "codex-a1"), "every account, as before");
        assert!(km.unloaded_policies().is_empty());

        // A reload that wrote could restore a stale record over one that another
        // instance deleted and recreated: their version numbers can be the same.
        let (stored, after) = s.get_versioned("policies/dev").await.unwrap();
        assert_eq!((stored.unwrap(), after), (record, version), "the record stays as it is");

        km.reload().await.unwrap();
        assert!(km.authenticate(&key).await.is_some(), "and every later reload migrates it again");
    }

    #[tokio::test]
    async fn saving_a_migrated_policy_stores_the_current_form() {
        let (km, s) = manager().await;
        let key = key_under_stored(&km, &s, legacy_record("dev")).await;
        km.reload().await.unwrap();
        let migrated = km.policy("dev").unwrap().as_ref().clone();
        km.put_policy(migrated.clone(), &["claude", "codex", "vercel"].iter().map(|p| p.to_string()).collect(), &BTreeSet::new())
            .await
            .unwrap();

        let stored = s.get("policies/dev").await.unwrap().unwrap();
        assert!(!stored.contains_key("allow_models") && !stored.contains_key("deny_models"));
        let parsed: Policy = serde_json::from_value(Value::Object(stored)).unwrap();
        assert_eq!(parsed, migrated);
        km.reload().await.unwrap();
        assert!(km.state.read().migrated.is_empty(), "no longer migrated on load");
        assert_eq!(km.authenticate(&key).await.unwrap().policy.as_ref(), &migrated, "same effect");
    }

    #[tokio::test]
    async fn a_current_policy_without_accounts_keeps_none() {
        let (km, s) = manager().await;
        let record = json!({"name": "dev", "providers": ["*"], "scopes": ["proxy"]}).as_object().unwrap().clone();
        let key = key_under_stored(&km, &s, record.clone()).await;
        km.reload().await.unwrap();
        let p = km.authenticate(&key).await.unwrap().policy;
        assert!(!p.allows_account("claude", "claude-a1"), "not widened");
        assert_eq!(s.get("policies/dev").await.unwrap().unwrap(), record, "not rewritten");
    }

    #[tokio::test]
    async fn a_policy_that_does_not_load_is_reported_until_it_is_replaced() {
        let (km, s) = manager().await;
        let mut bad = legacy_record("dev");
        bad.insert("rules".into(), json!([]));
        let key = key_under_stored(&km, &s, bad.clone()).await;
        s.put("policies/typo", json!({"name": "typo", "providerz": []}).as_object().unwrap().clone()).await.unwrap();
        km.reload().await.unwrap();

        assert!(km.policy("dev").is_none() && km.authenticate(&key).await.is_none(), "fails closed");
        let unloaded = km.unloaded_policies();
        assert_eq!(unloaded.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), vec!["dev", "typo"]);
        assert!(unloaded[0].error.contains("can't be combined with rules"), "{}", unloaded[0].error);
        assert!(unloaded[1].error.contains("providerz"), "{}", unloaded[1].error);
        assert_eq!(s.get("policies/dev").await.unwrap().unwrap(), bad, "an invalid record is left as it is");

        // Replacing or deleting one clears it at once, without a reload.
        km.put_policy(Policy { name: "dev".into(), providers: vec!["claude".into()], ..Default::default() }, &known(), &BTreeSet::new())
            .await
            .unwrap();
        assert!(km.authenticate(&key).await.is_some());
        km.delete_policy("typo").await.unwrap();
        assert!(km.unloaded_policies().is_empty());

        // A later reload finds it again if the record is still bad.
        s.put("policies/dev", bad).await.unwrap();
        km.reload().await.unwrap();
        assert_eq!(km.unloaded_policy("dev").as_deref().map(|e| e.contains("rules")), Some(true));
        s.put("policies/dev", legacy_record("dev")).await.unwrap();
        km.reload().await.unwrap();
        assert!(km.unloaded_policies().is_empty(), "a repair in the vault clears it at the next reload");
    }

    #[tokio::test]
    async fn an_unreadable_policy_is_reported() {
        struct Unreadable(MemoryStore);
        #[async_trait::async_trait]
        impl crate::secrets::SecretStore for Unreadable {
            async fn get(&self, key: &str) -> Result<Option<serde_json::Map<String, Value>>> {
                anyhow::ensure!(key != "policies/dev", "vault read denied");
                self.0.get(key).await
            }
            async fn put(&self, key: &str, data: serde_json::Map<String, Value>) -> Result<()> {
                self.0.put(key, data).await
            }
            async fn delete(&self, key: &str) -> Result<()> {
                self.0.delete(key).await
            }
            async fn list(&self, dir: &str) -> Result<Vec<String>> {
                self.0.list(dir).await
            }
            async fn get_versioned(&self, key: &str) -> Result<(Option<serde_json::Map<String, Value>>, u64)> {
                self.0.get_versioned(key).await
            }
            async fn put_cas(&self, key: &str, data: serde_json::Map<String, Value>, cas: u64) -> Result<u64> {
                self.0.put_cas(key, data, cas).await
            }
            async fn destroy_versions(&self, key: &str, versions: &[u64]) -> Result<()> {
                self.0.destroy_versions(key, versions).await
            }
        }
        let s: Secrets = Arc::new(Unreadable(MemoryStore::default()));
        s.put("policies/dev", legacy_record("dev")).await.unwrap();
        let km = KeyManager::new(s);
        km.reload().await.unwrap();
        assert_eq!(km.unloaded_policy("dev").as_deref(), Some("vault read denied"));
    }

    #[tokio::test]
    async fn expired_keys_rejected() {
        let (km, s) = manager().await;
        km.issue(IssueRequest { policy: "dev".into(), name: "a".into(), ttl_secs: Some(-1), ..Default::default() }).await.unwrap();
        assert!(km.authenticate(&vault_key(&s, "dev/a").await).await.is_none());
    }
}
