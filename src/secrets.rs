//! Secret storage abstraction. Production uses OpenBao ([`crate::vault`]);
//! tests use the in-memory store.
//!
//! Layout (relative to the configured prefix):
//! - `admin`                        `{ token }` bootstrap admin / management token
//! - `telemetry`                    OTLP header values
//! - `providers/<name>`             `{ api_key }` upstream gateway provider keys
//! - `accounts/<provider>/<id>`     OAuth credentials for subscription accounts
//! - `mcp/<server>`                 values for an MCP server's `{field}` header placeholders
//! - `mcp/<server>/oauth`           an MCP server's OAuth client registration and tokens
//! - `policies/<name>`               client key policies
//! - `keys/<policy>/<client>`       client key records (readable by that client)

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
    /// Latest version's data and its version number (0 when absent).
    async fn get_versioned(&self, key: &str) -> Result<(Option<Map<String, Value>>, u64)>;
    /// Check-and-set write: succeeds only if the current version is `cas`
    /// (0 = must not exist). Returns the new version; `Conflict` on mismatch.
    async fn put_cas(&self, key: &str, data: Map<String, Value>, cas: u64) -> Result<u64>;
    /// Permanently destroy the data of the given versions (metadata is kept).
    async fn destroy_versions(&self, key: &str, versions: &[u64]) -> Result<()>;
}

/// A check-and-set write lost a race with another writer.
#[derive(Debug, thiserror::Error)]
#[error("secret {0} was changed concurrently; reload and retry")]
pub struct Conflict(pub String);

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

/// Latest version number and data of one in-memory secret.
type Versioned = (u64, Option<Map<String, Value>>);

#[derive(Default)]
pub struct MemoryStore {
    data: Mutex<BTreeMap<String, Versioned>>,
}

#[async_trait::async_trait]
impl SecretStore for MemoryStore {
    async fn get(&self, key: &str) -> Result<Option<Map<String, Value>>> {
        Ok(self.data.lock().get(key.trim_matches('/')).and_then(|(_, d)| d.clone()))
    }
    async fn put(&self, key: &str, data: Map<String, Value>) -> Result<()> {
        let mut m = self.data.lock();
        let e = m.entry(key.trim_matches('/').to_string()).or_insert((0, None));
        *e = (e.0 + 1, Some(data));
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
                if !out.contains(&entry) {
                    out.push(entry);
                }
            }
        }
        Ok(out)
    }
    async fn get_versioned(&self, key: &str) -> Result<(Option<Map<String, Value>>, u64)> {
        Ok(self.data.lock().get(key.trim_matches('/')).cloned().map(|(v, d)| (d, v)).unwrap_or((None, 0)))
    }
    async fn put_cas(&self, key: &str, data: Map<String, Value>, cas: u64) -> Result<u64> {
        let mut m = self.data.lock();
        let e = m.entry(key.trim_matches('/').to_string()).or_insert((0, None));
        if e.0 != cas {
            return Err(Conflict(key.to_string()).into());
        }
        *e = (e.0 + 1, Some(data));
        Ok(e.0)
    }
    async fn destroy_versions(&self, _key: &str, _versions: &[u64]) -> Result<()> {
        // Only the latest version is kept in memory.
        Ok(())
    }
}

/// Test stores for failure and race scenarios.
#[cfg(test)]
pub mod testing {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    /// An in-memory store that can fail every write, and pause once at a
    /// chosen read or write until the test releases it: before a `get` or
    /// `put`, or after a `get` has read its value (`"got"`).
    #[derive(Default)]
    pub struct GatedStore {
        pub inner: MemoryStore,
        pub fail_writes: AtomicBool,
        gate: Mutex<Option<(&'static str, String)>>,
        /// Notified when the armed operation is reached (and paused).
        pub reached: tokio::sync::Notify,
        /// Notify to let the paused operation continue.
        pub release: tokio::sync::Notify,
    }

    impl GatedStore {
        /// Pause the next `op` (`"get"`, `"got"` or `"put"`) on `key`.
        pub fn arm(&self, op: &'static str, key: &str) {
            *self.gate.lock() = Some((op, key.to_string()));
        }

        pub fn set_fail_writes(&self, fail: bool) {
            self.fail_writes.store(fail, Ordering::SeqCst);
        }

        async fn pass(&self, op: &'static str, key: &str) {
            let hit = {
                let mut g = self.gate.lock();
                let hit = g.as_ref().is_some_and(|(o, k)| *o == op && k == key);
                if hit {
                    *g = None;
                }
                hit
            };
            if hit {
                self.reached.notify_one();
                self.release.notified().await;
            }
        }

        fn writable(&self) -> Result<()> {
            anyhow::ensure!(!self.fail_writes.load(Ordering::SeqCst), "vault unavailable");
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl SecretStore for GatedStore {
        async fn get(&self, key: &str) -> Result<Option<Map<String, Value>>> {
            self.pass("get", key).await;
            let value = self.inner.get(key).await;
            self.pass("got", key).await;
            value
        }
        async fn put(&self, key: &str, data: Map<String, Value>) -> Result<()> {
            self.pass("put", key).await;
            self.writable()?;
            self.inner.put(key, data).await
        }
        async fn delete(&self, key: &str) -> Result<()> {
            self.inner.delete(key).await
        }
        async fn list(&self, dir: &str) -> Result<Vec<String>> {
            self.inner.list(dir).await
        }
        async fn get_versioned(&self, key: &str) -> Result<(Option<Map<String, Value>>, u64)> {
            self.pass("get", key).await;
            self.inner.get_versioned(key).await
        }
        async fn put_cas(&self, key: &str, data: Map<String, Value>, cas: u64) -> Result<u64> {
            self.pass("put", key).await;
            self.writable()?;
            self.inner.put_cas(key, data, cas).await
        }
        async fn destroy_versions(&self, key: &str, versions: &[u64]) -> Result<()> {
            self.inner.destroy_versions(key, versions).await
        }
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
