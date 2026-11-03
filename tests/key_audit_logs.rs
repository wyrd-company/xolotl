//! What the log records about who acted on a client key. This file is its own
//! process with one test, so that its global subscriber sees every event; see
//! `policy_logs.rs`.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::Request;
use serde_json::{Value, json};
use tower::ServiceExt;
use xolotl::config::Config;
use xolotl::db::Db;
use xolotl::gateway::Gateway;
use xolotl::keys::{KeyManager, Scope};
use xolotl::limits::Limiter;
use xolotl::mcp::McpHub;
use xolotl::policy::Policy;
use xolotl::secrets::{MemoryStore, Secrets};
use xolotl::server::{AppState, router};
use xolotl::subscription::Subscriptions;

#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl Logs {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
    /// The one log line that holds a message.
    fn line(&self, message: &str) -> String {
        let text = self.text();
        let found: Vec<&str> = text.lines().filter(|l| l.contains(message)).collect();
        assert_eq!(found.len(), 1, "{message}: {text}");
        found[0].to_string()
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

async fn state(secrets: Secrets) -> AppState {
    let cfg = Config::default();
    let db = Db::in_memory().await.unwrap();
    let http = xolotl::proxy::upstream_client(std::time::Duration::from_secs(5)).unwrap();
    let keys = Arc::new(KeyManager::new(secrets.clone()));
    let limiter = Arc::new(Limiter::default());
    let gateway = Gateway::new(&cfg.gateway, secrets.clone(), http.clone(), db.clone()).with_limiter(limiter.clone());
    let catalog = Arc::new(xolotl::catalog::ModelCatalog::new(cfg.catalog.clone(), gateway.kinds()).with_db(db.clone()).await);
    let gateway = Arc::new(gateway.with_catalog(catalog.clone()));
    let subs = Arc::new(
        Subscriptions::new(cfg.subscription.clone(), db.clone(), secrets.clone(), http.clone())
            .with_limiter(limiter.clone())
            .with_catalog(catalog.clone()),
    );
    let mcp = Arc::new(McpHub::new(&cfg, secrets.clone(), db.clone()));
    AppState { cfg: Arc::new(cfg), db, secrets, keys, subs, gateway, catalog, vault: None, sessions: Default::default(), mcp, limiter }
}

async fn call(state: &AppState, method: &str, uri: &str, key: &str, body: Option<Value>) -> u16 {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json");
    let body = body.map(|b| Body::from(b.to_string())).unwrap_or_else(Body::empty);
    router(state.clone()).oneshot(req.body(body).unwrap()).await.unwrap().status().as_u16()
}

#[tokio::test]
async fn the_log_names_the_key_that_acted() {
    let logs = Logs::default();
    let writer = logs.clone();
    tracing_subscriber::fmt().with_writer(move || writer.clone()).with_ansi(false).init();

    let secrets: Secrets = Arc::new(MemoryStore::default());
    secrets.put("admin", json!({"token": "boot"}).as_object().unwrap().clone()).await.unwrap();
    let s = state(secrets.clone()).await;
    s.keys.reload().await.unwrap();
    let known = s.catalog.known_providers();
    let none = Default::default();
    s.keys.put_policy(Policy { name: "target".into(), providers: vec!["*".into()], ..Default::default() }, &known, &none).await.unwrap();
    s.keys.put_policy(Policy { name: "other".into(), providers: vec!["*".into()], ..Default::default() }, &known, &none).await.unwrap();
    s.keys
        .put_policy(
            Policy { name: "issuer".into(), scopes: vec![Scope::Keys], key_policies: vec!["target".into()], ..Default::default() },
            &known,
            &none,
        )
        .await
        .unwrap();
    assert_eq!(call(&s, "POST", "/admin/keys", "boot", Some(json!({"policy": "issuer", "name": "robot"}))).await, 201);
    assert!(logs.line("issued client key").contains(r#"actor="admin""#), "{}", logs.text());
    let robot = secrets.get("keys/issuer/robot").await.unwrap().unwrap()["api_key"].as_str().unwrap().to_string();

    let audited = r#"actor="issuer/robot""#;
    assert_eq!(call(&s, "POST", "/admin/keys", &robot, Some(json!({"policy": "target", "name": "a"}))).await, 201);
    let issued = logs
        .text()
        .lines()
        .filter(|l| l.contains("issued client key") && l.contains("client=target/a"))
        .map(str::to_string)
        .collect::<Vec<_>>();
    assert_eq!(issued.len(), 1, "{}", logs.text());
    assert!(issued[0].contains(audited), "{}", issued[0]);

    assert_eq!(call(&s, "POST", "/admin/keys", &robot, Some(json!({"policy": "target", "name": "a"}))).await, 200);
    let exists = logs.line("client key exists, not changed");
    assert!(exists.contains(audited) && exists.contains("client=target/a"), "{exists}");

    assert_eq!(call(&s, "POST", "/admin/keys/target/a/disable", &robot, None).await, 200);
    let changed = logs.line("changed client key");
    assert!(changed.contains(audited) && changed.contains("disabled=true"), "{changed}");

    assert_eq!(call(&s, "DELETE", "/admin/keys/target/a", &robot, None).await, 200);
    let revoked = logs.line("revoke of client key");
    assert!(revoked.contains(audited) && revoked.contains("client=target/a") && revoked.contains(r#"status="revoked""#), "{revoked}");

    assert_eq!(call(&s, "DELETE", "/admin/keys/other/b", &robot, None).await, 403);
    let refused = logs.line("refused a client key request");
    assert!(refused.contains(audited) && refused.contains(r#"policy="other""#) && refused.contains(r#"action="revoke""#), "{refused}");

    assert!(!logs.text().contains(&robot), "the log holds no key");
}
