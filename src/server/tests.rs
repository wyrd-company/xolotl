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
use crate::limits::Limits;
use crate::policy::{AliasTargets, Policy};
use crate::secrets::MemoryStore;
use crate::subscription::{Credential, auth_index};

struct Harness {
    state: AppState,
    admin: String,
    upstream: MockServer,
}

async fn harness() -> Harness {
    harness_with(|_| {}).await
}

async fn harness_with(tweak: impl FnOnce(&mut Config)) -> Harness {
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
                ("grok".to_string(), vec!["grok-build".to_string()]),
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
        grok_api: format!("{uri}/grok/v1"),
        grok_auth: uri.clone(),
    };
    tweak(&mut cfg);
    let db = Db::in_memory().await.unwrap();
    let http = crate::proxy::upstream_client(std::time::Duration::from_secs(30)).unwrap();
    let keys = Arc::new(KeyManager::new(secrets.clone()));
    keys.reload().await.unwrap();
    let limiter = Arc::new(Limiter::default());
    let subs =
        Arc::new(Subscriptions::new(cfg.subscription.clone(), db.clone(), secrets.clone(), http.clone()).with_limiter(limiter.clone()));
    let gateway = Arc::new(Gateway::new(&cfg.gateway, secrets.clone(), http, db.clone()).with_limiter(limiter.clone()));
    let catalog = Arc::new(ModelCatalog::new(cfg.catalog.clone(), gateway.kinds()));
    let mcp = Arc::new(McpHub::new(&cfg, secrets.clone(), db.clone()));
    let state =
        AppState { cfg: Arc::new(cfg), db, secrets, keys, subs, gateway, catalog, vault: None, sessions: Default::default(), mcp, limiter };
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
        self.state.keys.put_policy(p, &self.state.catalog.known_providers(), &self.state.mcp.server_names()).await.unwrap();
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
    let rows = h.state.db.usage_summary("model", 0, &Default::default()).await.unwrap();
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
async fn grok_shell_gets_its_model_shape_limited_by_policy() {
    let h = harness().await;
    let key = h.key(Policy { name: "grok".into(), providers: vec!["grok".into(), "codex".into()], ..Default::default() }).await;
    let auth = format!("Bearer {key}");
    let ua = ("user-agent", "grok-shell/1.0.52 (linux; x86_64)");
    let (status, v) = h.call_with("GET", "/v1/models", &[("authorization", &auth), ua], None).await;
    assert_eq!(status, 200, "{v}");
    let data = v["data"].as_array().unwrap();
    assert_eq!(data.len(), 2, "only the policy's names: {v}");
    let grok = data.iter().find(|m| m["id"] == "grok-build").unwrap();
    assert_eq!(
        *grok,
        json!({"id": "grok-build", "model": "grok-build", "name": "grok-build", "api_backend": "responses", "supported_in_api": true})
    );
    // Other clients keep the OpenAI shape.
    let (_, v) = h.call("GET", "/v1/models", Some(&key), None).await;
    assert_eq!(v["data"][0]["object"], "model");

    // Names no target can serve on Responses are left out.
    let key = h.key(policy("all")).await;
    let auth = format!("Bearer {key}");
    let (_, v) = h.call_with("GET", "/v1/models", &[("authorization", &auth), ua], None).await;
    let ids: Vec<&str> = v["data"].as_array().unwrap().iter().map(|m| m["id"].as_str().unwrap()).collect();
    for id in ["claude-opus-5", "claude-haiku-5-5"] {
        assert!(!ids.contains(&id), "{id} belongs to the Claude pool, which serves only /v1/messages: {ids:?}");
    }
    for id in ["grok-build", "gpt-6-codex", "qwen3", "kimi-k3"] {
        assert!(ids.contains(&id), "{id} reaches Responses natively or translated: {ids:?}");
    }
}

#[tokio::test]
async fn native_responses_clients_get_json_unless_they_stream() {
    let h = harness().await;
    h.account(Credential {
        provider: SubProvider::Grok,
        access_token: "xai-tok".into(),
        refresh_token: Some("rt".into()),
        expires_at: Some(now() + 3600),
        account_id: Some("sub-1".into()),
        ..Default::default()
    })
    .await;
    let stream = [
        json!({"type": "response.created", "response": {"id": "resp_1", "status": "in_progress"}}),
        json!({"type": "keepalive"}),
        json!({"type": "response.output_item.done", "output_index": 0, "item": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "hi there"}]}}),
        json!({"type": "response.completed", "response": {"id": "resp_1", "status": "completed", "output": [], "usage": {"input_tokens": 4, "output_tokens": 2}}}),
    ]
    .iter()
    .map(|e| format!("data: {e}\n\n"))
    .collect::<String>();
    Mock::given(method("POST"))
        .and(path("/grok/v1/responses"))
        .and(body_partial_json(json!({"stream": true})))
        .respond_with(ResponseTemplate::new(200).set_body_raw(stream, "text/event-stream"))
        .mount(&h.upstream)
        .await;
    let key = h.key(policy("p")).await;
    for body in [json!({"model": "grok-build", "input": "hi"}), json!({"model": "grok-build", "input": "hi", "stream": false})] {
        let (status, out) = h.call("POST", "/v1/responses", Some(&key), Some(body.clone())).await;
        assert_eq!(status, 200, "{out}");
        assert!(out.is_object(), "JSON for {body}: {out}");
        assert_eq!(out["id"], "resp_1");
        assert_eq!(out["output"][0]["content"][0]["text"], "hi there", "{out}");
    }
    let (status, out) =
        h.call("POST", "/v1/responses", Some(&key), Some(json!({"model": "grok-build", "input": "hi", "stream": true}))).await;
    assert_eq!(status, 200);
    let text = out.as_str().unwrap();
    assert!(text.contains("response.completed") && text.contains(": keepalive"), "{text}");
}

