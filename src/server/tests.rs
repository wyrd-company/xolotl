use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::Request;
use serde_json::{Value, json};
use tower::ServiceExt;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::config::{CatalogConfig, Endpoints, GatewayConfig, ProviderConfig, ProviderKind};
use crate::db::now;
use crate::keys::IssueRequest;
use crate::policy::{AliasTargets, Policy};
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
        },
        catalog: CatalogConfig {
            static_models: BTreeMap::from([
                ("claude".to_string(), vec!["claude-haiku-5-5".to_string(), "claude-opus-5".to_string()]),
                ("codex".to_string(), vec!["gpt-6-codex".to_string()]),
                ("opencode-go".to_string(), vec!["claude-haiku-5-5".to_string(), "kimi-k3".to_string()]),
                ("llama".to_string(), vec!["qwen3".to_string(), "busy".to_string()]),
            ]),
            ..Default::default()
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
    let http = crate::proxy::upstream_client(std::time::Duration::from_secs(30)).unwrap();
    let keys = Arc::new(KeyManager::new(secrets.clone()));
    keys.reload().await.unwrap();
    let subs = Arc::new(Subscriptions::new(cfg.subscription.clone(), db.clone(), secrets.clone(), http.clone()));
    let gateway = Arc::new(Gateway::new(&cfg.gateway, secrets.clone(), http, db.clone()));
    let catalog = Arc::new(ModelCatalog::new(cfg.catalog.clone(), gateway.kinds()));
    let state = AppState { cfg: Arc::new(cfg), db, secrets, keys, subs, gateway, catalog, vault: None, sessions: Default::default() };
    Harness { state, admin: "boot".into(), upstream }
}

fn policy(name: &str) -> Policy {
    Policy { name: name.into(), providers: vec!["*".into()], ..Default::default() }
}

impl Harness {
    async fn call_with(&self, method: &str, uri: &str, headers: &[(&str, &str)], body: Option<Value>) -> (u16, Value) {
        let mut req = Request::builder().method(method).uri(uri);
        for (k, v) in headers {
            req = req.header(*k, *v);
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
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v = serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into()));
        (status, v)
    }

    async fn call(&self, method: &str, uri: &str, key: Option<&str>, body: Option<Value>) -> (u16, Value) {
        let auth = key.map(|k| format!("Bearer {k}"));
        let headers: Vec<(&str, &str)> = auth.iter().map(|a| ("authorization", a.as_str())).collect();
        self.call_with(method, uri, &headers, body).await
    }

    /// Create the policy and issue a key under it; returns the key read from the vault.
    async fn key(&self, p: Policy) -> String {
        let name = p.name.clone();
        self.state.keys.put_policy(p, &self.state.catalog.known_providers()).await.unwrap();
        let info = self.state.keys.issue(IssueRequest { policy: name, name: "client".into(), ..Default::default() }).await.unwrap();
        let rec = self.state.secrets.get(&info.vault_path).await.unwrap().unwrap();
        rec["api_key"].as_str().unwrap().to_string()
    }

    async fn account(&self, cred: Credential) -> String {
        self.state.subs.save_account(cred).await.unwrap().id
    }

    async fn codex_account(&self) -> String {
        self.account(Credential {
            provider: SubProvider::Codex,
            access_token: "codex-tok".into(),
            refresh_token: Some("rt".into()),
            expires_at: Some(now() + 3600),
            account_id: Some("acct-1".into()),
            email: Some("me@x.com".into()),
            plan: Some("pro".into()),
            ..Default::default()
        })
        .await
    }
}

#[tokio::test]
async fn health_and_auth() {
    let h = harness().await;
    assert_eq!(h.call("GET", "/healthz", None, None).await.0, 200);
    assert_eq!(h.call("GET", "/readyz", None, None).await.0, 200);
    assert_eq!(h.call("POST", "/v1/chat/completions", None, Some(json!({}))).await.0, 401);
    assert_eq!(h.call("POST", "/v1/chat/completions", Some("xol_nope"), Some(json!({}))).await.0, 401);
    let proxy_key = h.key(policy("p")).await;
    assert_eq!(h.call("GET", "/admin/keys", Some(&proxy_key), None).await.0, 403);
    assert_eq!(h.call("GET", "/v0/management/auth-files", Some(&proxy_key), None).await.0, 401);
}

