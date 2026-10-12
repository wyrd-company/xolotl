use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::Request;
use serde_json::{Value, json};
use tower::ServiceExt;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::config::{Endpoints, GatewayConfig, ProviderConfig, ProviderKind};
use crate::db::now;
use crate::keys::IssueRequest;
use crate::secrets::MemoryStore;
use crate::subscription::{Credential, auth_index};

struct Harness {
    state: AppState,
    admin: String,
    upstream: MockServer,
}

async fn harness() -> Harness {
    let upstream = MockServer::start().await;
    let uri = upstream.uri();
    let secrets: Secrets = Arc::new(MemoryStore::default());
    secrets.put("admin", json!({"token": "boot"}).as_object().unwrap().clone()).await.unwrap();
    secrets.put("providers/llama", json!({"api_key": "llama-key"}).as_object().unwrap().clone()).await.unwrap();

    let mut cfg = Config {
        gateway: GatewayConfig {
            providers: BTreeMap::from([(
                "llama".to_string(),
                ProviderConfig { kind: ProviderKind::OpenaiCompatible, base_url: Some(format!("{uri}/llama/v1")), ..Default::default() },
            )]),
            models: vec![],
        },
        ..Default::default()
    };
    cfg.subscription.endpoints = Endpoints {
        codex_backend: format!("{uri}/backend-api"),
        codex_auth: uri.clone(),
        claude_api: uri.clone(),
        claude_authorize: format!("{uri}/oauth/authorize"),
        claude_token: format!("{uri}/v1/oauth/token"),
        claude_manual_redirect: format!("{uri}/cb"),
        opencode_go: format!("{uri}/zen/go"),
    };
    let db = Db::in_memory().await.unwrap();
    let http = reqwest::Client::new();
    let keys = Arc::new(KeyManager::new(secrets.clone()));
    keys.reload().await.unwrap();
    let subs = Arc::new(Subscriptions::new(cfg.subscription.clone(), db.clone(), secrets.clone(), http.clone()));
    let gateway = Arc::new(Gateway::new(&cfg.gateway, secrets.clone(), http, db.clone()));
    let state = AppState { cfg: Arc::new(cfg), db, secrets, keys, subs, gateway, vault: None };
    Harness { state, admin: "boot".into(), upstream }
}

impl Harness {
    async fn call(&self, method: &str, uri: &str, key: Option<&str>, body: Option<Value>) -> (u16, Value, axum::http::HeaderMap) {
        let mut req = Request::builder().method(method).uri(uri);
        if let Some(k) = key {
            req = req.header("authorization", format!("Bearer {k}"));
        }
        let body = match body {
            Some(b) => {
                req = req.header("content-type", "application/json");
                Body::from(b.to_string())
            }
            None => Body::empty(),
        };
        let resp = router(self.state.clone()).oneshot(req.body(body).unwrap()).await.unwrap();
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v = serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into()));
        (status, v, headers)
    }

    async fn issue(&self, req: IssueRequest) -> String {
        self.state.keys.issue(req).await.unwrap().key
    }

    async fn codex_account(&self) -> String {
        self.state
            .subs
            .save_account(Credential {
                provider: crate::subscription::SubProvider::Codex,
                access_token: "codex-tok".into(),
                refresh_token: Some("rt".into()),
                expires_at: Some(now() + 3600),
                account_id: Some("acct-1".into()),
                email: Some("me@x.com".into()),
                plan: Some("pro".into()),
                ..Default::default()
            })
            .await
            .unwrap()
            .id
    }
}

#[tokio::test]
async fn health_and_auth() {
    let h = harness().await;
    assert_eq!(h.call("GET", "/healthz", None, None).await.0, 200);
    assert_eq!(h.call("GET", "/readyz", None, None).await.0, 200);
    assert_eq!(h.call("POST", "/v1/chat/completions", None, Some(json!({}))).await.0, 401);
    assert_eq!(h.call("POST", "/v1/chat/completions", Some("xol_nope_nope"), Some(json!({}))).await.0, 401);
    let proxy_key = h.issue(IssueRequest { name: "p".into(), ..Default::default() }).await;
    assert_eq!(h.call("GET", "/admin/keys", Some(&proxy_key), None).await.0, 403);
    assert_eq!(h.call("GET", "/v0/management/auth-files", Some(&proxy_key), None).await.0, 401);
}