#[tokio::test]
async fn chat_client_reaches_grok_through_translation() {
    let h = harness().await;
    h.account(Credential {
        provider: SubProvider::Grok,
        access_token: "xai-tok".into(),
        refresh_token: Some("rt".into()),
        expires_at: Some(now() + 3600),
        account_id: Some("sub-1".into()),
        ..Default::default()
    })
    .await;
    let stream = [
        json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "message"}}),
        json!({"type": "keepalive"}),
        json!({"type": "response.output_text.delta", "delta": "from grok"}),
        json!({"type": "response.output_item.done", "item": {"type": "message"}}),
        json!({"type": "response.completed", "response": {"status": "completed", "usage": {"input_tokens": 3, "output_tokens": 2}}}),
    ]
    .iter()
    .map(|e| format!("data: {e}\n\n"))
    .collect::<String>();
    Mock::given(method("POST"))
        .and(path("/grok/v1/responses"))
        .and(header("authorization", "Bearer xai-tok"))
        .and(header("x-xai-token-auth", "xai-grok-cli"))
        .and(body_partial_json(json!({"model": "grok-build", "stream": true})))
        .respond_with(ResponseTemplate::new(200).set_body_raw(stream, "text/event-stream"))
        .mount(&h.upstream)
        .await;
    let key = h.key(policy("p")).await;
    let body = json!({"model": "grok-build", "messages": [{"role": "user", "content": "hi"}]});
    let (status, out) = h.call("POST", "/v1/chat/completions", Some(&key), Some(body)).await;
    assert_eq!(status, 200, "{out}");
    assert_eq!(out["choices"][0]["message"]["content"], "from grok", "{out}");
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
async fn policy_limits_answer_429_in_the_endpoint_shape() {
    let h = harness().await;
    Mock::given(method("POST"))
        .and(path("/llama/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"choices": [], "usage": {"prompt_tokens": 30, "completion_tokens": 12}})),
        )
        .mount(&h.upstream)
        .await;
    let limits = Limits { requests_per_minute: Some(2), tokens_per_day: Some(1_000), ..Default::default() };
    let key = h.key(Policy { limits, ..policy("lim") }).await;
    let chat = || json!({"model": "llama/qwen3", "messages": []});
    assert_eq!(h.call("POST", "/v1/chat/completions", Some(&key), Some(chat())).await.0, 200);

    // Keys list: the key's consumption, its policy's limits, and the consumption they check.
    let (_, keys) = h.call("GET", "/admin/keys", Some(&h.admin), None).await;
    let k = keys.as_array().unwrap().iter().find(|k| k["client_id"] == "lim/client").unwrap();
    assert_eq!(k["usage"], json!({"requests_per_minute": 1, "requests_per_day": 1, "tokens_per_minute": 42, "tokens_per_day": 42}));
    assert_eq!(k["limits"], json!({"requests_per_minute": 2, "tokens_per_day": 1000}));
    assert_eq!(k["limit_usage"]["tokens_per_day"], 42);

    // The second request is the last one this minute; passthroughs count too.
    Mock::given(method("GET")).and(path("/llama/v1/props")).respond_with(ResponseTemplate::new(200)).mount(&h.upstream).await;
    assert_eq!(h.call("GET", "/llama/v1/props", Some(&key), None).await.0, 200);
    let raw = |uri: &str, body: Option<Value>| {
        let mut req = Request::builder()
            .method(if body.is_some() { "POST" } else { "GET" })
            .uri(uri)
            .header("authorization", format!("Bearer {key}"));
        if body.is_some() {
            req = req.header("content-type", "application/json");
        }
        let req = req.body(body.map(|b| Body::from(b.to_string())).unwrap_or_default()).unwrap();
        router(h.state.clone()).oneshot(req)
    };
    let resp = raw("/v1/chat/completions", Some(chat())).await.unwrap();
    assert_eq!(resp.status(), 429);
    let retry: i64 = resp.headers()["retry-after"].to_str().unwrap().parse().unwrap();
    assert!((1..=60).contains(&retry), "{retry}");
    let body: Value = serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(body["error"]["type"], "rate_limit_error");
    assert!(body["error"]["message"].as_str().unwrap().contains("requests_per_minute of 2 for this key"));
    // Messages clients get Anthropic's error shape.
    let (status, body) = h.call("POST", "/v1/messages", Some(&key), Some(json!({"model": "llama/qwen3", "messages": []}))).await;
    assert_eq!(status, 429, "{body}");
    assert_eq!((body["type"].as_str(), body["error"]["type"].as_str()), (Some("error"), Some("rate_limit_error")));
    let resp = raw("/llama/v1/props", None).await.unwrap();
    assert_eq!(resp.status(), 429);
    let resp = raw("/opencode-go/v1/models", None).await.unwrap();
    assert_eq!(resp.status(), 429, "limited before the pool is asked");
    let resp = raw("/llama/v1/messages", Some(json!({"model": "qwen3"}))).await.unwrap();
    assert_eq!(resp.status(), 429);
    let body: Value = serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(body["type"], "error", "Anthropic shape on a Messages passthrough: {body}");
    // Refused requests are not counted, whether by a limit or by the policy.
    assert_eq!(h.call("POST", "/v1/chat/completions", Some(&key), Some(json!({"model": "nope"}))).await.0, 404);
    assert_eq!(h.state.limiter.consumption("lim/client", now()).requests_per_minute, 2);

    // Limits are validated on save.
    let zero = json!({"name": "zero", "providers": ["*"], "limits": {"requests_per_day": 0}});
    let (status, body) = h.call("PUT", "/admin/policies/zero", Some(&h.admin), Some(zero)).await;
    assert_eq!(status, 400, "{body}");
    let unknown = json!({"name": "spend", "providers": ["*"], "limits": {"dollars_per_day": 5}});
    assert_eq!(h.call("PUT", "/admin/policies/spend", Some(&h.admin), Some(unknown)).await.0, 422);
}

#[tokio::test]
async fn chat_streams_report_usage_for_token_limits() {
    let h = harness().await;
    // A conforming upstream sends usage only when asked to.
    let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
               data: {\"choices\":[],\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":2}}\n\ndata: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/llama/v1/chat/completions"))
        .and(body_partial_json(json!({"stream_options": {"include_usage": true}})))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"))
        .mount(&h.upstream)
        .await;
    let limits = Limits { tokens_per_day: Some(13), ..Default::default() };
    let key = h.key(Policy { limits, ..policy("st") }).await;
    let omitted = json!({"model": "llama/qwen3", "messages": [], "stream": true});
    let refused = json!({"model": "llama/qwen3", "messages": [], "stream": true, "stream_options": {"include_usage": false}});
    assert_eq!(h.call("POST", "/v1/chat/completions", Some(&key), Some(omitted.clone())).await.0, 200);
    assert_eq!(h.call("POST", "/v1/chat/completions", Some(&key), Some(refused.clone())).await.0, 200);
    // The passthrough asks too.
    assert_eq!(h.call("POST", "/llama/v1/chat/completions", Some(&key), Some(refused)).await.0, 200);
    assert_eq!(h.state.limiter.consumption("st/client", now()).tokens_per_day, 18);
    assert_eq!(h.call("POST", "/v1/chat/completions", Some(&key), Some(omitted)).await.0, 429);
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

    // The preview lists denied models too, with the name each one is served under.
    let draft = json!({
        "name": "dev", "providers": ["claude", "opencode-go"], "deny_models": ["opencode-go/kimi-k3"],
        "renames": {"claude/claude-opus-5": "opus"},
    });
    let (status, view) = h.call("POST", "/admin/policy-preview", Some(&h.admin), Some(draft)).await;
    assert_eq!(status, 200, "{view}");
    let row = |provider: &str, model: &str| {
        view["catalog"].as_array().unwrap().iter().find(|c| c["provider"] == provider && c["model"] == model).cloned().unwrap()
    };
    assert_eq!(
        row("opencode-go", "kimi-k3"),
        json!({"provider": "opencode-go", "model": "kimi-k3", "allowed": false, "rule": "opencode-go/kimi-k3", "name": null})
    );
    assert_eq!(row("claude", "claude-opus-5")["name"], "opus");
    let names: Vec<&str> = view["models"].as_array().unwrap().iter().map(|m| m["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["claude-haiku-5-5", "opus"]);

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

    // Usage narrowed by filters; values may be URL-encoded and an empty one selects NULL.
    for (client, model) in [(Some("usage a"), "m1"), (Some("usage a"), "m2"), (Some("usage b"), "m1"), (None, "m3")] {
        let record = crate::db::RequestRecord {
            client_id: client.map(Into::into),
            provider: "usage-test".into(),
            model: Some(model.into()),
            endpoint: "/v1/responses".into(),
            status: 200,
            ..Default::default()
        };
        h.state.db.log_request(&record).await.unwrap();
    }
    let (status, rows) =
        h.call("GET", "/admin/usage?group_by=model&since_secs=3600&provider=usage-test&client=usage%20a", Some(&h.admin), None).await;
    assert_eq!(status, 200, "{rows}");
    let mut models: Vec<_> = rows.as_array().unwrap().iter().map(|r| r["key"].as_str().unwrap()).collect();
    models.sort();
    assert_eq!(models, ["m1", "m2"]);
    let (_, rows) = h.call("GET", "/admin/usage?group_by=model&provider=usage-test&client=", Some(&h.admin), None).await;
    assert_eq!(rows, json!([{"key": "m3", "requests": 1, "errors": 0, "input_tokens": 0, "output_tokens": 0, "cached_tokens": 0}]));
}

/// A mock OpenID provider at `<upstream>/oidc` for the web UI sign-in.
struct Idp {
    issuer: String,
    key: openidconnect::core::CoreRsaPrivateSigningKey,
}

impl Idp {
    async fn mount(h: &mut Harness) -> Idp {
        use openidconnect::PrivateSigningKey;
        let issuer = format!("{}/oidc", h.upstream.uri());
        let key = openidconnect::core::CoreRsaPrivateSigningKey::from_pem(
            include_str!("testdata/oidc-test-key.pem"),
            Some(openidconnect::JsonWebKeyId::new("k1".into())),
        )
        .unwrap();
        Mock::given(method("GET"))
            .and(path("/oidc/.well-known/openid-configuration"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issuer": issuer,
                "authorization_endpoint": format!("{issuer}/authorize"),
                "token_endpoint": format!("{issuer}/token"),
                "jwks_uri": format!("{issuer}/jwks"),
                "response_types_supported": ["code"],
                "subject_types_supported": ["public"],
                "id_token_signing_alg_values_supported": ["RS256"],
            })))
            .mount(&h.upstream)
            .await;
        Mock::given(method("GET"))
            .and(path("/oidc/jwks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"keys": [key.as_verification_key()]})))
            .mount(&h.upstream)
            .await;
        h.state.secrets.put("oidc", json!({"client_secret": "shh"}).as_object().unwrap().clone()).await.unwrap();
        let mut cfg = (*h.state.cfg).clone();
        cfg.ui.oidc = Some(crate::config::OidcConfig {
            issuer: issuer.clone(),
            client_id: "xolotl".into(),
            redirect_url: format!("https://gw.example.test{}", session::CALLBACK_PATH),
            admin_group: "admins".into(),
        });
        h.state.cfg = Arc::new(cfg);
        Idp { issuer, key }
    }

    fn id_token(&self, nonce: &str, groups: &[&str]) -> String {
        use openidconnect::core::{CoreGenderClaim, CoreJweContentEncryptionAlgorithm, CoreJwsSigningAlgorithm};
        use openidconnect::{Audience, EndUserUsername, IdToken, IdTokenClaims, IssuerUrl, Nonce, StandardClaims, SubjectIdentifier};
        let now = chrono::Utc::now();
        let claims = IdTokenClaims::new(
            IssuerUrl::new(self.issuer.clone()).unwrap(),
            vec![Audience::new("xolotl".into())],
            now + chrono::Duration::minutes(5),
            now,
            StandardClaims::new(SubjectIdentifier::new("u-1".into())).set_preferred_username(Some(EndUserUsername::new("operator".into()))),
            session::Groups { groups: groups.iter().map(|g| g.to_string()).collect() },
        )
        .set_nonce(Some(Nonce::new(nonce.into())));
        IdToken::<_, CoreGenderClaim, CoreJweContentEncryptionAlgorithm, _>::new(
            claims,
            &self.key,
            CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256,
            None,
            None,
        )
        .unwrap()
        .to_string()
    }

    /// Start a sign-in; the provider then answers one code exchange with an
    /// ID token for `groups` (and the nonce from `nonce`, given the real one).
    /// Returns the callback query's `state` and the pending-sign-in cookie.
    async fn start(&self, h: &Harness, groups: &[&str], nonce: impl Fn(&str) -> String) -> (String, String) {
        self.start_with(h, |real| self.id_token(&nonce(real), groups)).await
    }

    /// Start a sign-in whose code exchange returns `id_token(nonce)`.
    async fn start_with(&self, h: &Harness, id_token: impl FnOnce(&str) -> String) -> (String, String) {
        let resp = h.send("GET", "/admin/session/login", &[]).await;
        assert_eq!(resp.status(), 303);
        let location = url::Url::parse(resp.headers()["location"].to_str().unwrap()).unwrap();
        assert!(location.as_str().starts_with(&format!("{}/authorize", self.issuer)), "{location}");
        let q: BTreeMap<String, String> = location.query_pairs().into_owned().collect();
        assert_eq!(q["client_id"], "xolotl");
        assert_eq!(q["code_challenge_method"], "S256");
        assert!(q["scope"].split(' ').any(|s| s == "groups"), "{}", q["scope"]);
        let pending = resp.headers()["set-cookie"].to_str().unwrap();
        for attr in ["SameSite=Lax", "Secure", "HttpOnly", "Path=/admin/session/callback"] {
            assert!(pending.contains(attr), "{attr} missing: {pending}");
        }
        Mock::given(method("POST"))
            .and(path("/oidc/token"))
            .and(header("authorization", "Basic eG9sb3RsOnNoaA=="))
            .and(wiremock::matchers::body_string_contains("code_verifier="))
            // Each sign-in's code is its state (see `callback`), so the exchange gets its own token.
            .and(wiremock::matchers::body_string_contains(format!("code={}", q["state"])))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "at", "token_type": "Bearer", "expires_in": 300,
                "id_token": id_token(&q["nonce"]),
            })))
            .up_to_n_times(1)
            .mount(&h.upstream)
            .await;
        (q["state"].clone(), pending.split(';').next().unwrap().to_string())
    }
}

