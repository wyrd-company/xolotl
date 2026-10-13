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

use std::collections::{BTreeSet, HashMap};
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
use crate::secrets::Secrets;

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

struct Entry {
    info: KeyInfo,
    revoked: bool,
    hash: Option<String>,
    previous: Option<(String, DateTime<Utc>)>,
}

#[derive(Default)]
struct State {
    policies: HashMap<String, Arc<Policy>>,
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

fn valid_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= 63
        && b[0].is_ascii_alphanumeric()
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-' || *c == b'_')
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
                Ok(Some(m)) => match serde_json::from_value::<Policy>(Value::Object(m)) {
                    Ok(mut p) => {
                        p.normalize();
                        fresh.policies.insert(p.name.clone(), Arc::new(p));
                    }
                    Err(e) => tracing::warn!(policy = %name, error = %e, "skipping invalid policy"),
                },
                Ok(None) => {}
                Err(e) => tracing::warn!(policy = %name, error = %e, "skipping unreadable policy"),
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
        let policy = st.policies.get(&e.info.policy)?.clone();
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

    pub async fn put_policy(&self, mut policy: Policy, known: &BTreeSet<String>) -> Result<Arc<Policy>> {
        policy.normalize();
        policy.validate(known)?;
        let _writes = self.writes.lock().await;
        let data = match serde_json::to_value(&policy)? {
            Value::Object(m) => m,
            _ => unreachable!("policy serializes to an object"),
        };
        self.secrets.put(&format!("policies/{}", policy.name), data).await?;
        let p = Arc::new(policy);
        self.state.write().policies.insert(p.name.clone(), p.clone());
        Ok(p)
    }

    pub async fn delete_policy(&self, name: &str) -> Result<()> {
        let _writes = self.writes.lock().await;
        let in_use = self.state.read().keys.values().any(|e| !e.revoked && e.info.policy == name);
        anyhow::ensure!(!in_use, "policy {name} still has keys; revoke them first");
        self.secrets.delete(&format!("policies/{name}")).await?;
        self.state.write().policies.remove(name);
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
    /// where the client reads it.
    pub async fn issue(&self, req: IssueRequest) -> Result<KeyInfo> {
        anyhow::ensure!(valid_name(&req.name), "key name must be lowercase letters, digits, '-' or '_' (max 63)");
        let _writes = self.writes.lock().await;
        // Checked under the lock, so a concurrent delete_policy can't slip in between.
        anyhow::ensure!(self.policy(&req.policy).is_some(), "policy {:?} does not exist", req.policy);
        let id = format!("{}/{}", req.policy, req.name);
        let (existing, version) = self.secrets.get_versioned(&key_path(&id)).await?;
        let was_revoked = existing.as_ref().is_some_and(|m| m.get("revoked").and_then(Value::as_bool).unwrap_or(false));
        anyhow::ensure!(existing.is_none() || was_revoked, "key {id} already exists; rotate it instead");
        let now = Utc::now();
        let record = KeyRecord {
            api_key: Some(new_key()),
            client_id: id.clone(),
            policy: req.policy,
            name: req.name,
            description: req.description.trim().to_string(),
            created_at: Some(now),
            expires_at: req.ttl_secs.map(|s| now + chrono::Duration::seconds(s)),
            ..Default::default()
        };
        // CAS against the version we saw (0 = new, or the revocation marker).
        self.write(&id, &record, version).await?;
        self.get(&id).context("issued key missing from cache")
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
    pub async fn revoke(&self, client_id: &str) -> Result<()> {
        let _writes = self.writes.lock().await;
        let (record, version) = self.fetch(client_id).await?;
        let marker = KeyRecord {
            api_key: None,
            client_id: record.client_id,
            policy: record.policy,
            name: record.name,
            revoked: true,
            revoked_at: Some(Utc::now()),
            ..Default::default()
        };
        let new_version = self.write(client_id, &marker, version).await?;
        let old: Vec<u64> = (1..new_version).collect();
        self.secrets.destroy_versions(&key_path(client_id), &old).await?;
        Ok(())
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
        let mut st = self.state.write();
        if let Some(old) = st.keys.remove(client_id) {
            st.by_hash.retain(|_, id| id != client_id);
            drop(old);
        }
        insert_entry(&mut st, client_id, record);
        Ok(version)
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
        km.put_policy(Policy { name: "dev".into(), providers: vec!["claude".into()], ..Default::default() }, &known()).await.unwrap();
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
        assert_eq!(info.client_id, "dev/devcontainer");
        assert_eq!(info.vault_path, "keys/dev/devcontainer");
        assert!(km.issue(IssueRequest { policy: "dev".into(), name: "devcontainer".into(), ..Default::default() }).await.is_err());

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
        assert!(km.revoke("dev/devcontainer").await.is_err());
        // Re-issuing after revocation is allowed.
        km.issue(IssueRequest { policy: "dev".into(), name: "devcontainer".into(), ..Default::default() }).await.unwrap();
        assert!(km.delete_policy("dev").await.is_err(), "policy with keys can't be deleted");
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
        km.put_policy(Policy { name: "dev".into(), providers: vec!["claude".into()], ..Default::default() }, &known()).await.unwrap();
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
        km.put_policy(Policy { name: "dev".into(), providers: vec!["claude".into()], ..Default::default() }, &known()).await.unwrap();

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

    #[tokio::test]
    async fn expired_keys_rejected() {
        let (km, s) = manager().await;
        km.issue(IssueRequest { policy: "dev".into(), name: "a".into(), ttl_secs: Some(-1), ..Default::default() }).await.unwrap();
        assert!(km.authenticate(&vault_key(&s, "dev/a").await).await.is_none());
    }
}