#[tokio::test]
async fn gateway_routing_injects_upstream_key() {
    let h = harness().await;
    Mock::given(method("POST"))
        .and(path("/llama/v1/chat/completions"))
        .and(header("authorization", "Bearer llama-key"))
        .and(body_partial_json(json!({"model": "qwen3"})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"choices": [], "usage": {"prompt_tokens": 3, "completion_tokens": 4}})),
        )
        .mount(&h.upstream)
        .await;
    let key = h.issue(IssueRequest { name: "p".into(), allowed_models: vec!["llama/*".into()], ..Default::default() }).await;
    let (status, _, _) = h.call("POST", "/v1/chat/completions", Some(&key), Some(json!({"model": "llama/qwen3", "messages": []}))).await;
    assert_eq!(status, 200);
    // Not allowed by the key's model globs.
    let (status, _, _) = h.call("POST", "/v1/responses", Some(&key), Some(json!({"model": "gpt-5-codex"}))).await;
    assert_eq!(status, 403);
    // Unknown prefix on a gateway-only endpoint.
    let all = h.issue(IssueRequest { name: "all".into(), ..Default::default() }).await;
    let (status, body, _) = h.call("POST", "/v1/chat/completions", Some(&all), Some(json!({"model": "nope/x"}))).await;
    assert_eq!(status, 400, "{body}");
}

