//! Secret storage abstraction. Production uses OpenBao ([`crate::vault`]);
//! tests use the in-memory store.
//!
//! Layout (relative to the configured prefix):
//! - `admin`                        `{ token }` bootstrap admin / management token
//! - `telemetry`                    OTLP header values
//! - `providers/<name>`             `{ api_key }` upstream gateway provider keys
//! - `accounts/<provider>/<id>`     OAuth credentials for subscription accounts
//! - `clients/<id>`                 downstream client key records (hash only)

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Result;
use parking_lot::Mutex;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

#[async_trait::async_trait]
pub trait SecretStore: Send + Sync {
    async fn get(&self, key: &str) -> Result<Option<Map<String, Value>>>;
    async fn put(&self, key: &str, data: Map<String, Value>) -> Result<()>;
    async fn delete(&self, key: &str) -> Result<()>;
    /// Direct children of `dir`; sub-directories end with `/`.
    async fn list(&self, dir: &str) -> Result<Vec<String>>;
}

pub type Secrets = Arc<dyn SecretStore>;

pub async fn get_typed<T: DeserializeOwned>(store: &dyn SecretStore, key: &str) -> Result<Option<T>> {
    match store.get(key).await? {
        Some(m) => Ok(Some(serde_json::from_value(Value::Object(m))?)),
        None => Ok(None),
    }
}

pub async fn put_typed<T: Serialize>(store: &dyn SecretStore, key: &str, value: &T) -> Result<()> {
    match serde_json::to_value(value)? {
        Value::Object(m) => store.put(key, m).await,
        other => anyhow::bail!("secret {key} must serialize to an object, got {other}"),
    }
}

#[derive(Default)]
pub struct MemoryStore {
    data: Mutex<BTreeMap<String, Map<String, Value>>>,
}

#[async_trait::async_trait]
impl SecretStore for MemoryStore {
    async fn get(&self, key: &str) -> Result<Option<Map<String, Value>>> {
        Ok(self.data.lock().get(key.trim_matches('/')).cloned())
    }
    async fn put(&self, key: &str, data: Map<String, Value>) -> Result<()> {
        self.data.lock().insert(key.trim_matches('/').to_string(), data);
        Ok(())
    }
    async fn delete(&self, key: &str) -> Result<()> {
        self.data.lock().remove(key.trim_matches('/'));
        Ok(())
    }
    async fn list(&self, dir: &str) -> Result<Vec<String>> {
        let prefix = format!("{}/", dir.trim_matches('/'));
        let mut out: Vec<String> = Vec::new();
        for k in self.data.lock().keys() {
            if let Some(rest) = k.strip_prefix(&prefix) {
                let entry = match rest.split_once('/') {
                    Some((d, _)) => format!("{d}/"),
                    None => rest.to_string(),
                };
                if out.last() != Some(&entry) && !out.contains(&entry) {
                    out.push(entry);
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn memory_list_returns_children() {
        let s = MemoryStore::default();
        for k in ["accounts/codex/a", "accounts/codex/b", "accounts/claude/c", "clients/x"] {
            s.put(k, json!({"v": 1}).as_object().unwrap().clone()).await.unwrap();
        }
        assert_eq!(s.list("accounts").await.unwrap(), vec!["claude/", "codex/"]);
        assert_eq!(s.list("accounts/codex").await.unwrap(), vec!["a", "b"]);
        assert!(s.list("nothing").await.unwrap().is_empty());
    }
}