impl Harness {
    async fn send(&self, method: &str, uri: &str, headers: &[(&str, &str)]) -> axum::response::Response {
        let mut req = Request::builder().method(method).uri(uri);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        router(self.state.clone()).oneshot(req.body(Body::empty()).unwrap()).await.unwrap()
    }

    /// The callback: where it sends the browser, and the session cookie it
    /// sets. It ends the pending sign-in (clears its cookie) when the cookie
    /// names this `state`, and only then.
    async fn callback(&self, state: &str, cookie: &str) -> (String, Option<String>) {
        let resp = self.send("GET", &format!("/admin/session/callback?code={state}&state={state}"), &[("cookie", cookie)]).await;
        assert_eq!(resp.status(), 303);
        let cookies: Vec<String> = resp.headers().get_all("set-cookie").iter().map(|v| v.to_str().unwrap().to_string()).collect();
        let cleared = cookies.iter().any(|c| c.starts_with("xolotl_oidc=;") && c.contains("Max-Age=0"));
        assert_eq!(cleared, cookie == format!("xolotl_oidc={state}"), "pending cookie cleared only for its own sign-in: {cookies:?}");
        let session = cookies.into_iter().find(|c| c.starts_with("xolotl_session="));
        (resp.headers()["location"].to_str().unwrap().to_string(), session)
    }
}

#[tokio::test]
async fn ui_sign_in_with_oidc_gives_an_admin_session() {
    let mut h = harness().await;
    let (_, s) = h.call_with("GET", "/admin/session", &[], None).await;
    assert_eq!(s, json!({"authenticated": false, "configured": false}));
    assert_eq!(h.send("GET", "/admin/session/login", &[]).await.status(), 404);

    let idp = Idp::mount(&mut h).await;
    let (_, s) = h.call_with("GET", "/admin/session", &[], None).await;
    assert_eq!(s, json!({"authenticated": false, "configured": true}));

    let (state, pending) = idp.start(&h, &["admins"], str::to_string).await;
    let (to, session) = h.callback(&state, &pending).await;
    assert_eq!(to, "/");
    let session = session.expect("a session cookie");
    assert!(session.contains("HttpOnly") && session.contains("SameSite=Strict") && session.contains("Secure"), "{session}");
    let cookie = session.split(';').next().unwrap().to_string();

    let (_, s) = h.call_with("GET", "/admin/session", &[("cookie", &cookie)], None).await;
    assert_eq!(s["user"], "operator");
    let csrf = s["csrf_token"].as_str().unwrap().to_string();

    // The cookie alone is not enough; with the CSRF token it is admin, and only admin.
    assert_eq!(h.call_with("GET", "/admin/keys", &[("cookie", &cookie)], None).await.0, 401);
    assert_eq!(h.call_with("GET", "/admin/keys", &[("cookie", &cookie), ("x-csrf-token", "nope")], None).await.0, 401);
    let signed_in = [("cookie", cookie.as_str()), ("x-csrf-token", csrf.as_str())];
    assert_eq!(h.call_with("GET", "/admin/keys", &signed_in, None).await.0, 200);
    assert_eq!(h.call_with("GET", "/v1/models", &signed_in, None).await.0, 401);

    // A callback completes once.
    assert_eq!(h.callback(&state, &pending).await, ("/?signin=expired".into(), None));

    assert_eq!(h.call_with("DELETE", "/admin/session", &signed_in, None).await.0, 200);
    assert_eq!(h.call_with("GET", "/admin/keys", &signed_in, None).await.0, 401);
}

#[tokio::test]
async fn ui_sign_in_with_oidc_refusals() {
    let mut h = harness().await;
    let idp = Idp::mount(&mut h).await;

    // Outside the admin group, including groups whose names merely start with it.
    let (state, pending) = idp.start(&h, &["users", "admins-readonly"], str::to_string).await;
    assert_eq!(h.callback(&state, &pending).await, ("/?signin=forbidden".into(), None));

    // An ID token for another sign-in.
    let (state, pending) = idp.start(&h, &["admins"], |_| "someone-elses".into()).await;
    assert_eq!(h.callback(&state, &pending).await, ("/?signin=failed".into(), None));

    // Signed with the client secret instead of the provider's key.
    let (state, pending) = idp
        .start_with(&h, |nonce| {
            use openidconnect::core::{CoreGenderClaim, CoreHmacKey, CoreJweContentEncryptionAlgorithm, CoreJwsSigningAlgorithm};
            use openidconnect::{Audience, IdToken, IdTokenClaims, IssuerUrl, Nonce, StandardClaims, SubjectIdentifier};
            let now = chrono::Utc::now();
            let claims = IdTokenClaims::new(
                IssuerUrl::new(idp.issuer.clone()).unwrap(),
                vec![Audience::new("xolotl".into())],
                now + chrono::Duration::minutes(5),
                now,
                StandardClaims::new(SubjectIdentifier::new("u-1".into())),
                session::Groups { groups: vec!["admins".into()] },
            )
            .set_nonce(Some(Nonce::new(nonce.into())));
            IdToken::<_, CoreGenderClaim, CoreJweContentEncryptionAlgorithm, _>::new(
                claims,
                &CoreHmacKey::new("shh"),
                CoreJwsSigningAlgorithm::HmacSha256,
                None,
                None,
            )
            .unwrap()
            .to_string()
        })
        .await;
    assert_eq!(h.callback(&state, &pending).await, ("/?signin=failed".into(), None));

    // Finished in a browser that didn't start it.
    let (state, _) = idp.start(&h, &["admins"], str::to_string).await;
    assert_eq!(h.callback(&state, "xolotl_oidc=other").await, ("/?signin=expired".into(), None));
    assert_eq!(h.callback(&state, "").await, ("/?signin=expired".into(), None));

    // Two sign-ins in one browser: the second's cookie replaces the first's.
    // The first's callback fails without disturbing the second, which completes.
    let (first, _) = idp.start(&h, &["admins"], str::to_string).await;
    let (second, pending) = idp.start(&h, &["admins"], str::to_string).await;
    assert_eq!(h.callback(&first, &pending).await, ("/?signin=expired".into(), None));
    let (to, session) = h.callback(&second, &pending).await;
    assert_eq!(to, "/");
    assert!(session.is_some());

    // The provider refused.
    let (state, pending) = idp.start(&h, &["admins"], str::to_string).await;
    let resp = h.send("GET", &format!("/admin/session/callback?error=access_denied&state={state}"), &[("cookie", &pending)]).await;
    assert_eq!(resp.headers()["location"], "/?signin=denied");
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
    // The icons browsers look for at the root come from the UI build too.
    let resp = router(h.state.clone()).oneshot(Request::builder().uri("/favicon.svg").body(Body::empty()).unwrap()).await.unwrap();
    if resp.status() == 200 {
        assert_eq!(resp.headers()["content-type"], "image/svg+xml");
        assert_eq!(resp.headers()["cache-control"], "public, max-age=86400");
    }
    let resp = router(h.state.clone()).oneshot(Request::builder().uri("/site.webmanifest").body(Body::empty()).unwrap()).await.unwrap();
    if resp.status() == 200 {
        assert_eq!(resp.headers()["content-type"], "application/manifest+json");
    }
}

