//! Downstream client keys: issue, rotate, revoke, authenticate.
//!
//! Keys look like `xol_<id>_<secret>`. Only a SHA-256 hash is stored, in
//! OpenBao under `clients/<id>`; the plaintext is shown once at issue/rotate.
//! The id prefix makes lookup O(1) without a hash index. The secret is 256
//! random bits so a fast hash is sufficient.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use parking_lot::RwLock;
use rand::RngExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::db::now;
use crate::secrets::{Secrets, get_typed, put_typed};

pub const KEY_PREFIX: &str = "xol";
const ID_ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// Use the LLM proxy / gateway endpoints.
    Proxy,
    /// CLIProxyAPI-compatible management API (T3Code hubs).
    Management,
    /// Full administration (keys, accounts). Implies every other scope.
    Admin,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClientKey {
    pub id: String,
    pub name: String,
    pub scopes: Vec<Scope>,
    pub hash: String,
    /// Old key hash kept valid until `previous_expires_at` after a rotation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_expires_at: Option<i64>,
    /// Glob patterns (`*` wildcard) of models this key may use. Empty = all.
    #[serde(default)]
    pub allowed_models: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotated_at: Option<i64>,
    #[serde(default)]
    pub disabled: bool,
}

impl ClientKey {
    pub fn has_scope(&self, scope: Scope) -> bool {
        self.scopes.contains(&Scope::Admin) || self.scopes.contains(&scope)
    }

    pub fn allows_model(&self, model: &str) -> bool {
        self.allowed_models.is_empty() || self.allowed_models.iter().any(|p| glob_match(p, model))
    }

    /// Public view without hashes.
    pub fn redacted(&self) -> serde_json::Value {
        let mut v = serde_json::to_value(self).unwrap_or_default();
        if let Some(m) = v.as_object_mut() {
            m.remove("hash");
            m.remove("previous_hash");
        }
        v
    }
}

/// The authenticated caller.
#[derive(Debug, Clone)]
pub enum Principal {
    /// The bootstrap admin token from the vault `admin` secret.
    Bootstrap,
    Client(Arc<ClientKey>),
}

