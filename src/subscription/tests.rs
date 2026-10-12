use std::sync::Arc;

use axum::http::{HeaderMap, Method};
use bytes::Bytes;
use serde_json::json;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::config::{Endpoints, SubscriptionConfig};
use crate::secrets::MemoryStore;

async fn setup(server: &MockServer, strategy: Strategy) -> (Arc<Subscriptions>, Secrets) {
    let secrets: Secrets = Arc::new(MemoryStore::default());
    let uri = server.uri();
    let cfg = SubscriptionConfig {
        strategy,
        endpoints: Endpoints {
            codex_backend: format!("{uri}/backend-api"),
            codex_auth: uri.clone(),
            claude_api: uri.clone(),
            claude_authorize: format!("{uri}/oauth/authorize"),
            claude_token: format!("{uri}/v1/oauth/token"),
            claude_manual_redirect: format!("{uri}/oauth/code/callback"),
            opencode_go: format!("{uri}/zen/go"),
        },
        ..Default::default()
    };
    let db = Db::in_memory().await.unwrap();
    (Arc::new(Subscriptions::new(cfg, db, secrets.clone(), reqwest::Client::new())), secrets)
}

fn codex_cred(acct: &str, token: &str) -> Credential {
    Credential {
        provider: SubProvider::Codex,
        access_token: token.into(),
        refresh_token: Some(format!("rt-{acct}")),
        expires_at: Some(now() + 3600),
        account_id: Some(acct.into()),
        email: Some(format!("{acct}@example.com")),
        ..Default::default()
    }
}

fn responses_body() -> Bytes {
    Bytes::from(json!({"model": "gpt-5-codex", "input": [], "instructions": "x"}).to_string())
}