#[tokio::test]
async fn ui_sign_in_does_not_follow_provider_redirects_or_read_huge_responses() {
    let mut h = harness().await;
    let idp = Idp::mount(&mut h).await;
    let start = |h: &Harness| {
        let h = h.state.clone();
        async move {
            let resp = router(h).oneshot(Request::builder().uri("/admin/session/login").body(Body::empty()).unwrap()).await.unwrap();
            resp.headers()["location"].to_str().unwrap().to_string()
        }
    };
    // A working provider first, so the failures below are the redirect and the size.
    assert!(start(&h).await.starts_with(&format!("{}/authorize", idp.issuer)));

    let elsewhere = MockServer::start().await;
    h.upstream.reset().await;
    Mock::given(method("GET"))
        .and(path("/oidc/.well-known/openid-configuration"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", format!("{}/discovery", elsewhere.uri())))
        .mount(&h.upstream)
        .await;
    Mock::given(method("GET"))
        .and(path("/discovery"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "issuer": idp.issuer, "authorization_endpoint": format!("{}/authorize", idp.issuer),
            "jwks_uri": format!("{}/jwks", idp.issuer), "response_types_supported": ["code"],
            "subject_types_supported": ["public"], "id_token_signing_alg_values_supported": ["RS256"],
        })))
        .mount(&elsewhere)
        .await;
    Mock::given(method("GET"))
        .and(path("/oidc/jwks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"keys": []})))
        .mount(&h.upstream)
        .await;
    assert_eq!(start(&h).await, "/?signin=unavailable", "a redirect is not followed");

    h.upstream.reset().await;
    let mut huge = serde_json::to_string(&json!({
        "issuer": idp.issuer, "authorization_endpoint": format!("{}/authorize", idp.issuer),
        "jwks_uri": format!("{}/jwks", idp.issuer), "response_types_supported": ["code"],
        "subject_types_supported": ["public"], "id_token_signing_alg_values_supported": ["RS256"],
    }))
    .unwrap();
    huge.push_str(&" ".repeat(2 * 1024 * 1024));
    Mock::given(method("GET"))
        .and(path("/oidc/.well-known/openid-configuration"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(huge, "application/json"))
        .mount(&h.upstream)
        .await;
    Mock::given(method("GET"))
        .and(path("/oidc/jwks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"keys": []})))
        .mount(&h.upstream)
        .await;
    assert_eq!(start(&h).await, "/?signin=unavailable", "an oversized response is refused");
}

// ------------------------------------------------------------------ MCP

mod mcp_gateway {
    use std::net::SocketAddr;

    use rmcp::model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ClientCapabilities, ClientConfig, ContentBlock, Implementation,
        ListToolsResult, PaginatedRequestParams, ProtocolVersion, ServerCapabilities, ServerConfig, Tool,
    };
    use rmcp::service::{RequestContext, RoleServer};
    use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
    use rmcp::transport::{StreamableHttpClientTransport, StreamableHttpServerConfig, StreamableHttpService};
    use rmcp::{ClientLifecycleMode, ClientServiceExt, ErrorData, ServerHandler};

    use super::*;
    use crate::config::McpServerConfig;

    /// An upstream MCP server with a few tools; `whoami` echoes the
    /// Authorization header it received.
    #[derive(Clone)]
    struct Upstream;

    fn schema(v: Value) -> Arc<serde_json::Map<String, Value>> {
        Arc::new(v.as_object().unwrap().clone())
    }

    impl ServerHandler for Upstream {
        fn get_info(&self) -> ServerConfig {
            ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
        }

        async fn list_tools(&self, _: Option<PaginatedRequestParams>, _: RequestContext<RoleServer>) -> Result<ListToolsResult, ErrorData> {
            let num = json!({"type": "object", "properties": {"a": {"type": "number"}, "b": {"type": "number"}}, "required": ["a", "b"]});
            Ok(ListToolsResult::with_all_items(vec![
                Tool::new("add", "Add two numbers", schema(num.clone()))
                    .with_raw_output_schema(schema(json!({"type": "object", "properties": {"sum": {"type": "number"}}}))),
                Tool::new("whoami", "Who the server thinks you are", schema(json!({"type": "object"}))),
                Tool::new("delete_everything", "Do not call", schema(json!({"type": "object"}))),
                Tool::new("fail", "Always fails", schema(json!({"type": "object"}))),
            ]))
        }

        async fn call_tool(&self, req: CallToolRequestParams, ctx: RequestContext<RoleServer>) -> Result<CallToolResponse, ErrorData> {
            let args = req.arguments.unwrap_or_default();
            if let Some(ms) = args.get("delay_ms").and_then(Value::as_u64) {
                tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
            }
            let result = match req.name.as_ref() {
                "add" => {
                    let n = |k: &str| args.get(k).and_then(Value::as_f64).unwrap_or(0.0);
                    CallToolResult::structured(json!({"sum": n("a") + n("b")}))
                }
                "whoami" => {
                    let parts = ctx.extensions.get::<axum::http::request::Parts>().unwrap();
                    let auth = parts.headers.get("authorization").map(|v| v.to_str().unwrap().to_string()).unwrap_or_default();
                    CallToolResult::success(vec![ContentBlock::text(auth)])
                }
                "fail" => CallToolResult::error(vec![ContentBlock::text("upstream said no")]),
                _ => CallToolResult::success(vec![ContentBlock::text("deleted")]),
            };
            Ok(CallToolResponse::Complete(result))
        }
    }

    async fn serve(router: axum::Router) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        addr
    }

    async fn upstream() -> SocketAddr {
        let svc: StreamableHttpService<Upstream, rmcp::transport::streamable_http_server::session::local::LocalSessionManager> =
            StreamableHttpService::new(|| Ok(Upstream), Default::default(), StreamableHttpServerConfig::default());
        serve(axum::Router::new().nest_service("/mcp", svc)).await
    }

    struct Gateway {
        h: Harness,
        addr: SocketAddr,
    }

    async fn gateway() -> Gateway {
        let up = upstream().await;
        let h = harness_with(|cfg| {
            for name in ["calc", "calc-two"] {
                cfg.mcp.servers.insert(
                    name.into(),
                    McpServerConfig {
                        url: format!("http://{up}/mcp"),
                        description: format!("Calculator {name}"),
                        headers: BTreeMap::from([("Authorization".into(), "Bearer {token}".into())]),
                        ..Default::default()
                    },
                );
            }
        })
        .await;
        h.state.mcp.set_secret("calc", json!({"token": "one"}).as_object().unwrap().clone()).await.unwrap();
        h.state.mcp.set_secret("calc-two", json!({"token": "two"}).as_object().unwrap().clone()).await.unwrap();
        let addr = serve(router(h.state.clone())).await;
        Gateway { h, addr }
    }

    type Client = rmcp::service::RunningService<rmcp::service::RoleClient, ClientConfig>;

    async fn client(addr: SocketAddr, key: &str, lifecycle: ClientLifecycleMode) -> Client {
        let cfg = StreamableHttpClientTransportConfig::with_uri(format!("http://{addr}/mcp")).auth_header(key);
        let transport = StreamableHttpClientTransport::from_config(cfg);
        ClientConfig::new(ClientCapabilities::default(), Implementation::new("test", "1"))
            .serve_with_lifecycle(transport, lifecycle)
            .await
            .unwrap()
    }

    fn modern() -> ClientLifecycleMode {
        ClientLifecycleMode::Discover { preferred_versions: vec![ProtocolVersion::V_2026_07_28] }
    }

    async fn call(c: &Client, tool: &str, args: Value) -> (bool, String) {
        let mut p = CallToolRequestParams::new(tool.to_string());
        p.arguments = args.as_object().cloned();
        let r = c.call_tool(p).await.unwrap();
        let text = r.content.iter().filter_map(|c| c.as_text().map(|t| t.text.clone())).collect::<Vec<_>>().join("\n");
        (r.is_error == Some(true), text)
    }

    fn mcp_policy(servers: &[&str], deny_tools: &[&str]) -> Policy {
        Policy {
            name: "agents".into(),
            scopes: vec![Scope::Mcp],
            mcp_servers: servers.iter().map(|s| s.to_string()).collect(),
            deny_tools: deny_tools.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn search_and_execute_through_the_policy() {
        let g = gateway().await;
        let key = g.h.key(mcp_policy(&["calc", "calc-two"], &["delete_*"])).await;
        let c = client(g.addr, &key, modern()).await;

        let tools = c.list_all_tools().await.unwrap();
        assert_eq!(tools.iter().map(|t| t.name.as_ref()).collect::<Vec<_>>(), vec!["search", "execute"]);
        assert!(tools[1].description.as_deref().unwrap().contains("`calc_two`: Calculator calc-two"));

        let (err, ts) = call(&c, "search", json!({"query": "add"})).await;
        assert!(!err);
        assert!(ts.contains("declare const calc: {") && ts.contains("declare const calc_two: {"), "{ts}");
        assert!(ts.contains("add(args: {") && ts.contains("Promise<{\n    sum?: number;\n  }>"), "{ts}");
        let (_, all) = call(&c, "search", json!({"query": ""})).await;
        assert!(all.contains("whoami") && !all.contains("delete_everything"), "denied tools are hidden: {all}");

        // Two instances of one server, each with its own credentials.
        let code = "const s = await calc.add({a: 2, b: 3}); \
                    return {sum: s.sum, one: await calc.whoami(), two: await calc_two.whoami()};";
        let (err, out) = call(&c, "execute", json!({"code": code})).await;
        assert!(!err, "{out}");
        let out: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(out["result"], json!({"sum": 5, "one": "Bearer one", "two": "Bearer two"}));
        assert_eq!(out["calls"].as_array().unwrap().len(), 3);

        // Denied tools and tool errors throw inside the code.
        let (_, out) = call(&c, "execute", json!({"code": "try { await calc.delete_everything() } catch (e) { return e.message }"})).await;
        assert!(out.contains("not a tool available to this key"), "{out}");
        let (err, out) = call(&c, "execute", json!({"code": "await calc.fail()"})).await;
        assert!(err && out.contains("upstream said no"), "{out}");

        // Upstream calls are logged; denied ones never left the gateway.
        let (_, calls) = g.h.call("GET", "/admin/mcp/calls", Some(&g.h.admin), None).await;
        let calls = calls.as_array().unwrap();
        assert_eq!(calls.len(), 4, "{calls:?}");
        assert_eq!(calls[0]["tool"], "fail");
        assert_eq!(calls[0]["status"], "tool_error");
        let (_, usage) = g.h.call("GET", "/admin/mcp/usage?group_by=server", Some(&g.h.admin), None).await;
        assert_eq!(usage[0]["key"], "calc");
        assert_eq!(usage[0]["calls"], 3);
        assert_eq!(usage[0]["errors"], 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn legacy_clients_auth_and_admin() {
        let g = gateway().await;
        let key = g.h.key(mcp_policy(&["calc"], &[])).await;

        // A pre-2026-07-28 client (initialize handshake) works too.
        let c = client(g.addr, &key, ClientLifecycleMode::Initialize).await;
        let (err, out) = call(&c, "execute", json!({"code": "return (await calc.add({a: 1, b: 1})).sum"})).await;
        assert!(!err, "{out}");
        assert_eq!(serde_json::from_str::<Value>(&out).unwrap()["result"], json!(2));
        let (_, out) = call(&c, "execute", json!({"code": "return typeof calc_two"})).await;
        assert!(out.contains("\"undefined\""), "servers outside the policy don't exist: {out}");

        // No key, a key without the mcp scope, and browser origins are refused.
        let http = reqwest::Client::new();
        let url = format!("http://{}/mcp", g.addr);
        let r = http.post(&url).json(&json!({})).send().await.unwrap();
        assert_eq!(r.status(), 401);
        assert_eq!(r.headers()["www-authenticate"], "Bearer");
        let proxy_key = g.h.key(Policy { name: "llm".into(), providers: vec!["*".into()], ..Default::default() }).await;
        assert_eq!(http.post(&url).bearer_auth(&proxy_key).json(&json!({})).send().await.unwrap().status(), 403);
        let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "x", "version": "1"}}});
        let r = http
            .post(&url)
            .bearer_auth(&key)
            .header("origin", "https://evil.example")
            .header("accept", "application/json, text/event-stream")
            .json(&init)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 403);

        // Admin: servers with secret state, tool lists, secrets, policy view.
        let (_, servers) = g.h.call("GET", "/admin/mcp/servers", Some(&g.h.admin), None).await;
        assert_eq!(servers[0]["name"], "calc");
        assert_eq!(servers[0]["namespace"], "calc");
        assert_eq!(servers[1]["namespace"], "calc_two");
        assert_eq!(servers[0]["secret_fields"], json!({"token": true}));
        assert_eq!(servers[0]["tools"], 4);
        let (status, tools) = g.h.call("POST", "/admin/mcp/servers/calc-two/refresh", Some(&g.h.admin), None).await;
        assert_eq!((status, tools["tools"].clone()), (200, json!(4)));
        let (_, tools) = g.h.call("GET", "/admin/mcp/servers/calc/tools", Some(&g.h.admin), None).await;
        assert_eq!(tools[0]["name"], "add");
        let body = json!({"fields": {"token": null}});
        assert_eq!(g.h.call("PUT", "/admin/mcp/servers/calc/secret", Some(&g.h.admin), Some(body)).await.0, 200);
        let (_, servers) = g.h.call("GET", "/admin/mcp/servers", Some(&g.h.admin), None).await;
        assert_eq!(servers[0]["secret_fields"], json!({"token": false}));
        let (_, out) = call(&c, "execute", json!({"code": "await calc.whoami()"})).await;
        assert!(out.contains("secret field \\\"token\\\" is not set"), "{out}");

        let draft = json!({"name": "x", "scopes": ["mcp"], "mcp_servers": ["calc"], "deny_tools": ["fail", "delete_*"]});
        let (status, view) = g.h.call("POST", "/admin/policy-preview", Some(&g.h.admin), Some(draft)).await;
        assert_eq!(status, 200, "{view}");
        assert_eq!(view["mcp_servers"], json!(["calc"]));
        assert_eq!(view["tools"], json!(["calc/add", "calc/whoami"]));
        let bad = json!({"name": "x", "scopes": ["mcp"], "mcp_servers": ["nope"]});
        assert_eq!(g.h.call("POST", "/admin/policy-preview", Some(&g.h.admin), Some(bad)).await.0, 400);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_call_cut_off_by_the_deadline_is_still_logged() {
        let up = upstream().await;
        let h = harness_with(|cfg| {
            cfg.mcp.execute_timeout_secs = 1;
            cfg.mcp.servers.insert("calc".into(), McpServerConfig { url: format!("http://{up}/mcp"), ..Default::default() });
        })
        .await;
        let addr = serve(router(h.state.clone())).await;
        let key = h.key(mcp_policy(&["calc"], &[])).await;
        let c = client(addr, &key, modern()).await;
        // The upstream accepts the call and takes 3 s; the execution ends after 1 s.
        let (err, out) = call(&c, "execute", json!({"code": "return await calc.whoami({delay_ms: 3000})"})).await;
        assert!(err && out.contains("timed out"), "{out}");
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let calls = h.state.db.recent_mcp_calls(10).await.unwrap();
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert_eq!((calls[0].tool.as_str(), calls[0].status.as_str()), ("whoami", "cancelled"));
        assert!(calls[0].error.as_deref().unwrap().contains("outcome is unknown"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_call_cancelled_while_its_row_waits_is_still_logged() {
        let up = upstream().await;
        let h = harness_with(|cfg| {
            cfg.mcp.servers.insert("calc".into(), McpServerConfig { url: format!("http://{up}/mcp"), ..Default::default() });
        })
        .await;
        h.state.mcp.refresh("calc").await.unwrap();
        // The database is busy: the row write waits after the upstream answered.
        let held = h.state.db.hold().await;
        let hub = h.state.mcp.clone();
        let p = crate::keys::Principal { client_id: None, policy: Arc::new(mcp_policy(&["calc"], &[])) };
        let call = tokio::spawn(async move { hub.call(&p, "calc", "whoami", Default::default()).await });
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        call.abort();
        let _ = call.await;
        drop(held);
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let calls = h.state.db.recent_mcp_calls(10).await.unwrap();
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert_eq!((calls[0].tool.as_str(), calls[0].status.as_str()), ("whoami", "ok"));
    }

    /// An upstream whose tools are named like JavaScript hooks.
    #[derive(Clone)]
    struct Hooks;

    impl ServerHandler for Hooks {
        fn get_info(&self) -> ServerConfig {
            ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
        }

        async fn list_tools(&self, _: Option<PaginatedRequestParams>, _: RequestContext<RoleServer>) -> Result<ListToolsResult, ErrorData> {
            Ok(ListToolsResult::with_all_items(
                ["then", "toJSON"].map(|n| Tool::new(n, format!("The {n} tool"), schema(json!({"type": "object"})))).to_vec(),
            ))
        }

        async fn call_tool(&self, req: CallToolRequestParams, _: RequestContext<RoleServer>) -> Result<CallToolResponse, ErrorData> {
            Ok(CallToolResponse::Complete(CallToolResult::success(vec![ContentBlock::text(format!("called {}", req.name))])))
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tools_named_then_and_tojson_can_be_called() {
        let svc: StreamableHttpService<Hooks, rmcp::transport::streamable_http_server::session::local::LocalSessionManager> =
            StreamableHttpService::new(|| Ok(Hooks), Default::default(), StreamableHttpServerConfig::default());
        let up = serve(axum::Router::new().nest_service("/mcp", svc)).await;
        let h = harness_with(|cfg| {
            cfg.mcp.servers.insert("hooks".into(), McpServerConfig { url: format!("http://{up}/mcp"), ..Default::default() });
        })
        .await;
        let addr = serve(router(h.state.clone())).await;
        let key = h.key(mcp_policy(&["hooks"], &[])).await;
        let c = client(addr, &key, modern()).await;

        let (_, ts) = call(&c, "search", json!({"query": ""})).await;
        assert!(ts.contains(r#"codemode.call("hooks", "then", args)"#) && ts.contains(r#"codemode.call("hooks", "toJSON", args)"#), "{ts}");
        let code = "return [await codemode.call('hooks', 'then', {}), await codemode.call('hooks', 'toJSON', {}), typeof hooks.then]";
        let (err, out) = call(&c, "execute", json!({"code": code})).await;
        assert!(!err, "{out}");
        assert_eq!(serde_json::from_str::<Value>(&out).unwrap()["result"], json!(["called then", "called toJSON", "undefined"]));
        let calls = h.state.db.recent_mcp_calls(10).await.unwrap();
        assert_eq!(calls.len(), 2, "both went through the gateway's call path");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn servers_added_at_runtime() {
        let g = gateway().await;
        let h = &g.h;
        let url = h.state.cfg.mcp.servers["calc"].url.clone();
        let live = json!({"name": "live", "url": url, "description": "Added from the UI", "headers": {"Authorization": "Bearer {token}"}});

        // A policy can't name a server before it exists.
        let p = json!({"name": "agents", "scopes": ["mcp"], "mcp_servers": ["live"]});
        assert_eq!(h.call("PUT", "/admin/policies/agents", Some(&h.admin), Some(p.clone())).await.0, 400);

        assert_eq!(h.call("POST", "/admin/mcp/servers", Some(&h.admin), Some(live.clone())).await.0, 200);
        let (_, servers) = h.call("GET", "/admin/mcp/servers", Some(&h.admin), None).await;
        let find = |servers: &Value, n: &str| servers.as_array().unwrap().iter().find(|s| s["name"] == n).cloned();
        let s = find(&servers, "live").unwrap();
        assert_eq!((s["source"].clone(), s["definition"]["description"].clone()), (json!("vault"), json!("Added from the UI")));
        assert_eq!(s["secret_fields"], json!({"token": false}));
        let calc = find(&servers, "calc").unwrap();
        assert_eq!(calc["source"], "config");
        assert!(calc.get("definition").is_none());

        // Refused: duplicates, config-file names, names that collide in code, bad definitions.
        for (body, status) in [
            (live.clone(), 409),
            (json!({"name": "calc", "url": url}), 409),
            (json!({"name": "calc_two", "url": url}), 409),
            (json!({"name": "bad", "url": "ftp://x"}), 400),
            (json!({"name": "Bad", "url": url}), 400),
            (json!({"name": "bad", "url": url, "colour": "red"}), 400),
            (json!({"name": "bad", "url": url, "oauth": {"client_id": "x"}}), 400),
        ] {
            let (got, err) = h.call("POST", "/admin/mcp/servers", Some(&h.admin), Some(body.clone())).await;
            assert_eq!(got, status, "{body}: {err}");
        }

        // It works like any other server.
        h.state.mcp.set_secret("live", json!({"token": "tok"}).as_object().unwrap().clone()).await.unwrap();
        let key = h.key(serde_json::from_value(p).unwrap()).await;
        let c = client(g.addr, &key, modern()).await;
        let (err, out) = call(&c, "execute", json!({"code": "return await live.whoami()"})).await;
        assert!(!err && out.contains("Bearer tok"), "{out}");

        // Changes apply at once; the config file's servers are read-only.
        let changed = json!({"url": url, "description": "Renamed", "headers": {"Authorization": "Bearer {token}"}});
        assert_eq!(h.call("PUT", "/admin/mcp/servers/live", Some(&h.admin), Some(changed)).await.0, 200);
        let (_, servers) = h.call("GET", "/admin/mcp/servers", Some(&h.admin), None).await;
        assert_eq!(find(&servers, "live").unwrap()["description"], "Renamed");
        assert_eq!(h.call("PUT", "/admin/mcp/servers/calc", Some(&h.admin), Some(json!({"url": url}))).await.0, 409);
        assert_eq!(h.call("PUT", "/admin/mcp/servers/nope", Some(&h.admin), Some(json!({"url": url}))).await.0, 404);
        let renamed = json!({"name": "other", "url": url});
        assert_eq!(h.call("PUT", "/admin/mcp/servers/live", Some(&h.admin), Some(renamed)).await.0, 400);

        // Deleting is refused while a policy lists the server.
        let (status, err) = h.call("DELETE", "/admin/mcp/servers/live", Some(&h.admin), None).await;
        assert_eq!(status, 409);
        assert!(err["error"]["message"].as_str().unwrap().contains("used by policy agents"), "{err}");
        assert_eq!(h.call("DELETE", "/admin/mcp/servers/calc", Some(&h.admin), None).await.0, 409);
        let p = json!({"name": "agents", "scopes": ["mcp"], "mcp_servers": ["calc"]});
        assert_eq!(h.call("PUT", "/admin/policies/agents", Some(&h.admin), Some(p)).await.0, 200);
        assert_eq!(h.call("DELETE", "/admin/mcp/servers/live", Some(&h.admin), None).await.0, 200);
        assert!(!h.state.mcp.has_server("live"));
        assert!(h.state.secrets.get("mcp/live").await.unwrap().is_none(), "its secrets go too");
        assert_eq!(h.call("DELETE", "/admin/mcp/servers/live", Some(&h.admin), None).await.0, 404);

        // Servers written by another instance appear on reload; invalid ones are skipped.
        let ext = json!({"url": url}).as_object().unwrap().clone();
        h.state.secrets.put("mcp-servers/ext", ext).await.unwrap();
        h.state.secrets.put("mcp-servers/broken", json!({"url": "nope"}).as_object().unwrap().clone()).await.unwrap();
        assert_eq!(h.state.mcp.reload().await.unwrap(), 1);
        assert!(h.state.mcp.has_server("ext") && !h.state.mcp.has_server("broken"));
    }

    mod oauth {
        use std::collections::{HashMap, HashSet};
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        use axum::extract::{Form, State as AxState};
        use base64::Engine;
        use parking_lot::Mutex;
        use sha2::Digest;

        use super::*;
        use crate::config::{McpAuth, McpServerOAuthConfig};
        use crate::keys::Principal;
        use crate::mcp::oauth::{DEFAULT_REDIRECT_URI, RegistrationKind};

        /// A mock authorization server and an OAuth-protected MCP server on one origin.
        #[derive(Default)]
        struct Auth {
            base: String,
            cimd: AtomicBool,
            /// Tokens from the next code exchange expire within rmcp's refresh margin.
            short_ttl: AtomicBool,
            registrations: AtomicUsize,
            token_calls: AtomicUsize,
            refreshes: AtomicUsize,
            n: AtomicUsize,
            codes: Mutex<HashMap<String, String>>,
            access: Mutex<HashSet<String>>,
            refresh: Mutex<Option<String>>,
            last_token_request: Mutex<HashMap<String, String>>,
            last_token_auth: Mutex<Option<String>>,
        }

        impl Auth {
            fn issue(&self, short: bool) -> Value {
                let n = self.n.fetch_add(1, Ordering::SeqCst) + 1;
                let (at, rt) = (format!("at-{n}"), format!("rt-{n}"));
                self.access.lock().insert(at.clone());
                *self.refresh.lock() = Some(rt.clone());
                json!({"access_token": at, "refresh_token": rt, "token_type": "Bearer", "expires_in": if short { 5 } else { 3600 }, "scope": "tools"})
            }

            /// What the browser does: approve, and follow the redirect with a code.
            fn authorize(&self, auth_url: &str, iss: Option<&str>) -> String {
                let u = url::Url::parse(auth_url).unwrap();
                let q: HashMap<String, String> = u.query_pairs().map(|(k, v)| (k.into(), v.into())).collect();
                assert_eq!(q["code_challenge_method"], "S256");
                assert_eq!(q["resource"], format!("{}/mcp", self.base), "RFC 8707 resource");
                let code = format!("code-{}", self.codes.lock().len() + 1);
                self.codes.lock().insert(code.clone(), q["code_challenge"].clone());
                let mut redirect = url::Url::parse(&q["redirect_uri"]).unwrap();
                redirect.query_pairs_mut().append_pair("code", &code).append_pair("state", &q["state"]);
                if let Some(iss) = iss {
                    redirect.query_pairs_mut().append_pair("iss", iss);
                }
                redirect.to_string()
            }
        }

        async fn token(
            AxState(a): AxState<Arc<Auth>>,
            headers: axum::http::HeaderMap,
            Form(f): Form<HashMap<String, String>>,
        ) -> axum::response::Response {
            use axum::response::IntoResponse;
            a.token_calls.fetch_add(1, Ordering::SeqCst);
            *a.last_token_request.lock() = f.clone();
            *a.last_token_auth.lock() = headers.get("authorization").map(|v| v.to_str().unwrap().to_string());
            let invalid = || (axum::http::StatusCode::BAD_REQUEST, Json(json!({"error": "invalid_grant"}))).into_response();
            assert_eq!(f.get("resource").map(String::as_str), Some(format!("{}/mcp", a.base).as_str()), "resource on token requests");
            match f["grant_type"].as_str() {
                "authorization_code" => {
                    let Some(challenge) = a.codes.lock().remove(&f["code"]) else { return invalid() };
                    let digest = sha2::Sha256::digest(f["code_verifier"].as_bytes());
                    assert_eq!(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest), challenge, "PKCE");
                    Json(a.issue(a.short_ttl.load(Ordering::SeqCst))).into_response()
                }
                "refresh_token" => {
                    if a.refresh.lock().as_deref() != Some(f["refresh_token"].as_str()) {
                        return invalid();
                    }
                    a.refreshes.fetch_add(1, Ordering::SeqCst);
                    Json(a.issue(false)).into_response()
                }
                _ => invalid(),
            }
        }

        async fn auth_server() -> Arc<Auth> {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let a = Arc::new(Auth { base: base.clone(), ..Default::default() });
            let prm = json!({"resource": format!("{base}/mcp"), "authorization_servers": [base], "scopes_supported": ["tools"]});
            let a2 = a.clone();
            let metadata = move || {
                json!({
                    "issuer": a2.base,
                    "authorization_endpoint": format!("{}/authorize", a2.base),
                    "token_endpoint": format!("{}/token", a2.base),
                    "registration_endpoint": format!("{}/register", a2.base),
                    "response_types_supported": ["code"],
                    "grant_types_supported": ["authorization_code", "refresh_token"],
                    "code_challenge_methods_supported": ["S256"],
                    "authorization_response_iss_parameter_supported": true,
                    "client_id_metadata_document_supported": a2.cimd.load(Ordering::SeqCst),
                })
            };
            let svc: StreamableHttpService<Upstream, rmcp::transport::streamable_http_server::session::local::LocalSessionManager> =
                StreamableHttpService::new(|| Ok(Upstream), Default::default(), StreamableHttpServerConfig::default());
            let guard = {
                let a = a.clone();
                move |req: axum::extract::Request, next: axum::middleware::Next| {
                    let a = a.clone();
                    async move {
                        use axum::response::IntoResponse;
                        let token =
                            req.headers().get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer "));
                        if token.is_some_and(|t| a.access.lock().contains(t)) {
                            return next.run(req).await;
                        }
                        let challenge =
                            format!("Bearer resource_metadata=\"{}/.well-known/oauth-protected-resource/mcp\", scope=\"tools\"", a.base);
                        (axum::http::StatusCode::UNAUTHORIZED, [("www-authenticate", challenge)]).into_response()
                    }
                }
            };
            let mcp = axum::Router::new().nest_service("/mcp", svc).layer(axum::middleware::from_fn(guard));
            let a3 = a.clone();
            let router = axum::Router::new()
                .route("/.well-known/oauth-protected-resource/mcp", axum::routing::get(move || std::future::ready(Json(prm.clone()))))
                .route("/.well-known/oauth-authorization-server", axum::routing::get(move || std::future::ready(Json(metadata()))))
                .route(
                    "/register",
                    axum::routing::post(move |Json(req): Json<Value>| {
                        a3.registrations.fetch_add(1, Ordering::SeqCst);
                        std::future::ready(Json(json!({"client_id": "dyn-client", "redirect_uris": req["redirect_uris"]})))
                    }),
                )
                .route("/token", axum::routing::post(token))
                .with_state(a.clone())
                .merge(mcp);
            tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
            a
        }

        async fn oauth_harness(a: &Auth, tweak: impl FnOnce(&mut Config)) -> Harness {
            let url = format!("{}/mcp", a.base);
            harness_with(move |cfg| {
                cfg.mcp.servers.insert("cloud".into(), McpServerConfig { url, auth: McpAuth::Oauth, ..Default::default() });
                tweak(cfg);
            })
            .await
        }

        fn principal() -> Principal {
            Principal { client_id: Some("agents/client".into()), policy: Arc::new(mcp_policy(&["cloud"], &[])) }
        }

        async fn whoami(hub: &McpHub) -> Result<String, String> {
            hub.call(&principal(), "cloud", "whoami", Default::default()).await.map(|r| crate::mcp::text_of(&r))
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn login_refresh_and_relogin() {
            let a = auth_server().await;
            let h = oauth_harness(&a, |_| {}).await;
            let hub = &h.state.mcp;
            let e = whoami(hub).await.unwrap_err();
            assert!(e.contains("sign-in needed (not signed in)"), "{e}");

            // Dynamic registration; an `iss` mismatch is rejected before the code is spent.
            let start = hub.login_start("cloud").await.unwrap();
            assert_eq!(
                (start.registration, start.callback, start.redirect_uri.as_str()),
                (RegistrationKind::Dynamic, false, DEFAULT_REDIRECT_URI)
            );
            let e = hub.login_complete(&start.flow_id, &a.authorize(&start.url, Some("http://evil.example"))).await.unwrap_err();
            assert!(format!("{e:#}").to_lowercase().contains("issuer"), "{e:#}");
            assert_eq!(a.token_calls.load(Ordering::SeqCst), 0);
            assert!(hub.login_complete(&start.flow_id, "http://127.0.0.1:33418/callback?code=x&state=y").await.is_err());

            // The registration is reused; the token is sent upstream.
            let start = hub.login_start("cloud").await.unwrap();
            assert_eq!(a.registrations.load(Ordering::SeqCst), 1);
            assert_eq!(hub.login_complete(&start.flow_id, &a.authorize(&start.url, Some(&a.base))).await.unwrap(), "cloud");
            assert_eq!(whoami(hub).await.unwrap(), "Bearer at-1");
            let (_, servers) = h.call("GET", "/admin/mcp/servers", Some(&h.admin), None).await;
            let o = &servers[0]["oauth"];
            assert_eq!(
                (o["signed_in"].clone(), o["refreshable"].clone(), o["registration"].clone()),
                (json!(true), json!(true), json!("dynamic"))
            );
            assert_eq!(servers[0]["tools"], 4, "tools were listed after the login");
            let rec = h.state.secrets.get("mcp/cloud/oauth").await.unwrap().unwrap();
            assert_eq!(rec["registration"]["client_id"], "dyn-client");

            // A token the server stops accepting is refreshed and the request retried.
            a.access.lock().clear();
            assert_eq!(whoami(hub).await.unwrap(), "Bearer at-2");

            // The login survives a restart (it lives in the vault).
            let hub2 = McpHub::new(&h.state.cfg, h.state.secrets.clone(), h.state.db.clone());
            assert_eq!(whoami(&hub2).await.unwrap(), "Bearer at-2");

            // A rejected refresh token means a new login.
            a.access.lock().clear();
            *a.refresh.lock() = None;
            let e = whoami(hub).await.unwrap_err();
            assert!(e.contains("sign-in needed (the refresh token was rejected)"), "{e}");
            let e = whoami(hub).await.unwrap_err();
            assert!(e.contains("sign-in needed"), "fails fast until a new login: {e}");

            // Sign in again through xolotl's callback, then sign out.
            let start = hub.login_start("cloud").await.unwrap();
            let redirect = url::Url::parse(&a.authorize(&start.url, Some(&a.base))).unwrap();
            let (status, page) = h.call("GET", &format!("/admin/mcp/oauth/callback?{}", redirect.query().unwrap()), None, None).await;
            assert_eq!(status, 200, "{page}");
            assert!(page.as_str().unwrap().contains("signed in to cloud"));
            assert!(whoami(hub).await.unwrap().starts_with("Bearer at-"));
            let (status, _) = h.call("GET", &format!("/admin/mcp/oauth/callback?{}", redirect.query().unwrap()), None, None).await;
            assert_eq!(status, 400, "a login completes once");
            assert_eq!(h.call("DELETE", "/admin/mcp/servers/cloud/oauth", Some(&h.admin), None).await.0, 200);
            assert!(whoami(hub).await.unwrap_err().contains("sign-in needed (signed out)"));
        }

        async fn login(hub: &McpHub, a: &Auth) {
            login_to(hub, a, "cloud").await;
        }

        async fn login_to(hub: &McpHub, a: &Auth, server: &str) {
            let start = hub.login_start(server).await.unwrap();
            hub.login_complete(&start.flow_id, &a.authorize(&start.url, Some(&a.base))).await.unwrap();
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn stale_logins_and_other_instances() {
            let a = auth_server().await;
            let h = oauth_harness(&a, |_| {}).await;
            let hub = &h.state.mcp;

            // A login started before a sign-out can't sign the server back in.
            let start = hub.login_start("cloud").await.unwrap();
            let redirect = a.authorize(&start.url, Some(&a.base));
            hub.logout("cloud").await.unwrap();
            assert!(hub.login_complete(&start.flow_id, &redirect).await.is_err());
            assert!(whoami(hub).await.unwrap_err().contains("sign-in needed (signed out)"));

            // Two instances sharing the vault: a sign-out on one reaches the other.
            login(hub, &a).await;
            let other = McpHub::new(&h.state.cfg, h.state.secrets.clone(), h.state.db.clone());
            other.reload().await.unwrap();
            assert!(whoami(&other).await.is_ok());
            hub.logout("cloud").await.unwrap();
            other.reload().await.unwrap();
            assert!(whoami(&other).await.unwrap_err().contains("sign-in needed"), "the sign-out wins");
            assert!(h.state.secrets.get("mcp/cloud/oauth").await.unwrap().unwrap().get("credentials").is_none());

            // The losing side of a refresh uses the winner's tokens.
            login(hub, &a).await;
            other.reload().await.unwrap();
            assert!(whoami(&other).await.is_ok());
            a.access.lock().clear();
            assert!(whoami(hub).await.is_ok(), "refreshed (single-use refresh token spent)");
            let t = whoami(&other).await;
            assert!(t.as_ref().is_ok_and(|t| t.starts_with("Bearer at-")), "{t:?}");
            let status = other.servers().await.unwrap();
            assert_eq!(status[0].status.oauth.as_ref().unwrap().needs_login, None);
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_removed_servers_login_stays_removed() {
            let a = auth_server().await;
            let h = oauth_harness(&a, |_| {}).await;
            let hub = &h.state.mcp;
            let def = || serde_json::from_value::<McpServerConfig>(json!({"url": format!("{}/mcp", a.base), "auth": "oauth"})).unwrap();
            hub.put_server("live", def(), true).await.unwrap();
            let start = hub.login_start("live").await.unwrap();
            let redirect = a.authorize(&start.url, Some(&a.base));
            hub.delete_server("live", &[]).await.unwrap();
            hub.put_server("live", def(), true).await.unwrap();
            let e = hub.login_complete(&start.flow_id, &redirect).await.unwrap_err();
            assert!(format!("{e:#}").contains("no such login"), "{e:#}");
            assert!(h.state.secrets.get("mcp/live/oauth").await.unwrap().is_none_or(|r| r.get("credentials").is_none()));

            // Editing the description keeps the login; changing the URL doesn't.
            let start = hub.login_start("live").await.unwrap();
            hub.login_complete(&start.flow_id, &a.authorize(&start.url, Some(&a.base))).await.unwrap();
            let mut renamed = def();
            renamed.description = "new".into();
            hub.put_server("live", renamed, false).await.unwrap();
            let p = Principal { client_id: None, policy: Arc::new(mcp_policy(&["live"], &[])) };
            assert!(hub.call(&p, "live", "whoami", Default::default()).await.is_ok());
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_change_of_login_settings_ends_the_login() {
            let a = auth_server().await;
            let h = oauth_harness(&a, |_| {}).await;
            let hub = &h.state.mcp;
            let p = Principal { client_id: None, policy: Arc::new(mcp_policy(&["live", "cloud"], &[])) };
            let call = |server: &'static str| hub.call(&p, server, "whoami", Default::default());
            let def = |extra: Value| {
                let mut v = json!({"url": format!("{}/mcp", a.base), "auth": "oauth"});
                v.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
                serde_json::from_value::<McpServerConfig>(v).unwrap()
            };
            hub.put_server("live", def(json!({})), true).await.unwrap();

            // A different client id, then different scopes, end the login.
            for changed in [json!({"oauth": {"client_id": "other"}}), json!({"oauth": {"client_id": "other", "scopes": ["tools"]}})] {
                login_to(hub, &a, "live").await;
                assert!(call("live").await.is_ok());
                hub.put_server("live", def(changed.clone()), false).await.unwrap();
                let e = call("live").await.unwrap_err();
                assert!(e.contains("sign-in needed"), "{changed}: {e}");
                assert!(h.state.secrets.get("mcp/live/oauth").await.unwrap().is_none(), "{changed}: old login removed");
            }

            // So does switching to headers and back.
            login_to(hub, &a, "live").await;
            let headers = serde_json::from_value::<McpServerConfig>(json!({"url": format!("{}/mcp", a.base)})).unwrap();
            hub.put_server("live", headers, false).await.unwrap();
            hub.put_server("live", def(json!({"oauth": {"client_id": "other", "scopes": ["tools"]}})), false).await.unwrap();
            assert!(call("live").await.unwrap_err().contains("sign-in needed"));

            // A config-file server whose settings changed across a restart.
            login_to(hub, &a, "cloud").await;
            assert!(call("cloud").await.is_ok());
            let mut cfg = (*h.state.cfg).clone();
            cfg.mcp.servers.get_mut("cloud").unwrap().oauth.scopes = vec!["tools".into()];
            let restarted = McpHub::new(&cfg, h.state.secrets.clone(), h.state.db.clone());
            let e = restarted.call(&p, "cloud", "whoami", Default::default()).await.unwrap_err();
            assert!(e.contains("the server's OAuth settings changed"), "{e}");
            let unchanged = McpHub::new(&h.state.cfg, h.state.secrets.clone(), h.state.db.clone());
            assert!(unchanged.call(&p, "cloud", "whoami", Default::default()).await.is_ok());
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn tokens_near_expiry_are_refreshed_once_at_a_time() {
            let a = auth_server().await;
            a.short_ttl.store(true, Ordering::SeqCst);
            let h = oauth_harness(&a, |_| {}).await;
            let hub = &h.state.mcp;
            let start = hub.login_start("cloud").await.unwrap();
            hub.login_complete(&start.flow_id, &a.authorize(&start.url, Some(&a.base))).await.unwrap();
            // at-1 expires within the refresh margin: concurrent calls refresh
            // without spending one (single-use) refresh token twice.
            let calls = (0..6).map(|_| whoami(hub));
            for r in futures::future::join_all(calls).await {
                assert!(r.as_ref().is_ok_and(|t| t != "Bearer at-1"), "{r:?}");
            }
            assert!(a.refreshes.load(Ordering::SeqCst) >= 1);
            let status = h.call("GET", "/admin/mcp/servers", Some(&h.admin), None).await.1;
            assert_eq!(status[0]["oauth"]["needs_login"], Value::Null);
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn client_metadata_documents_and_pre_registered_clients() {
            let a = auth_server().await;
            a.cimd.store(true, Ordering::SeqCst);
            let h = oauth_harness(&a, |cfg| {
                cfg.server.public_url = Some("https://xolotl.example".into());
                cfg.mcp.servers.insert(
                    "pre".into(),
                    McpServerConfig {
                        url: format!("{}/mcp", a.base),
                        auth: McpAuth::Oauth,
                        oauth: McpServerOAuthConfig { client_id: Some("pre-1".into()), scopes: vec!["tools".into()] },
                        ..Default::default()
                    },
                );
            })
            .await;
            let hub = &h.state.mcp;

            let start = hub.login_start("cloud").await.unwrap();
            assert_eq!(start.registration, RegistrationKind::ClientMetadataDocument);
            assert!(start.callback);
            let q: HashMap<String, String> =
                url::Url::parse(&start.url).unwrap().query_pairs().map(|(k, v)| (k.into(), v.into())).collect();
            assert_eq!(q["client_id"], "https://xolotl.example/admin/mcp/oauth/client-metadata.json");
            assert_eq!(q["redirect_uri"], "https://xolotl.example/admin/mcp/oauth/callback");
            assert_eq!(a.registrations.load(Ordering::SeqCst), 0);
            let (status, doc) = h.call("GET", "/admin/mcp/oauth/client-metadata.json", None, None).await;
            assert_eq!(status, 200);
            assert_eq!(doc["client_id"], q["client_id"]);
            assert_eq!(doc["redirect_uris"], json!([q["redirect_uri"]]));

            hub.set_secret("pre", json!({"client_secret": "s3cret"}).as_object().unwrap().clone()).await.unwrap();
            let (_, servers) = h.call("GET", "/admin/mcp/servers", Some(&h.admin), None).await;
            assert_eq!(servers[1]["secret_fields"], json!({"client_secret": true}));
            let (status, start) = h.call("POST", "/admin/mcp/servers/pre/oauth/start", Some(&h.admin), None).await;
            assert_eq!(status, 200, "{start}");
            assert_eq!(start["registration"], "pre-registered");
            let q: HashMap<String, String> =
                url::Url::parse(start["url"].as_str().unwrap()).unwrap().query_pairs().map(|(k, v)| (k.into(), v.into())).collect();
            assert_eq!(q["client_id"], "pre-1");
            let redirect = a.authorize(start["url"].as_str().unwrap(), Some(&a.base));
            let flow = start["flow_id"].as_str().unwrap();
            let body = json!({"redirect_url": redirect});
            let (status, done) = h.call("POST", &format!("/admin/mcp/oauth/flows/{flow}/complete"), Some(&h.admin), Some(body)).await;
            assert_eq!(status, 200, "{done}");
            let (_, flow_status) = h.call("GET", &format!("/admin/mcp/oauth/flows/{flow}"), Some(&h.admin), None).await;
            assert_eq!(flow_status["status"], "complete");
            // The pre-registered secret authenticates the token request.
            let basic = a.last_token_auth.lock().clone();
            let sent_secret = a.last_token_request.lock().get("client_secret").cloned();
            assert!(basic.is_some_and(|b| b.starts_with("Basic ")) || sent_secret.as_deref() == Some("s3cret"));
            assert_eq!(
                hub.call(&principal_for("pre"), "pre", "add", json!({"a": 1, "b": 2}).as_object().unwrap().clone())
                    .await
                    .unwrap()
                    .structured_content,
                Some(json!({"sum": 3.0}))
            );
        }

        fn principal_for(server: &str) -> Principal {
            Principal { client_id: None, policy: Arc::new(mcp_policy(&[server], &[])) }
        }
    }
}

#[tokio::test]
async fn opencode_sessions_reach_upstream_on_native_translated_and_passthrough_routes() {
    let h = harness().await;
    h.account(Credential { provider: SubProvider::OpencodeGo, api_key: Some("test-key".into()), ..Default::default() }).await;
    let key = h.key(policy("session-test")).await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"choices": [{"message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}]})),
        )
        .mount(&h.upstream)
        .await;
    for route in ["/v1/chat/completions", "/v1/messages", "/opencode-go/v1/chat/completions"] {
        for source in [None, Some("x-opencode-session"), Some("x-claude-code-session-id"), Some("session_id")] {
            let auth = format!("Bearer {key}");
            let mut headers = vec![("authorization", auth.as_str()), ("user-agent", "sample-client/1.0")];
            if let Some(source) = source {
                headers.push((source, "conversation-1"));
            }
            let (status, body) = h
                .call_with(
                    "POST",
                    route,
                    &headers,
                    Some(json!({
                        "model": "opencode-go/kimi-k3", "max_tokens": 16,
                        "messages": [{"role": "user", "content": "hello"}]
                    })),
                )
                .await;
            assert_eq!(status, 200, "{route}: {body}");
        }
    }
    let requests = h.upstream.received_requests().await.unwrap();
    assert_eq!(requests.len(), 12);
    let derived = requests[0].headers["x-opencode-session"].to_str().unwrap();
    assert!(derived.starts_with("xolotl-"));
    for (i, req) in requests.iter().enumerate() {
        assert_eq!(req.headers["x-opencode-session"], if i % 4 == 0 { derived } else { "conversation-1" });
        assert_eq!(req.headers["user-agent"], concat!("xolotl/", env!("CARGO_PKG_VERSION"), " (sample-client/1.0)"));
    }
}

#[tokio::test]
async fn catalog_discovery_reports_scope_errors_and_cached_models() {
    let h = harness().await;
    let (status, initial) = h.call("GET", "/admin/catalog/status", Some(&h.admin), None).await;
    assert_eq!(status, 200);
    assert!(initial["llama"]["refreshed_at"].is_null());
    assert_eq!(initial["llama"]["models"], 2);
    assert_eq!(h.call("GET", "/admin/catalog/status", None, None).await.0, 401);
    assert_eq!(h.call("POST", "/admin/catalog/refresh?providers=missing", Some(&h.admin), None).await.0, 400);
    assert_eq!(h.call("GET", "/admin/catalog/status", Some(&h.admin), None).await.1, initial);

    Mock::given(method("GET"))
        .and(path("/llama/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [{"id": "sample-model"}]})))
        .expect(1)
        .mount(&h.upstream)
        .await;
    let (status, models) = h.call("POST", "/admin/catalog/refresh?providers=llama", Some(&h.admin), None).await;
    assert_eq!(status, 200);
    // The existing CLI response remains a provider -> model list map.
    assert!(models["llama"].as_array().unwrap().contains(&json!("sample-model")));
    let (_, discovered) = h.call("GET", "/admin/catalog/status", Some(&h.admin), None).await;
    assert!(discovered["llama"]["refreshed_at"].as_i64().is_some());
    assert_eq!(discovered["llama"]["models"], 3);
    assert!(discovered["llama"]["error"].is_null());
    assert_eq!(discovered["codex"], initial["codex"], "scoped refresh must leave other providers untouched");

    h.upstream.reset().await;
    Mock::given(method("GET")).and(path("/llama/v1/models")).respond_with(ResponseTemplate::new(403)).mount(&h.upstream).await;
    assert_eq!(h.call("POST", "/admin/catalog/refresh?providers=llama,opencode-go", Some(&h.admin), None).await.0, 200);
    let (_, failed) = h.call("GET", "/admin/catalog/status", Some(&h.admin), None).await;
    assert!(failed["llama"]["error"].as_str().unwrap().contains("403"));
    assert_eq!(failed["llama"]["models"], 3, "failed discovery retains cached and static models");
    assert_eq!(failed["opencode-go"]["error"], "no available accounts");
    let (_, retained) = h.call("GET", "/admin/catalog", Some(&h.admin), None).await;
    assert_eq!(retained["llama"], models["llama"]);
}

#[tokio::test]
async fn catalog_refresh_distinguishes_empty_success_from_failure() {
    let h = harness_with(|cfg| cfg.catalog.static_models.clear()).await;
    Mock::given(method("GET"))
        .and(path("/llama/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
        .mount(&h.upstream)
        .await;
    assert_eq!(h.call("POST", "/admin/catalog/refresh", Some(&h.admin), None).await.0, 200);
    let (_, status) = h.call("GET", "/admin/catalog/status", Some(&h.admin), None).await;
    assert_eq!(status["llama"]["models"], 0);
    assert!(status["llama"]["error"].is_null());
    assert!(status["llama"]["refreshed_at"].as_i64().is_some());
    assert_eq!(status["opencode-go"]["models"], 0);
    assert_eq!(status["opencode-go"]["error"], "no available accounts");
}

#[tokio::test]
async fn resetting_account_automatically_rediscovers_models() {
    let h = harness_with(|cfg| cfg.catalog.static_models.clear()).await;
    let id = h.account(Credential { provider: SubProvider::OpencodeGo, api_key: Some("sample-key".into()), ..Default::default() }).await;
    h.state.db.mark_auth_failed(&id, true, Some("sample failure")).await.unwrap();
    h.call("POST", "/admin/catalog/refresh?providers=opencode-go", Some(&h.admin), None).await;
    assert_eq!(h.state.catalog.discovery_status()["opencode-go"].error.as_deref(), Some("no available accounts"));
    Mock::given(method("GET"))
        .and(path("/zen/go/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [{"id": "sample-model"}]})))
        .mount(&h.upstream)
        .await;
    assert_eq!(h.call("POST", &format!("/admin/accounts/{id}/reset"), Some(&h.admin), None).await.0, 200);
    // The mutation responds before background discovery completes.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if h.state.catalog.models()["opencode-go"].contains("sample-model") {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(h.state.catalog.discovery_status()["opencode-go"].error.is_none());
}

#[tokio::test]
async fn concurrent_scoped_catalog_refreshes_preserve_both_results() {
    let h = harness_with(|cfg| cfg.catalog.static_models.clear()).await;
    h.account(Credential { provider: SubProvider::OpencodeGo, api_key: Some("sample-key".into()), ..Default::default() }).await;
    Mock::given(method("GET"))
        .and(path("/llama/v1/models"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_millis(50))
                .set_body_json(json!({"data": [{"id": "sample-one"}]})),
        )
        .mount(&h.upstream)
        .await;
    Mock::given(method("GET"))
        .and(path("/zen/go/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [{"id": "sample-two"}]})))
        .mount(&h.upstream)
        .await;
    let (a, b) = tokio::join!(
        h.call("POST", "/admin/catalog/refresh?providers=llama", Some(&h.admin), None),
        h.call("POST", "/admin/catalog/refresh?providers=opencode-go", Some(&h.admin), None),
    );
    assert_eq!((a.0, b.0), (200, 200));
    let models = h.state.catalog.models();
    assert!(models["llama"].contains("sample-one"));
    assert!(models["opencode-go"].contains("sample-two"));
    let status = h.state.catalog.discovery_status();
    assert!(status["llama"].refreshed_at.is_some());
    assert!(status["opencode-go"].refreshed_at.is_some());
}