impl Principal {
    pub fn has_scope(&self, scope: Scope) -> bool {
        match self {
            Principal::Bootstrap => true,
            Principal::Client(k) => k.has_scope(scope),
        }
    }
    pub fn client_id(&self) -> Option<&str> {
        match self {
            Principal::Bootstrap => None,
            Principal::Client(k) => Some(&k.id),
        }
    }
    pub fn allows_model(&self, model: &str) -> bool {
        match self {
            Principal::Bootstrap => true,
            Principal::Client(k) => k.allows_model(model),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct IssueRequest {
    pub name: String,
    #[serde(default)]
    pub scopes: Vec<Scope>,
    #[serde(default)]
    pub allowed_models: Vec<String>,
    /// Lifetime in seconds; none = no expiry.
    #[serde(default)]
    pub ttl_secs: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssuedKey {
    pub key: String,
    pub record: serde_json::Value,
}

pub struct KeyManager {
    secrets: Secrets,
    cache: RwLock<HashMap<String, Arc<ClientKey>>>,
    bootstrap_hash: RwLock<Option<String>>,
}

impl KeyManager {
    pub fn new(secrets: Secrets) -> Self {
        Self { secrets, cache: RwLock::new(HashMap::new()), bootstrap_hash: RwLock::new(None) }
    }

    /// Reload every key record and the bootstrap admin token from the vault.
    pub async fn reload(&self) -> Result<usize> {
        let mut fresh = HashMap::new();
        for id in self.secrets.list("clients").await? {
            if id.ends_with('/') {
                continue;
            }
            match get_typed::<ClientKey>(&*self.secrets, &format!("clients/{id}")).await {
                Ok(Some(k)) => {
                    fresh.insert(k.id.clone(), Arc::new(k));
                }
                Ok(None) => {}
                Err(e) => tracing::warn!(id, error = %e, "skipping unreadable client key record"),
            }
        }
        let n = fresh.len();
        *self.cache.write() = fresh;
        let admin = self.secrets.get("admin").await?;
        *self.bootstrap_hash.write() =
            admin.and_then(|m| m.get("token").and_then(|t| t.as_str()).filter(|t| !t.is_empty()).map(sha256_hex));
        Ok(n)
    }

    pub fn authenticate(&self, presented: &str) -> Option<Principal> {
        let presented = presented.trim();
        if presented.is_empty() {
            return None;
        }
        let hash = sha256_hex(presented);
        if let Some(b) = self.bootstrap_hash.read().as_deref()
            && ct_eq(b.as_bytes(), hash.as_bytes())
        {
            return Some(Principal::Bootstrap);
        }
        let id = parse_key_id(presented)?;
        let key = self.cache.read().get(id).cloned()?;
        let t = now();
        if key.disabled || key.expires_at.is_some_and(|e| e <= t) {
            return None;
        }
        let current = ct_eq(key.hash.as_bytes(), hash.as_bytes());
        let previous = key.previous_expires_at.is_some_and(|e| e > t)
            && key.previous_hash.as_deref().is_some_and(|p| ct_eq(p.as_bytes(), hash.as_bytes()));
        (current || previous).then_some(Principal::Client(key))
    }

    pub fn list(&self) -> Vec<Arc<ClientKey>> {
        let mut v: Vec<_> = self.cache.read().values().cloned().collect();
        v.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        v
    }

    pub fn get(&self, id: &str) -> Option<Arc<ClientKey>> {
        self.cache.read().get(id).cloned()
    }

    pub async fn issue(&self, req: IssueRequest) -> Result<IssuedKey> {
        anyhow::ensure!(!req.name.trim().is_empty(), "key name is required");
        let id = loop {
            let id = random_id(10);
            if !self.cache.read().contains_key(&id) {
                break id;
            }
        };
        let (key, hash) = new_secret(&id);
        let scopes = if req.scopes.is_empty() { vec![Scope::Proxy] } else { req.scopes };
        let record = ClientKey {
            id: id.clone(),
            name: req.name.trim().to_string(),
            scopes,
            hash,
            previous_hash: None,
            previous_expires_at: None,
            allowed_models: req.allowed_models,
            expires_at: req.ttl_secs.map(|s| now() + s),
            created_at: now(),
            rotated_at: None,
            disabled: false,
        };
        self.store(&record).await?;
        Ok(IssuedKey { key, record: record.redacted() })
    }

    /// Replace a key's secret. The old secret stays valid for `grace_secs`.
    pub async fn rotate(&self, id: &str, grace_secs: i64) -> Result<IssuedKey> {
        let mut record = self.fetch(id).await?;
        let (key, hash) = new_secret(id);
        let t = now();
        if grace_secs > 0 {
            record.previous_hash = Some(std::mem::replace(&mut record.hash, hash));
            record.previous_expires_at = Some(t + grace_secs);
        } else {
            record.hash = hash;
            record.previous_hash = None;
            record.previous_expires_at = None;
        }
        record.rotated_at = Some(t);
        self.store(&record).await?;
        Ok(IssuedKey { key, record: record.redacted() })
    }

    /// Permanently delete a key from the vault (all versions).
    pub async fn revoke(&self, id: &str) -> Result<bool> {
        let existed = self.cache.read().contains_key(id) || self.secrets.get(&format!("clients/{id}")).await?.is_some();
        self.secrets.delete(&format!("clients/{id}")).await?;
        self.cache.write().remove(id);
        Ok(existed)
    }

    pub async fn set_disabled(&self, id: &str, disabled: bool) -> Result<serde_json::Value> {
        let mut record = self.fetch(id).await?;
        record.disabled = disabled;
        self.store(&record).await?;
        Ok(record.redacted())
    }

    async fn fetch(&self, id: &str) -> Result<ClientKey> {
        get_typed::<ClientKey>(&*self.secrets, &format!("clients/{id}")).await?.with_context(|| format!("client key {id} not found"))
    }

    async fn store(&self, record: &ClientKey) -> Result<()> {
        put_typed(&*self.secrets, &format!("clients/{}", record.id), record).await?;
        self.cache.write().insert(record.id.clone(), Arc::new(record.clone()));
        Ok(())
    }
}

fn parse_key_id(key: &str) -> Option<&str> {
    let rest = key.strip_prefix(KEY_PREFIX)?.strip_prefix('_')?;
    let (id, secret) = rest.split_once('_')?;
    (!id.is_empty() && !secret.is_empty()).then_some(id)
}

fn new_secret(id: &str) -> (String, String) {
    let bytes: [u8; 32] = rand::rng().random();
    let key = format!("{KEY_PREFIX}_{id}_{}", URL_SAFE_NO_PAD.encode(bytes));
    let hash = sha256_hex(&key);
    (key, hash)
}

pub fn random_id(len: usize) -> String {
    let mut rng = rand::rng();
    (0..len).map(|_| ID_ALPHABET[rng.random_range(0..ID_ALPHABET.len())] as char).collect()
}

pub fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
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

    fn manager() -> (KeyManager, Secrets) {
        let s: Secrets = Arc::new(MemoryStore::default());
        (KeyManager::new(s.clone()), s)
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
        let (km, _) = manager();
        let issued = km
            .issue(IssueRequest { name: "devcontainer".into(), allowed_models: vec!["claude-*".into()], ..Default::default() })
            .await
            .unwrap();
        assert!(issued.key.starts_with("xol_"));
        assert!(issued.record.get("hash").is_none());
        let p = km.authenticate(&issued.key).expect("valid key");
        assert!(p.has_scope(Scope::Proxy));
        assert!(!p.has_scope(Scope::Admin));
        assert!(p.allows_model("claude-opus-4"));
        assert!(!p.allows_model("gpt-5"));
        assert!(km.authenticate(&format!("{}x", issued.key)).is_none());

        let id = issued.record["id"].as_str().unwrap().to_string();
        let rotated = km.rotate(&id, 60).await.unwrap();
        assert!(km.authenticate(&rotated.key).is_some());
        assert!(km.authenticate(&issued.key).is_some(), "old key valid during grace");

        let rotated2 = km.rotate(&id, 0).await.unwrap();
        assert!(km.authenticate(&rotated.key).is_none());
        assert!(km.authenticate(&rotated2.key).is_some());

        km.set_disabled(&id, true).await.unwrap();
        assert!(km.authenticate(&rotated2.key).is_none());
        km.set_disabled(&id, false).await.unwrap();

        assert!(km.revoke(&id).await.unwrap());
        assert!(km.authenticate(&rotated2.key).is_none());
        assert!(!km.revoke(&id).await.unwrap());
    }

    #[tokio::test]
    async fn reload_picks_up_bootstrap_and_records() {
        let (km, s) = manager();
        let issued = km.issue(IssueRequest { name: "a".into(), ..Default::default() }).await.unwrap();
        s.put("admin", json!({"token": "boot"}).as_object().unwrap().clone()).await.unwrap();
        let km2 = KeyManager::new(s);
        assert_eq!(km2.reload().await.unwrap(), 1);
        assert!(km2.authenticate(&issued.key).is_some());
        assert!(matches!(km2.authenticate("boot"), Some(Principal::Bootstrap)));
    }

    #[tokio::test]
    async fn expired_keys_rejected() {
        let (km, _) = manager();
        let issued = km.issue(IssueRequest { name: "a".into(), ttl_secs: Some(-1), ..Default::default() }).await.unwrap();
        assert!(km.authenticate(&issued.key).is_none());
    }
}