async fn run(subs: &Subscriptions, provider: SubProvider, path: &str, headers: HeaderMap, body: Bytes) -> (u16, String) {
    let resp = subs.execute(provider, Method::POST, path, headers, body, Some("client"), Some("m")).await;
    let status = resp.status().as_u16();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

const SSE_OK: &str = "event: response.created\ndata: {\"type\":\"response.created\"}\n\nevent: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}}\n\n";

#[tokio::test]
async fn codex_fails_over_on_usage_limit_and_cools_down() {
    let server = MockServer::start().await;
    let (subs, _) = setup(&server, Strategy::FillFirst).await;
    let a = subs.save_account(codex_cred("acct-a", "tok-a")).await.unwrap();
    let b = subs.save_account(codex_cred("acct-b", "tok-b")).await.unwrap();
    subs.db.set_account_priority(&a.id, 10).await.unwrap();

    Mock::given(method("POST"))
        .and(path("/backend-api/codex/responses"))
        .and(header("authorization", "Bearer tok-a"))
        .respond_with(ResponseTemplate::new(429).set_body_json(json!({"error": {"type": "usage_limit_reached", "resets_in_seconds": 600}})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/responses"))
        .and(header("authorization", "Bearer tok-b"))
        .and(header("chatgpt-account-id", "acct-b"))
        .and(body_partial_json(json!({"stream": true, "store": false})))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-codex-primary-used-percent", "40")
                .insert_header("x-codex-primary-window-minutes", "300")
                .set_body_raw(SSE_OK, "text/event-stream"),
        )
        .mount(&server)
        .await;

    let (status, body) = run(&subs, SubProvider::Codex, "/v1/responses", HeaderMap::new(), responses_body()).await;
    assert_eq!(status, 200);
    assert!(body.contains("response.completed"));

    let a = subs.db.get_account(&a.id).await.unwrap().unwrap();
    assert!(a.cooldown_until.unwrap() >= now() + 590, "exhausted account cools down until reset");
    let usage = subs.db.get_usage(&b.id).await.unwrap().unwrap();
    assert_eq!(usage.windows[0].name, "5h");
    assert_eq!(usage.windows[0].used_percent, 40.0);

    // Request log records the serving account and token usage.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let s = subs.db.usage_summary("account", 0).await.unwrap();
    let row = s.iter().find(|r| r.key.as_deref() == Some(b.id.as_str())).unwrap();
    assert_eq!(row.input_tokens, 5);
}

#[tokio::test]
async fn codex_in_stream_quota_error_fails_over_before_bytes_sent() {
    let server = MockServer::start().await;
    let (subs, _) = setup(&server, Strategy::FillFirst).await;
    let a = subs.save_account(codex_cred("acct-a", "tok-a")).await.unwrap();
    subs.save_account(codex_cred("acct-b", "tok-b")).await.unwrap();
    subs.db.set_account_priority(&a.id, 10).await.unwrap();
    let in_stream_error = "event: response.created\ndata: {\"type\":\"response.created\"}\n\nevent: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"usage_limit_reached\",\"resets_in_seconds\":120}}\n\n";
    Mock::given(header("authorization", "Bearer tok-a"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(in_stream_error, "text/event-stream"))
        .mount(&server)
        .await;
    Mock::given(header("authorization", "Bearer tok-b"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(SSE_OK, "text/event-stream"))
        .mount(&server)
        .await;
    let (status, body) = run(&subs, SubProvider::Codex, "/v1/responses", HeaderMap::new(), responses_body()).await;
    assert_eq!(status, 200);
    assert!(body.contains("\"delta\":\"hi\""));
    assert!(!body.contains("usage_limit_reached"));
    assert!(subs.db.get_account(&a.id).await.unwrap().unwrap().cooling_down(now()));
}

#[tokio::test]
async fn refreshes_on_401_and_persists_rotated_token() {
    let server = MockServer::start().await;
    let (subs, secrets) = setup(&server, Strategy::FillFirst).await;
    let a = subs.save_account(codex_cred("acct-a", "stale")).await.unwrap();
    Mock::given(header("authorization", "Bearer stale"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({"error": {"code": "token_expired"}})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"access_token": "fresh", "refresh_token": "rt-new", "expires_in": 864000})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(header("authorization", "Bearer fresh"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(SSE_OK, "text/event-stream"))
        .mount(&server)
        .await;
    let (status, _) = run(&subs, SubProvider::Codex, "/v1/responses", HeaderMap::new(), responses_body()).await;
    assert_eq!(status, 200);
    let stored: Credential = get_typed(&*secrets, &format!("accounts/codex/{}", a.id)).await.unwrap().unwrap();
    assert_eq!(stored.access_token, "fresh");
    assert_eq!(stored.refresh_token.as_deref(), Some("rt-new"));
    assert_eq!(stored.account_id.as_deref(), Some("acct-a"), "identity kept across refresh");
}

#[tokio::test]
async fn terminal_refresh_failure_marks_account_for_relogin() {
    let server = MockServer::start().await;
    let (subs, _) = setup(&server, Strategy::FillFirst).await;
    let mut cred = codex_cred("acct-a", "old");
    cred.expires_at = Some(now() - 10);
    let a = subs.save_account(cred).await.unwrap();
    Mock::given(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": "refresh_token_reused"})))
        .mount(&server)
        .await;
    let (status, _) = run(&subs, SubProvider::Codex, "/v1/responses", HeaderMap::new(), responses_body()).await;
    assert_eq!(status, 503);
    assert!(subs.db.get_account(&a.id).await.unwrap().unwrap().auth_failed);
}

#[tokio::test]
async fn all_accounts_cooling_returns_429_with_retry_after() {
    let server = MockServer::start().await;
    let (subs, _) = setup(&server, Strategy::RoundRobin).await;
    let a = subs.save_account(codex_cred("acct-a", "tok-a")).await.unwrap();
    subs.db.set_cooldown(&a.id, now() + 300, "test").await.unwrap();
    let resp = subs.execute(SubProvider::Codex, Method::POST, "/v1/responses", HeaderMap::new(), responses_body(), None, None).await;
    assert_eq!(resp.status(), 429);
    let ra: i64 = resp.headers()["retry-after"].to_str().unwrap().parse().unwrap();
    assert!((295..=300).contains(&ra));
}

#[tokio::test]
async fn claude_round_robin_with_session_affinity() {
    let server = MockServer::start().await;
    let (subs, _) = setup(&server, Strategy::RoundRobin).await;
    for acct in ["u1", "u2"] {
        subs.save_account(Credential {
            provider: SubProvider::Claude,
            access_token: format!("tok-{acct}"),
            refresh_token: Some("rt".into()),
            expires_at: Some(now() + 3600),
            account_id: Some(acct.into()),
            device_id: Some("dev".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    }
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"usage": {"input_tokens": 1, "output_tokens": 1}})))
        .mount(&server)
        .await;

    let mut h = HeaderMap::new();
    h.insert("x-claude-code-session-id", "sess-1".parse().unwrap());
    let body = Bytes::from(json!({"model": "claude-opus-5", "messages": []}).to_string());
    for _ in 0..3 {
        let (status, _) = run(&subs, SubProvider::Claude, "/v1/messages", h.clone(), body.clone()).await;
        assert_eq!(status, 200);
    }
    // Without a session, round robin alternates.
    for _ in 0..2 {
        run(&subs, SubProvider::Claude, "/v1/messages", HeaderMap::new(), body.clone()).await;
    }
    let reqs = server.received_requests().await.unwrap();
    let tokens: Vec<String> = reqs.iter().map(|r| r.headers["authorization"].to_str().unwrap().to_string()).collect();
    assert!(tokens[..3].iter().all(|t| t == &tokens[0]), "session pinned: {tokens:?}");
    assert_ne!(tokens[3], tokens[4], "round robin: {tokens:?}");
    // OAuth beta and user_id rewrite reached upstream.
    let first = &reqs[0];
    assert!(first.headers["anthropic-beta"].to_str().unwrap().contains("oauth-2025-04-20"));
    assert!(first.url.query().unwrap_or("").contains("beta=true"));
}

#[tokio::test]
async fn claude_fatal_errors_are_returned_without_failover() {
    let server = MockServer::start().await;
    let (subs, _) = setup(&server, Strategy::FillFirst).await;
    for acct in ["u1", "u2"] {
        subs.save_account(Credential {
            provider: SubProvider::Claude,
            access_token: format!("tok-{acct}"),
            expires_at: Some(now() + 3600),
            account_id: Some(acct.into()),
            ..Default::default()
        })
        .await
        .unwrap();
    }
    Mock::given(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_json(json!({"type": "error", "error": {"type": "invalid_request_error", "message": "bad"}})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let (status, body) = run(&subs, SubProvider::Claude, "/v1/messages", HeaderMap::new(), Bytes::from_static(b"{\"model\":\"m\"}")).await;
    assert_eq!(status, 400);
    assert!(body.contains("invalid_request_error"));
}

#[tokio::test]
async fn usage_poll_benches_exhausted_accounts() {
    let server = MockServer::start().await;
    let (subs, _) = setup(&server, Strategy::FillFirst).await;
    let a = subs.save_account(codex_cred("acct-a", "tok-a")).await.unwrap();
    let reset = now() + 7200;
    Mock::given(method("GET"))
        .and(path("/backend-api/wham/usage"))
        .and(header("chatgpt-account-id", "acct-a"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"plan_type": "pro", "rate_limit": {
            "primary_window": {"used_percent": 100, "limit_window_seconds": 18000, "reset_at": reset},
            "secondary_window": {"used_percent": 30, "limit_window_seconds": 604800, "reset_at": reset + 1000}}})))
        .mount(&server)
        .await;
    let u = subs.poll_usage(&a.id).await.unwrap();
    assert_eq!(u.windows.len(), 2);
    let row = subs.db.get_account(&a.id).await.unwrap().unwrap();
    assert_eq!(row.cooldown_until, Some(reset));
    assert_eq!(row.plan.as_deref(), Some("pro"));
}

#[tokio::test]
async fn api_call_substitutes_token_and_restricts_hosts() {
    let server = MockServer::start().await;
    let (subs, _) = setup(&server, Strategy::FillFirst).await;
    let a = subs.save_account(codex_cred("acct-a", "tok-a")).await.unwrap();
    Mock::given(method("GET"))
        .and(path("/backend-api/wham/usage"))
        .and(header("authorization", "Bearer tok-a"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"rate_limit": null})))
        .mount(&server)
        .await;
    let mut h = HashMap::new();
    h.insert("Authorization".to_string(), "Bearer $TOKEN$".to_string());
    let (status, _, body) = subs.api_call(&a, "GET", &format!("{}/backend-api/wham/usage", server.uri()), &h, None).await.unwrap();
    assert_eq!(status, 200);
    assert!(body.contains("rate_limit"));
    let err = subs.api_call(&a, "GET", "https://evil.example.com/steal", &h, None).await.unwrap_err();
    assert!(err.to_string().contains("not allowed"));
}

#[tokio::test]
async fn claude_login_flow_with_pasted_code() {
    let server = MockServer::start().await;
    let (subs, _) = setup(&server, Strategy::FillFirst).await;
    let start = subs.start_login(SubProvider::Claude, false).await.unwrap();
    assert!(start.needs_code);
    let url = url::Url::parse(&start.url).unwrap();
    let state = url.query_pairs().find(|(k, _)| k == "state").unwrap().1.to_string();
    assert!(url.query_pairs().any(|(k, v)| k == "code" && v == "true"));

    Mock::given(method("POST"))
        .and(path("/v1/oauth/token"))
        .and(body_partial_json(json!({"grant_type": "authorization_code", "code": "the-code", "state": state})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "sk-ant-oat01-x", "refresh_token": "sk-ant-ort01-y", "expires_in": 28800,
            "account": {"uuid": "acct-uuid", "email_address": "Me@Example.com"},
            "organization": {"uuid": "org-uuid", "name": "Org"}})))
        .mount(&server)
        .await;

    assert!(subs.complete_login(&start.flow_id, "the-code#wrong-state").await.is_err());
    let row = subs.complete_login(&start.flow_id, &format!("the-code#{state}")).await.unwrap();
    assert_eq!(row.provider, "claude");
    assert_eq!(row.email.as_deref(), Some("me@example.com"));
    assert!(matches!(subs.flow_status(&start.flow_id), Some(FlowStatus::Complete { .. })));

    // Logging in to the same account again replaces it rather than duplicating.
    let again = subs.start_login(SubProvider::Claude, false).await.unwrap();
    let state2 = url::Url::parse(&again.url).unwrap().query_pairs().find(|(k, _)| k == "state").unwrap().1.to_string();
    let row2 = subs.complete_login(&again.flow_id, &format!("the-code#{state2}")).await;
    // The mock only matches the first state; a mismatch upstream is an error, not a duplicate.
    assert!(row2.is_err() || row2.unwrap().id == row.id);
    assert_eq!(subs.db.list_accounts(Some("claude")).await.unwrap().len(), 1);
}

#[tokio::test]
async fn codex_device_flow_completes_in_background() {
    let server = MockServer::start().await;
    let (subs, _) = setup(&server, Strategy::FillFirst).await;
    Mock::given(path("/api/accounts/deviceauth/usercode"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"device_auth_id": "dev-1", "user_code": "ABCD-1234", "interval": "1"})),
        )
        .mount(&server)
        .await;
    Mock::given(path("/api/accounts/deviceauth/token")).respond_with(ResponseTemplate::new(403)).up_to_n_times(1).mount(&server).await;
    Mock::given(path("/api/accounts/deviceauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"authorization_code": "ac", "code_verifier": "cv"})))
        .mount(&server)
        .await;
    let id_token = format!(
        "h.{}.s",
        base64::Engine::encode(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
            json!({"email": "d@x.com", "https://api.openai.com/auth": {"chatgpt_account_id": "acct-d", "chatgpt_plan_type": "plus"}})
                .to_string()
        )
    );
    Mock::given(path("/oauth/token"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"access_token": "at", "refresh_token": "rt", "id_token": id_token, "expires_in": 3600})),
        )
        .mount(&server)
        .await;

    let start = subs.start_login(SubProvider::Codex, true).await.unwrap();
    assert_eq!(start.user_code.as_deref(), Some("ABCD-1234"));
    for _ in 0..50 {
        if let Some(FlowStatus::Complete { email, .. }) = subs.flow_status(&start.flow_id) {
            assert_eq!(email.as_deref(), Some("d@x.com"));
            let rows = subs.db.list_accounts(Some("codex")).await.unwrap();
            assert_eq!(rows[0].plan.as_deref(), Some("plus"));
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("device flow did not complete: {:?}", subs.flow_status(&start.flow_id));
}

#[tokio::test]
async fn sync_loads_accounts_from_vault() {
    let server = MockServer::start().await;
    let (subs, secrets) = setup(&server, Strategy::FillFirst).await;
    put_typed(
        &*secrets,
        "accounts/opencode-go/ocg-1",
        &Credential { provider: SubProvider::OpencodeGo, api_key: Some("k".into()), label: Some("main".into()), ..Default::default() },
    )
    .await
    .unwrap();
    assert_eq!(subs.sync().await.unwrap(), 1);
    let rows = subs.db.list_accounts(Some("opencode-go")).await.unwrap();
    assert_eq!(rows[0].label, "main");
}

/// A store whose writes can be made to fail, standing in for an OpenBao outage.
#[derive(Default)]
struct FlakyStore {
    inner: MemoryStore,
    fail_writes: std::sync::atomic::AtomicBool,
}

impl FlakyStore {
    fn check(&self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.fail_writes.load(std::sync::atomic::Ordering::SeqCst), "vault unavailable");
        Ok(())
    }
}

