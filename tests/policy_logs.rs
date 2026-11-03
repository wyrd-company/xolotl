//! What the operator reads in the log about stored policies. This file is its
//! own process with one test, so that its global subscriber sees every event:
//! `tracing` caches across threads whether a callsite has a listener, so a
//! capture beside parallel tests misses events.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value, json};
use xolotl::keys::{IssueRequest, KeyManager};
use xolotl::policy::Policy;
use xolotl::secrets::{MemoryStore, Secrets};

#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl Logs {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
    fn count(&self, needle: &str) -> usize {
        self.text().matches(needle).count()
    }
}

impl std::io::Write for Logs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn record(value: Value) -> Map<String, Value> {
    value.as_object().unwrap().clone()
}

async fn key_under_stored(km: &KeyManager, s: &Secrets, name: &str, stored: Map<String, Value>) -> String {
    let known = BTreeSet::from(["claude".to_string()]);
    km.put_policy(Policy { name: name.into(), providers: vec!["claude".into()], ..Default::default() }, &known, &BTreeSet::new())
        .await
        .unwrap();
    km.issue(IssueRequest { policy: name.into(), name: "client".into(), ..Default::default() }).await.unwrap();
    s.put(&format!("policies/{name}"), stored).await.unwrap();
    s.get(&format!("keys/{name}/client")).await.unwrap().unwrap()["api_key"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn the_log_names_migrated_policies_once_and_each_refused_key() {
    let logs = Logs::default();
    let writer = logs.clone();
    tracing_subscriber::fmt().with_max_level(tracing::Level::WARN).with_ansi(false).with_writer(move || writer.clone()).init();

    let s: Secrets = Arc::new(MemoryStore::default());
    let km = KeyManager::new(s.clone());
    let old = record(json!({"name": "old", "providers": ["claude"], "deny_models": ["x"], "scopes": ["proxy"]}));
    let old_key = key_under_stored(&km, &s, "old", old).await;
    let broken = record(json!({"name": "broken", "providers": ["claude"], "providerz": []}));
    let broken_key = key_under_stored(&km, &s, "broken", broken).await;

    km.reload().await.unwrap();
    assert_eq!(logs.count("migrated in memory"), 1, "{}", logs.text());
    assert_eq!(logs.count("policy=old"), 1, "{}", logs.text());
    km.reload().await.unwrap();
    assert_eq!(logs.count("migrated in memory"), 1, "not at every reload: {}", logs.text());

    assert!(km.authenticate(&old_key).await.is_some());
    assert_eq!(logs.count("refusing a valid key"), 0, "{}", logs.text());

    assert!(km.authenticate("xol_someone-else").await.is_none());
    assert_eq!(logs.count("refusing a valid key"), 0, "an unknown key is not a refused valid key: {}", logs.text());

    assert!(km.authenticate(&broken_key).await.is_none());
    let text = logs.text();
    assert!(text.contains("refusing a valid key"), "{text}");
    assert!(text.contains("client=broken/client") && text.contains("policy=broken"), "{text}");
    assert!(!text.contains(&broken_key), "the key stays out of the log");
}