#[tokio::test]
async fn passthrough_reaches_provider_paths() {
    let h = harness().await;
    Mock::given(method("GET"))
        .and(path("/llama/v1/props"))
        .and(header("authorization", "Bearer llama-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .mount(&h.upstream)
        .await;
    let key = h.issue(IssueRequest { name: "p".into(), ..Default::default() }).await;
    let (status, body, _) = h.call("GET", "/llama/v1/props", Some(&key), None).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["ok"], true);
    assert_eq!(h.call("GET", "/nope/v1/props", Some(&key), None).await.0, 404);
    let restricted = h.issue(IssueRequest { name: "r".into(), allowed_models: vec!["claude-*".into()], ..Default::default() }).await;
    assert_eq!(h.call("GET", "/llama/v1/props", Some(&restricted), None).await.0, 403);
}

#[tokio::test]
async fn responses_route_to_codex_pool_and_websocket_gets_426() {
    let h = harness().await;
    h.codex_account().await;
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/responses"))
        .and(header("authorization", "Bearer codex-tok"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("data: {\"type\":\"response.completed\"}\n\n", "text/event-stream"))
        .mount(&h.upstream)
        .await;
    let key = h.issue(IssueRequest { name: "codex".into(), allowed_models: vec!["codex/*".into()], ..Default::default() }).await;
    let (status, body, _) = h.call("POST", "/v1/responses", Some(&key), Some(json!({"model": "gpt-5-codex", "input": []}))).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(h.call("GET", "/v1/responses", Some(&key), None).await.0, 426);
}

#[tokio::test]
async fn messages_accept_x_api_key() {
    let h = harness().await;
    let key = h.issue(IssueRequest { name: "cc".into(), ..Default::default() }).await;
    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("x-api-key", &key)
        .header("content-type", "application/json")
        .body(Body::from(json!({"model": "claude-opus-5", "messages": []}).to_string()))
        .unwrap();
    let resp = router(h.state.clone()).oneshot(req).await.unwrap();
    // Authenticated, but no Claude accounts yet: Anthropic-shaped 503.
    assert_eq!(resp.status(), 503);
    let v: Value = serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(v["type"], "error");
}

#[tokio::test]
async fn t3code_management_endpoints() {
    let h = harness().await;
    let id = h.codex_account().await;
    let mgmt = h.issue(IssueRequest { name: "t3".into(), scopes: vec![Scope::Management], ..Default::default() }).await;

    let (status, body, _) = h.call("GET", "/v0/management/auth-files", Some(&mgmt), None).await;
    assert_eq!(status, 200);
    let f = &body["files"][0];
    assert_eq!(f["id"], id.as_str());
    assert_eq!(f["provider"], "codex");
    assert_eq!(f["disabled"], false);
    assert_eq!(f["email"], "me@x.com");
    assert_eq!(f["id_token"]["chatgpt_account_id"], "acct-1");
    assert_eq!(f["id_token"]["chatgpt_plan_type"], "pro");
    let idx = f["auth_index"].as_str().unwrap().to_string();
    assert_eq!(idx, auth_index(&id));

    Mock::given(method("GET"))
        .and(path("/backend-api/wham/usage"))
        .and(header("authorization", "Bearer codex-tok"))
        .and(header("chatgpt-account-id", "acct-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"rate_limit": {"primary_window": {"used_percent": 5}}})))
        .mount(&h.upstream)
        .await;
    let call = json!({
        "auth_index": idx, "method": "GET",
        "url": format!("{}/backend-api/wham/usage", h.upstream.uri()),
        "header": {"Authorization": "Bearer $TOKEN$", "Chatgpt-Account-Id": "acct-1", "OpenAI-Beta": "codex-1"}
    });
    let (status, body, _) = h.call("POST", "/v0/management/api-call", Some(&mgmt), Some(call)).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["status_code"], 200);
    let inner: Value = serde_json::from_str(body["body"].as_str().unwrap()).unwrap();
    assert_eq!(inner["rate_limit"]["primary_window"]["used_percent"], 5);

    let (status, _, _) = h
        .call("POST", "/v0/management/api-call", Some(&mgmt), Some(json!({"auth_index": "ffff", "method": "GET", "url": "https://x"})))
        .await;
    assert_eq!(status, 400);

    h.state.db.set_cooldown(&id, now() + 999, "test").await.unwrap();
    let (status, body, _) = h.call("POST", "/v0/management/reset-quota", Some(&mgmt), Some(json!({"auth_index": idx}))).await;
    assert_eq!(status, 200);
    assert_eq!(body["status"], "ok");
    assert!(!h.state.db.get_account(&id).await.unwrap().unwrap().cooling_down(now()));
    let (status, _, _) = h.call("POST", "/v0/management/reset-quota", Some(&mgmt), Some(json!({"auth_index": "nope"}))).await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn admin_key_lifecycle_and_accounts() {
    let h = harness().await;
    let (status, issued, _) =
        h.call("POST", "/admin/keys", Some(&h.admin), Some(json!({"name": "dev", "allowed_models": ["claude-*"], "ttl_secs": 3600}))).await;
    assert_eq!(status, 200);
    let key = issued["key"].as_str().unwrap().to_string();
    let id = issued["record"]["id"].as_str().unwrap().to_string();
    let (_, list, _) = h.call("GET", "/admin/keys", Some(&h.admin), None).await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert!(list[0].get("hash").is_none(), "hashes never leave the server");

    let (status, rotated, _) = h.call("POST", &format!("/admin/keys/{id}/rotate"), Some(&h.admin), Some(json!({"grace_secs": 0}))).await;
    assert_eq!(status, 200);
    assert!(h.state.keys.authenticate(&key).is_none());
    assert!(h.state.keys.authenticate(rotated["key"].as_str().unwrap()).is_some());
    assert_eq!(h.call("DELETE", &format!("/admin/keys/{id}"), Some(&h.admin), None).await.0, 200);
    assert_eq!(h.call("DELETE", &format!("/admin/keys/{id}"), Some(&h.admin), None).await.0, 404);

    // API-key account import and listing.
    let (status, row, _) = h
        .call(
            "POST",
            "/admin/accounts/import",
            Some(&h.admin),
            Some(json!({"provider": "opencode-go", "data": {"api_key": "ocg"}, "label": "go"})),
        )
        .await;
    assert_eq!(status, 200, "{row}");
    let acct = row["id"].as_str().unwrap().to_string();
    let (_, accounts, _) = h.call("GET", "/admin/accounts", Some(&h.admin), None).await;
    assert_eq!(accounts[0]["label"], "go");
    assert_eq!(accounts[0]["available"], true);
    assert_eq!(h.call("POST", &format!("/admin/accounts/{acct}/disable"), Some(&h.admin), None).await.0, 200);
    let (_, accounts, _) = h.call("GET", "/admin/accounts?provider=opencode-go", Some(&h.admin), None).await;
    assert_eq!(accounts[0]["available"], false);
    assert_eq!(h.call("DELETE", &format!("/admin/accounts/{acct}"), Some(&h.admin), None).await.0, 200);

    let (status, usage, _) = h.call("GET", "/admin/usage?group_by=model", Some(&h.admin), None).await;
    assert_eq!(status, 200, "{usage}");
}