#[async_trait::async_trait]
impl crate::secrets::SecretStore for FlakyStore {
    async fn get(&self, key: &str) -> anyhow::Result<Option<serde_json::Map<String, Value>>> {
        self.inner.get(key).await
    }
    async fn put(&self, key: &str, data: serde_json::Map<String, Value>) -> anyhow::Result<()> {
        self.check()?;
        self.inner.put(key, data).await
    }
    async fn delete(&self, key: &str) -> anyhow::Result<()> {
        self.inner.delete(key).await
    }
    async fn list(&self, dir: &str) -> anyhow::Result<Vec<String>> {
        self.inner.list(dir).await
    }
    async fn get_versioned(&self, key: &str) -> anyhow::Result<(Option<serde_json::Map<String, Value>>, u64)> {
        self.inner.get_versioned(key).await
    }
    async fn put_cas(&self, key: &str, data: serde_json::Map<String, Value>, cas: u64) -> anyhow::Result<u64> {
        self.check()?;
        self.inner.put_cas(key, data, cas).await
    }
    async fn destroy_versions(&self, key: &str, versions: &[u64]) -> anyhow::Result<()> {
        self.inner.destroy_versions(key, versions).await
    }
}

#[tokio::test]
async fn refreshed_credentials_survive_vault_outage() {
    let server = MockServer::start().await;
    let store = Arc::new(FlakyStore::default());
    let secrets: Secrets = store.clone();
    let uri = server.uri();
    let cfg = SubscriptionConfig {
        endpoints: Endpoints { codex_backend: format!("{uri}/backend-api"), codex_auth: uri.clone(), ..Default::default() },
        ..Default::default()
    };
    let subs = Subscriptions::new(cfg, Db::in_memory().await.unwrap(), secrets.clone(), reqwest::Client::new());
    let mut cred = codex_cred("acct-a", "old");
    cred.expires_at = Some(now() - 10);
    let a = subs.save_account(cred).await.unwrap();

    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"access_token": "fresh", "refresh_token": "rt-new", "expires_in": 3600})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(header("authorization", "Bearer fresh"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(SSE_OK, "text/event-stream"))
        .mount(&server)
        .await;

    // Vault goes down: the refresh still serves the request from memory.
    store.fail_writes.store(true, std::sync::atomic::Ordering::SeqCst);
    let (status, _) = run(&subs, SubProvider::Codex, "/v1/responses", HeaderMap::new(), responses_body()).await;
    assert_eq!(status, 200);
    let view = subs.accounts(None).await.unwrap();
    assert!(view[0].unsaved, "account flagged as unsaved");
    let stored: Credential = get_typed(&*secrets, &format!("accounts/codex/{}", a.id)).await.unwrap().unwrap();
    assert_eq!(stored.access_token, "old", "vault still holds the spent token");

    // Retries keep failing while the vault is down, without refreshing again.
    assert_eq!(subs.flush_unsaved().await, 1);
    let (status, _) = run(&subs, SubProvider::Codex, "/v1/responses", HeaderMap::new(), responses_body()).await;
    assert_eq!(status, 200);

    // Vault is back: the next flush saves the rotated credential.
    store.fail_writes.store(false, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(subs.flush_unsaved().await, 0);
    let stored: Credential = get_typed(&*secrets, &format!("accounts/codex/{}", a.id)).await.unwrap().unwrap();
    assert_eq!(stored.access_token, "fresh");
    assert_eq!(stored.refresh_token.as_deref(), Some("rt-new"));
    assert!(!subs.accounts(None).await.unwrap()[0].unsaved);

    // A periodic re-sync must not overwrite the fresh in-memory credential either.
    subs.sync().await.unwrap();
    assert_eq!(subs.credential(&a.id, None).await.unwrap().access_token, "fresh");
}