#[tokio::test]
async fn gateway_routing_respects_policy() {
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
    let key = h.key(Policy { name: "local".into(), providers: vec!["llama".into()], ..Default::default() }).await;
    // Plain name (unique in the catalog) and explicit form both work.
    assert_eq!(h.call("POST", "/v1/chat/completions", Some(&key), Some(json!({"model": "qwen3", "messages": []}))).await.0, 200);
    assert_eq!(h.call("POST", "/v1/chat/completions", Some(&key), Some(json!({"model": "llama/qwen3", "messages": []}))).await.0, 200);
    // Providers outside the policy are invisible.
    let (status, body) = h.call("POST", "/v1/responses", Some(&key), Some(json!({"model": "gpt-6-codex"}))).await;
    assert_eq!(status, 404, "{body}");
}

#[tokio::test]
async fn precedence_decides_plain_names_and_prefix_reaches_losers() {
    let h = harness().await;
    h.account(Credential {
        provider: SubProvider::Claude,
        access_token: "claude-tok".into(),
        expires_at: Some(now() + 3600),
        account_id: Some("u1".into()),
        ..Default::default()
    })
    .await;
    h.account(Credential { provider: SubProvider::OpencodeGo, api_key: Some("ocg-key".into()), ..Default::default() }).await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("authorization", "Bearer claude-tok"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"from": "claude"})))
        .mount(&h.upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/zen/go/v1/messages"))
        .and(header("x-api-key", "ocg-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"from": "opencode-go"})))
        .mount(&h.upstream)
        .await;
    let body = json!({"model": "claude-haiku-5-5", "messages": []});

    let default_key = h.key(policy("default")).await;
    let (status, v) = h.call("POST", "/v1/messages", Some(&default_key), Some(body.clone())).await;
    assert_eq!((status, v["from"].as_str()), (200, Some("claude")), "subscription default order");
    let (_, v) = h.call("POST", "/v1/messages", Some(&default_key), Some(json!({"model": "opencode-go/claude-haiku-5-5"}))).await;
    assert_eq!(v["from"], "opencode-go", "explicit prefix reaches the losing provider");

    let mut p = policy("go-first");
    p.precedence = vec!["opencode-go".into()];
    let go_key = h.key(p).await;
    let (_, v) = h.call("POST", "/v1/messages", Some(&go_key), Some(body)).await;
    assert_eq!(v["from"], "opencode-go");

    // Claude Code's model list, in Anthropic's shape, marks translated routes.
    let auth = format!("Bearer {default_key}");
    let (status, list) = h.call_with("GET", "/v1/models", &[("authorization", &auth), ("anthropic-version", "2023-06-01")], None).await;
    assert_eq!(status, 200);
    let data = list["data"].as_array().unwrap();
    let entry = |id: &str| data.iter().find(|m| m["id"] == id).unwrap_or_else(|| panic!("{id} missing from {list}")).clone();
    assert_eq!(entry("claude-opus-5")["type"], "model");
    assert_eq!(entry("claude-opus-5")["endpoints"], json!(["/v1/messages"]));
    assert_eq!(entry("gpt-6-codex")["endpoints"], json!(["/v1/responses"]));
    assert_eq!(entry("gpt-6-codex")["translated_endpoints"], json!(["/v1/messages", "/v1/chat/completions"]));
    let (_, list) = h.call("GET", "/v1/models", Some(&default_key), None).await;
    let kimi = list["data"].as_array().unwrap().iter().find(|m| m["id"] == "kimi-k3").unwrap().clone();
    assert_eq!(kimi["object"], "model");
    assert_eq!(kimi["translated_endpoints"], json!(["/v1/messages", "/v1/responses"]));
}

#[tokio::test]
async fn untranslatable_endpoint_explains_where_a_model_lives() {
    let h = harness().await;
    let key = h.key(policy("p")).await;
    let (status, body) = h.call("POST", "/v1/messages/count_tokens", Some(&key), Some(json!({"model": "gpt-6-codex"}))).await;
    assert_eq!(status, 400);
    assert!(body.to_string().contains("/v1/responses"), "{body}");
    assert_eq!(body["type"], "error", "Anthropic envelope on /v1/messages");
}

#[tokio::test]
async fn messages_client_reaches_codex_through_translation() {
    let h = harness().await;
    h.codex_account().await;
    let stream = [
        json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "message"}}),
        json!({"type": "response.output_text.delta", "delta": "from codex"}),
        json!({"type": "response.output_item.done", "item": {"type": "message"}}),
        json!({"type": "response.completed", "response": {"status": "completed", "usage": {"input_tokens": 11, "output_tokens": 5}}}),
    ]
    .iter()
    .map(|e| format!("data: {e}\n\n"))
    .collect::<String>();
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/responses"))
        .and(header("authorization", "Bearer codex-tok"))
        .and(body_partial_json(json!({"model": "gpt-6-codex", "stream": true, "store": false})))
        .respond_with(ResponseTemplate::new(200).set_body_raw(stream, "text/event-stream"))
        .mount(&h.upstream)
        .await;
    let key = h.key(policy("p")).await;
    let body = json!({"model": "gpt-6-codex", "max_tokens": 100, "stream": true, "system": "sys", "messages": [{"role": "user", "content": "hi"}]});
    let (status, out) = h.call_with("POST", "/v1/messages", &[("x-api-key", &key), ("anthropic-version", "2023-06-01")], Some(body)).await;
    assert_eq!(status, 200, "{out}");
    let text = out.as_str().unwrap();
    assert!(text.starts_with("event: message_start"), "{text}");
    assert!(text.contains(r#""text":"from codex""#), "{text}");
    assert!(text.contains(r#""stop_reason":"end_turn""#), "{text}");

    // Sent in Codex's format, without the client's Anthropic headers.
    let reqs = h.upstream.received_requests().await.unwrap();
    let sent = reqs.iter().find(|r| r.url.path() == "/backend-api/codex/responses").unwrap();
    assert!(sent.headers.get("anthropic-version").is_none());
    let sent: Value = serde_json::from_slice(&sent.body).unwrap();
    assert_eq!(sent["input"][0]["role"], "developer");
    assert_eq!(sent["input"][1]["content"][0], json!({"type": "input_text", "text": "hi"}));

    // Usage is recorded from the upstream response, against the upstream endpoint.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let rows = h.state.db.usage_summary("model", 0).await.unwrap();
    let row = rows.iter().find(|r| r.key.as_deref() == Some("gpt-6-codex")).unwrap_or_else(|| panic!("{rows:?}"));
    assert_eq!((row.input_tokens, row.output_tokens), (11, 5));
}

#[tokio::test]
async fn claude_accounts_serve_only_the_claude_code_path() {
    let mut h = harness().await;
    // A wire override can't put Claude accounts on another endpoint (config validation also rejects it).
    let mut catalog = h.state.cfg.catalog.clone();
    catalog.wires.insert("claude".into(), vec![Wire::Chat, Wire::Responses]);
    h.state.catalog = Arc::new(ModelCatalog::new(catalog, h.state.gateway.kinds()));
    h.account(Credential {
        provider: SubProvider::Claude,
        access_token: "claude-tok".into(),
        expires_at: Some(now() + 3600),
        account_id: Some("u1".into()),
        ..Default::default()
    })
    .await;
    h.account(Credential { provider: SubProvider::OpencodeGo, api_key: Some("ocg-key".into()), ..Default::default() }).await;
    Mock::given(method("POST"))
        .and(path("/zen/go/v1/messages"))
        .and(header("x-api-key", "ocg-key"))
        .and(body_partial_json(json!({
            "model": "claude-haiku-5-5",
            "thinking": {"type": "adaptive"},
            "output_config": {"effort": "high"},
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1", "type": "message", "role": "assistant", "content": [{"type": "text", "text": "hello"}],
            "stop_reason": "end_turn", "usage": {"input_tokens": 2, "output_tokens": 3},
        })))
        .mount(&h.upstream)
        .await;
    let mut p = policy("p");
    p.aliases.insert("haiku".into(), AliasTargets::Many(vec!["claude/claude-haiku-5-5".into(), "opencode-go/claude-haiku-5-5".into()]));
    let key = h.key(p).await;
    let chat = |model: &str| json!({"model": model, "reasoning_effort": "high", "messages": [{"role": "user", "content": "hi"}]});

    // A Claude subscription account is never translated: the other endpoints get a 400 naming the Messages path.
    for (path, body) in
        [("/v1/chat/completions", chat("claude-opus-5")), ("/v1/responses", json!({"model": "claude-opus-5", "input": "hi"}))]
    {
        let (status, out) = h.call("POST", path, Some(&key), Some(body)).await;
        assert_eq!(status, 400, "{path}: {out}");
        assert!(out["error"]["message"].as_str().unwrap().contains("/v1/messages"), "{out}");
    }
    let (_, models) = h.call("GET", "/v1/models", Some(&key), None).await;
    let opus = models["data"].as_array().unwrap().iter().find(|m| m["id"] == "claude-opus-5").unwrap().clone();
    assert_eq!((opus["endpoints"].clone(), opus["translated_endpoints"].clone()), (json!(["/v1/messages"]), json!([])));

    // The same model family on another provider is translated.
    let (status, out) = h.call("POST", "/v1/chat/completions", Some(&key), Some(chat("opencode-go/claude-haiku-5-5"))).await;
    assert_eq!(status, 200, "{out}");
    assert_eq!(out["object"], "chat.completion");
    assert_eq!(out["choices"][0]["message"]["content"], "hello");
    assert_eq!(out["usage"]["total_tokens"], 5);
    // An alias over both skips the Claude account off the Claude Code path.
    let (status, out) = h.call("POST", "/v1/chat/completions", Some(&key), Some(chat("haiku"))).await;
    assert_eq!(status, 200, "{out}");
    let claude_calls = h.upstream.received_requests().await.unwrap().iter().filter(|r| r.url.path() == "/v1/messages").count();
    assert_eq!(claude_calls, 0, "nothing reached the Claude account");
}

#[tokio::test]
async fn responses_client_reaches_a_chat_provider_through_translation() {
    let h = harness().await;
    Mock::given(method("POST"))
        .and(path("/llama/v1/chat/completions"))
        .and(header("authorization", "Bearer llama-key"))
        .and(body_partial_json(json!({"model": "qwen3", "messages": [{"role": "system", "content": "sys"}, {"role": "user", "content": "hi"}]})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"index": 0, "message": {"role": "assistant", "content": null,
                "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "f", "arguments": "{}"}}]}, "finish_reason": "tool_calls"}],
            "usage": {"prompt_tokens": 4, "completion_tokens": 1},
        })))
        .mount(&h.upstream)
        .await;
    let key = h.key(policy("p")).await;
    let body = json!({"model": "qwen3", "instructions": "sys", "input": "hi", "tools": [{"type": "function", "name": "f", "parameters": {"type": "object"}}]});
    let (status, out) = h.call("POST", "/v1/responses", Some(&key), Some(body)).await;
    assert_eq!(status, 200, "{out}");
    assert_eq!(out["object"], "response");
    assert_eq!(out["status"], "completed");
    assert_eq!(out["output"][0]["type"], "function_call");
    assert_eq!(out["output"][0]["call_id"], "c1");
}

#[tokio::test]
async fn alias_falls_back_between_targets() {
    let h = harness().await;
    Mock::given(path("/llama/v1/chat/completions"))
        .and(body_partial_json(json!({"model": "busy"})))
        .respond_with(ResponseTemplate::new(429))
        .mount(&h.upstream)
        .await;
    Mock::given(path("/llama/v1/chat/completions"))
        .and(body_partial_json(json!({"model": "qwen3"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .mount(&h.upstream)
        .await;
    let mut p = policy("p");
    p.aliases.insert("fast".into(), AliasTargets::Many(vec!["llama/busy".into(), "llama/qwen3".into()]));
    let key = h.key(p).await;
    let (status, body) = h.call("POST", "/v1/chat/completions", Some(&key), Some(json!({"model": "fast"}))).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["ok"], true);
}

#[tokio::test]
async fn passthroughs_swap_credentials() {
    let h = harness().await;
    Mock::given(method("GET"))
        .and(path("/llama/v1/props"))
        .and(header("authorization", "Bearer llama-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .mount(&h.upstream)
        .await;
    h.account(Credential { provider: SubProvider::OpencodeGo, api_key: Some("ocg-key".into()), ..Default::default() }).await;
    Mock::given(method("GET"))
        .and(path("/zen/go/v1/models"))
        .and(header("authorization", "Bearer ocg-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [{"id": "kimi-k3"}]})))
        .mount(&h.upstream)
        .await;
    let key = h.key(policy("p")).await;
    let (status, body) = h.call("GET", "/llama/v1/props", Some(&key), None).await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = h.call("GET", "/opencode-go/v1/models", Some(&key), None).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["data"][0]["id"], "kimi-k3");
    assert_eq!(h.call("GET", "/nope/v1/props", Some(&key), None).await.0, 404);

    let restricted = h.key(Policy { name: "r".into(), providers: vec!["claude".into()], ..Default::default() }).await;
    assert_eq!(h.call("GET", "/llama/v1/props", Some(&restricted), None).await.0, 403);
    assert_eq!(h.call("GET", "/opencode-go/v1/models", Some(&restricted), None).await.0, 403);
}

#[tokio::test]
async fn client_credentials_never_reach_upstream() {
    let h = harness().await;
    Mock::given(method("GET")).and(path("/llama/v1/props")).respond_with(ResponseTemplate::new(200)).mount(&h.upstream).await;
    // The bootstrap token sent as x-management-key authenticates the request
    // and must be stripped like the other client credential headers.
    let (status, body) = h.call_with("GET", "/llama/v1/props", &[("x-management-key", h.admin.as_str())], None).await;
    assert_eq!(status, 200, "{body}");
    let reqs = h.upstream.received_requests().await.unwrap();
    let sent = &reqs.last().unwrap().headers;
    assert!(sent.get("x-management-key").is_none(), "client credential forwarded upstream");
    assert_eq!(sent.get("authorization").unwrap(), "Bearer llama-key");
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
    let key = h.key(Policy { name: "codex".into(), providers: vec!["codex".into()], ..Default::default() }).await;
    let (status, body) = h.call("POST", "/v1/responses", Some(&key), Some(json!({"model": "gpt-6-codex", "input": []}))).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(h.call("GET", "/v1/responses", Some(&key), None).await.0, 426);
}

#[tokio::test]
async fn messages_accept_x_api_key() {
    let h = harness().await;
    let key = h.key(policy("cc")).await;
    let (status, v) = h.call_with("POST", "/v1/messages", &[("x-api-key", &key)], Some(json!({"model": "claude-opus-5"}))).await;
    // Authenticated, but no Claude accounts yet: Anthropic-shaped 503.
    assert_eq!(status, 503);
    assert_eq!(v["type"], "error");
}

#[tokio::test]
async fn t3code_management_endpoints() {
    let h = harness().await;
    let id = h.codex_account().await;
    let mgmt =
        h.key(Policy { name: "t3".into(), providers: vec!["codex".into()], scopes: vec![Scope::Management], ..Default::default() }).await;

    let (status, body) = h.call("GET", "/v0/management/auth-files", Some(&mgmt), None).await;
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
    let (status, body) = h.call("POST", "/v0/management/api-call", Some(&mgmt), Some(call)).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["status_code"], 200);
    let inner: Value = serde_json::from_str(body["body"].as_str().unwrap()).unwrap();
    assert_eq!(inner["rate_limit"]["primary_window"]["used_percent"], 5);

    let (status, _) = h
        .call("POST", "/v0/management/api-call", Some(&mgmt), Some(json!({"auth_index": "ffff", "method": "GET", "url": "https://x"})))
        .await;
    assert_eq!(status, 400);

    h.state.db.set_cooldown(&id, now() + 999, "test").await.unwrap();
    let (status, body) = h.call("POST", "/v0/management/reset-quota", Some(&mgmt), Some(json!({"auth_index": idx}))).await;
    assert_eq!(status, 200);
    assert_eq!(body["status"], "ok");
    assert!(!h.state.db.get_account(&id).await.unwrap().unwrap().cooling_down(now()));
    let (status, _) = h.call("POST", "/v0/management/reset-quota", Some(&mgmt), Some(json!({"auth_index": "nope"}))).await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn admin_policies_keys_and_accounts() {
    let h = harness().await;
    // Policies: validation, view, deletion guard.
    let bad = json!({"name": "dev", "providers": ["nope"]});
    assert_eq!(h.call("PUT", "/admin/policies/dev", Some(&h.admin), Some(bad)).await.0, 400);
    let dev = json!({"name": "dev", "providers": ["claude", "opencode-go"], "aliases": {"haiku-go": "opencode-go/claude-haiku-5-5"}});
    let (status, body) = h.call("PUT", "/admin/policies/dev", Some(&h.admin), Some(dev)).await;
    assert_eq!(status, 200, "{body}");
    let (_, view) = h.call("GET", "/admin/policies/dev/view", Some(&h.admin), None).await;
    let names: Vec<&str> = view["models"].as_array().unwrap().iter().map(|m| m["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["claude-haiku-5-5", "claude-opus-5", "haiku-go", "kimi-k3"]);

    // Keys: issued into the vault, never returned.
    let (status, issued) =
        h.call("POST", "/admin/keys", Some(&h.admin), Some(json!({"policy": "dev", "name": "laptop", "ttl_secs": 3600}))).await;
    assert_eq!(status, 200, "{issued}");
    assert!(issued.get("api_key").is_none());
    assert_eq!(issued["vault_path"], "keys/dev/laptop");
    let key = h.state.secrets.get("keys/dev/laptop").await.unwrap().unwrap()["api_key"].as_str().unwrap().to_string();
    assert!(h.state.keys.authenticate(&key).await.is_some());
    assert_eq!(h.call("DELETE", "/admin/policies/dev", Some(&h.admin), None).await.0, 400, "policy has keys");

    assert_eq!(h.call("POST", "/admin/keys/dev/laptop/rotate", Some(&h.admin), Some(json!({"grace_secs": 0}))).await.0, 200);
    assert!(h.state.keys.authenticate(&key).await.is_none());
    assert_eq!(h.call("DELETE", "/admin/keys/dev/laptop", Some(&h.admin), None).await.0, 200);
    assert_eq!(h.call("DELETE", "/admin/keys/dev/laptop", Some(&h.admin), None).await.0, 404);
    assert_eq!(h.call("DELETE", "/admin/policies/dev", Some(&h.admin), None).await.0, 200);

    // API-key account import and listing.
    let (status, row) = h
        .call(
            "POST",
            "/admin/accounts/import",
            Some(&h.admin),
            Some(json!({"provider": "opencode-go", "data": {"api_key": "ocg"}, "label": "go"})),
        )
        .await;
    assert_eq!(status, 200, "{row}");
    let acct = row["id"].as_str().unwrap().to_string();
    let (_, accounts) = h.call("GET", "/admin/accounts", Some(&h.admin), None).await;
    assert_eq!(accounts[0]["label"], "go");
    assert_eq!(accounts[0]["available"], true);
    assert_eq!(h.call("POST", &format!("/admin/accounts/{acct}/disable"), Some(&h.admin), None).await.0, 200);
    let (_, accounts) = h.call("GET", "/admin/accounts?provider=opencode-go", Some(&h.admin), None).await;
    assert_eq!(accounts[0]["available"], false);
    assert_eq!(h.call("DELETE", &format!("/admin/accounts/{acct}"), Some(&h.admin), None).await.0, 200);

    let (status, catalog) = h.call("GET", "/admin/catalog", Some(&h.admin), None).await;
    assert_eq!(status, 200);
    assert!(catalog["codex"].as_array().unwrap().iter().any(|m| m == "gpt-6-codex"));
    assert_eq!(h.call("GET", "/admin/usage?group_by=model", Some(&h.admin), None).await.0, 200);
}

#[tokio::test]
async fn ui_session_needs_password_cookie_and_csrf() {
    let h = harness().await;
    let (_, s) = h.call_with("GET", "/admin/session", &[], None).await;
    assert_eq!(s, json!({"authenticated": false, "password_set": false}));
    assert_eq!(h.call_with("POST", "/admin/session", &[], Some(json!({"password": "x"}))).await.0, 409, "no password set yet");

    // Only an admin can set the password, and only as an argon2id hash.
    let hash = session::hash_password("correct horse battery").unwrap();
    let req = json!({"password_hash": hash});
    assert_eq!(h.call_with("PUT", "/admin/ui/password", &[], Some(req.clone())).await.0, 401);
    assert_eq!(h.call("PUT", "/admin/ui/password", Some(&h.admin), Some(json!({"password_hash": "plain"}))).await.0, 400);
    assert_eq!(h.call("PUT", "/admin/ui/password", Some(&h.admin), Some(req)).await.0, 200);
    let admin = h.state.secrets.get("admin").await.unwrap().unwrap();
    assert_eq!(admin["token"], "boot", "the bootstrap token is kept");

    assert_eq!(h.call_with("POST", "/admin/session", &[], Some(json!({"password": "wrong"}))).await.0, 401);
    let resp = router(h.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/session")
                .header("content-type", "application/json")
                .header("x-forwarded-proto", "https")
                .body(Body::from(json!({"password": "correct horse battery"}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let set_cookie = resp.headers()["set-cookie"].to_str().unwrap().to_string();
    assert!(set_cookie.contains("HttpOnly") && set_cookie.contains("SameSite=Strict") && set_cookie.contains("Secure"), "{set_cookie}");
    let cookie = set_cookie.split(';').next().unwrap().to_string();
    let body: Value = serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    let csrf = body["csrf_token"].as_str().unwrap().to_string();

    // The cookie alone is not enough; with the CSRF token it is admin, and only admin.
    assert_eq!(h.call_with("GET", "/admin/keys", &[("cookie", &cookie)], None).await.0, 401);
    assert_eq!(h.call_with("GET", "/admin/keys", &[("cookie", &cookie), ("x-csrf-token", "nope")], None).await.0, 401);
    let session = [("cookie", cookie.as_str()), ("x-csrf-token", csrf.as_str())];
    assert_eq!(h.call_with("GET", "/admin/keys", &session, None).await.0, 200);
    assert_eq!(h.call_with("GET", "/v1/models", &session, None).await.0, 401);
    let (_, s) = h.call_with("GET", "/admin/session", &[("cookie", &cookie)], None).await;
    assert_eq!(s["csrf_token"], csrf.as_str());

    // Changing the password ends every session.
    let hash = session::hash_password("another long password").unwrap();
    assert_eq!(h.call_with("PUT", "/admin/ui/password", &session, Some(json!({"password_hash": hash}))).await.0, 200);
    assert_eq!(h.call_with("GET", "/admin/keys", &session, None).await.0, 401);
}

#[tokio::test]
async fn ui_sign_in_checked_against_a_replaced_password_gets_no_session() {
    use crate::secrets::SecretStore;
    let h = harness().await;
    let gated = Arc::new(crate::secrets::testing::GatedStore::default());
    let old = session::hash_password("correct horse battery").unwrap();
    gated.inner.put("admin", json!({"token": "boot", "ui_password_hash": old}).as_object().unwrap().clone()).await.unwrap();
    let mut state = h.state.clone();
    state.secrets = gated.clone();

    // The sign-in reads the old hash, then pauses before checking it.
    gated.arm("got", "admin");
    let login = tokio::spawn(
        router(state.clone()).oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/session")
                .header("content-type", "application/json")
                .body(Body::from(json!({"password": "correct horse battery"}).to_string()))
                .unwrap(),
        ),
    );
    gated.reached.notified().await;

    // Meanwhile the password changes.
    let new = session::hash_password("another long password").unwrap();
    let change = router(state.clone())
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/admin/ui/password")
                .header("authorization", "Bearer boot")
                .header("content-type", "application/json")
                .body(Body::from(json!({"password_hash": new}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(change.status(), 200);

    gated.release.notify_one();
    let resp = login.await.unwrap().unwrap();
    assert_eq!(resp.status(), 409);
    assert!(resp.headers().get("set-cookie").is_none(), "no session for the old password");
}

#[tokio::test]
async fn ui_logout_and_lockout() {
    let h = harness().await;
    let hash = session::hash_password("correct horse battery").unwrap();
    h.call("PUT", "/admin/ui/password", Some(&h.admin), Some(json!({"password_hash": hash}))).await;
    let resp = router(h.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/session")
                .header("content-type", "application/json")
                .body(Body::from(json!({"password": "correct horse battery"}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let cookie = resp.headers()["set-cookie"].to_str().unwrap().split(';').next().unwrap().to_string();
    let body: Value = serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    let csrf = body["csrf_token"].as_str().unwrap().to_string();
    let session = [("cookie", cookie.as_str()), ("x-csrf-token", csrf.as_str())];
    assert_eq!(h.call_with("DELETE", "/admin/session", &session, None).await.0, 200);
    assert_eq!(h.call_with("GET", "/admin/keys", &session, None).await.0, 401);

    for _ in 0..5 {
        assert_eq!(h.call_with("POST", "/admin/session", &[], Some(json!({"password": "guess"}))).await.0, 401);
    }
    let (status, _) = h.call_with("POST", "/admin/session", &[], Some(json!({"password": "correct horse battery"}))).await;
    assert_eq!(status, 429, "locked out even with the right password");
}

#[tokio::test]
async fn ui_admin_endpoints() {
    let h = harness().await;
    let (status, s) = h.call("GET", "/admin/status", Some(&h.admin), None).await;
    assert_eq!(status, 200, "{s}");
    assert_eq!(s["database"]["ok"], true);
    assert_eq!(s["subscription"]["strategy"], "round-robin");

    let (_, providers) = h.call("GET", "/admin/providers", Some(&h.admin), None).await;
    assert_eq!(
        providers,
        json!([{
            "name": "llama", "kind": "openai-compatible", "enabled": true, "base_url": format!("{}/llama/v1", h.upstream.uri()),
            "no_auth": false, "secret": "providers/llama", "has_key": true,
        }])
    );

    let (_, routes) = h.call("GET", "/admin/catalog/routes", Some(&h.admin), None).await;
    let codex = routes.as_array().unwrap().iter().find(|r| r["model"] == "gpt-6-codex").unwrap();
    assert_eq!(codex["endpoints"], json!(["/v1/responses"]));
    assert_eq!(codex["translated_endpoints"], json!(["/v1/messages", "/v1/chat/completions"]));

    // Preview an unsaved policy: the resolved names, and who wins shared names.
    let draft = json!({"name": "", "providers": ["claude", "opencode-go"], "winners": {"claude-haiku-5-5": "opencode-go"}});
    let (status, view) = h.call("POST", "/admin/policy-preview", Some(&h.admin), Some(draft)).await;
    assert_eq!(status, 200, "{view}");
    let haiku = view["models"].as_array().unwrap().iter().find(|m| m["name"] == "claude-haiku-5-5").unwrap();
    assert_eq!(haiku["targets"], json!(["opencode-go/claude-haiku-5-5"]));
    assert_eq!(view["shared"], json!([{"model": "claude-haiku-5-5", "providers": ["claude", "opencode-go"], "winner": "opencode-go"}]));
    let (status, err) = h.call("POST", "/admin/policy-preview", Some(&h.admin), Some(json!({"name": "x", "providers": ["nope"]}))).await;
    assert_eq!(status, 400);
    assert!(err["error"]["message"].as_str().unwrap().contains("unknown provider"));

    // `preview` is an ordinary policy name.
    let preview = json!({"name": "preview", "providers": ["*"]});
    assert_eq!(h.call("PUT", "/admin/policies/preview", Some(&h.admin), Some(preview)).await.0, 200);
    assert_eq!(h.call("GET", "/admin/policies/preview", Some(&h.admin), None).await.0, 200);
    assert_eq!(h.call("GET", "/admin/policies/preview/view", Some(&h.admin), None).await.0, 200);
    assert_eq!(h.call("DELETE", "/admin/policies/preview", Some(&h.admin), None).await.0, 200);

    // The UI page is public (the API behind it is not), with a strict CSP.
    let resp = router(h.state.clone()).oneshot(Request::builder().uri("/").body(Body::empty()).unwrap()).await.unwrap();
    assert!(resp.headers()["content-type"].to_str().unwrap().starts_with("text/html"));
    assert!(resp.headers()["content-security-policy"].to_str().unwrap().contains("frame-ancestors 'none'"));
    assert_eq!(h.call("GET", "/assets/../../Cargo.toml", None, None).await.0, 404);
}
