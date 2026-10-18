use std::sync::Arc;

use axum::http::{HeaderMap, Method};
use bytes::Bytes;
use serde_json::json;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::config::{Endpoints, SubscriptionConfig};
use crate::secrets::MemoryStore;
use crate::secrets::SecretStore;
use crate::secrets::testing::GatedStore;

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
            grok_api: format!("{uri}/grok/v1"),
            grok_auth: uri.clone(),
        },
        ..Default::default()
    };
    let db = Db::in_memory().await.unwrap();
    (
        Arc::new(Subscriptions::new(cfg, db, secrets.clone(), crate::proxy::upstream_client(std::time::Duration::from_secs(30)).unwrap())),
        secrets,
    )
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

fn principal(policy: crate::policy::Policy) -> Principal {
    Principal { client_id: Some(format!("{}/client", policy.name)), policy: Arc::new(policy) }
}

fn any_policy() -> crate::policy::Policy {
    crate::policy::Policy {
        name: "p".into(),
        providers: vec!["*".into()],
        accounts: SubProvider::ALL.iter().map(|p| (p.as_str().to_string(), vec!["*".to_string()])).collect(),
        ..Default::default()
    }
}

async fn run(subs: &Subscriptions, provider: SubProvider, path: &str, headers: HeaderMap, body: Bytes) -> (u16, String) {
    run_as(subs, &principal(any_policy()), provider, path, headers, body).await.1
}

/// Status, `retry-after` and body of one request under a key of the policy.
async fn run_as(
    subs: &Subscriptions,
    principal: &Principal,
    provider: SubProvider,
    path: &str,
    headers: HeaderMap,
    body: Bytes,
) -> (Option<String>, (u16, String)) {
    let resp = subs.execute(provider, Method::POST, path, headers, body, Some(principal), Some("m"), &mut Charge::default()).await;
    let status = resp.status().as_u16();
    let retry = resp.headers().get("retry-after").and_then(|v| v.to_str().ok()).map(str::to_string);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    (retry, (status, String::from_utf8_lossy(&bytes).to_string()))
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
    let s = subs.db.usage_summary("account", 0, &Default::default()).await.unwrap();
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
    let row = subs.db.get_account(&a.id).await.unwrap().unwrap();
    assert!(row.auth_failed);
    assert!(row.last_error.unwrap().contains("Stop sharing the login"));
}

#[tokio::test]
async fn all_accounts_cooling_returns_429_with_retry_after() {
    let server = MockServer::start().await;
    let (subs, _) = setup(&server, Strategy::RoundRobin).await;
    let a = subs.save_account(codex_cred("acct-a", "tok-a")).await.unwrap();
    subs.db.set_cooldown(&a.id, now() + 300, "test").await.unwrap();
    let resp = subs
        .execute(SubProvider::Codex, Method::POST, "/v1/responses", HeaderMap::new(), responses_body(), None, None, &mut Charge::default())
        .await;
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
#[tokio::test]
async fn refreshed_credentials_survive_vault_outage() {
    let server = MockServer::start().await;
    let store = Arc::new(GatedStore::default());
    let secrets: Secrets = store.clone();
    let uri = server.uri();
    let cfg = SubscriptionConfig {
        endpoints: Endpoints { codex_backend: format!("{uri}/backend-api"), codex_auth: uri.clone(), ..Default::default() },
        ..Default::default()
    };
    let subs = Subscriptions::new(
        cfg,
        Db::in_memory().await.unwrap(),
        secrets.clone(),
        crate::proxy::upstream_client(std::time::Duration::from_secs(30)).unwrap(),
    );
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
    store.set_fail_writes(true);
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
    store.set_fail_writes(false);
    assert_eq!(subs.flush_unsaved().await, 0);
    let stored: Credential = get_typed(&*secrets, &format!("accounts/codex/{}", a.id)).await.unwrap().unwrap();
    assert_eq!(stored.access_token, "fresh");
    assert_eq!(stored.refresh_token.as_deref(), Some("rt-new"));
    assert!(!subs.accounts(None).await.unwrap()[0].unsaved);

    // A periodic re-sync must not overwrite the fresh in-memory credential either.
    subs.sync().await.unwrap();
    assert_eq!(subs.credential(&a.id, None).await.unwrap().access_token, "fresh");
}

/// Subscriptions over a gated store, with Codex endpoints on the mock server.
async fn gated(server: &MockServer) -> (Arc<Subscriptions>, Arc<GatedStore>) {
    let store = Arc::new(GatedStore::default());
    let uri = server.uri();
    let cfg = SubscriptionConfig {
        endpoints: Endpoints { codex_backend: format!("{uri}/backend-api"), codex_auth: uri.clone(), ..Default::default() },
        ..Default::default()
    };
    let http = crate::proxy::upstream_client(std::time::Duration::from_secs(30)).unwrap();
    (Arc::new(Subscriptions::new(cfg, Db::in_memory().await.unwrap(), store.clone(), http)), store)
}

/// Token endpoint that hands out `gen1`, `gen2`, ... on successive refreshes.
async fn mount_refreshes(server: &MockServer, n: usize) {
    for i in (1..=n).rev() {
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"access_token": format!("gen{i}"), "refresh_token": format!("rt{i}"), "expires_in": 3600})),
            )
            .up_to_n_times(1)
            .with_priority(i as u8)
            .mount(server)
            .await;
    }
}

#[tokio::test]
async fn removed_account_stays_removed_after_inflight_refresh() {
    let server = MockServer::start().await;
    let (subs, store) = gated(&server).await;
    mount_refreshes(&server, 1).await;
    let mut cred = codex_cred("acct-a", "old");
    cred.expires_at = Some(now() - 10);
    let id = subs.save_account(cred).await.unwrap().id;
    let key = format!("accounts/codex/{id}");

    // A refresh pauses on its vault write while the account is removed.
    store.arm("put", &key);
    let refresh = tokio::spawn({
        let subs = subs.clone();
        let id = id.clone();
        async move { subs.credential(&id, None).await }
    });
    store.reached.notified().await;
    let remove = tokio::spawn({
        let subs = subs.clone();
        let id = id.clone();
        async move { subs.remove_account(&id).await.unwrap() }
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    store.release.notify_one();
    let _ = refresh.await.unwrap();
    assert!(remove.await.unwrap());

    subs.sync().await.unwrap();
    assert!(store.get(&key).await.unwrap().is_none(), "vault record stays deleted");
    assert!(subs.accounts(None).await.unwrap().is_empty(), "account does not come back");
    assert!(subs.credential(&id, None).await.is_err());
}

#[tokio::test]
async fn delayed_flush_does_not_overwrite_a_newer_refresh() {
    let server = MockServer::start().await;
    let (subs, store) = gated(&server).await;
    mount_refreshes(&server, 2).await;
    let mut cred = codex_cred("acct-a", "old");
    cred.expires_at = Some(now() - 10);
    let id = subs.save_account(cred).await.unwrap().id;
    let key = format!("accounts/codex/{id}");

    // gen1 is refreshed while the vault is down, so it is only in memory.
    store.set_fail_writes(true);
    assert_eq!(subs.credential(&id, None).await.unwrap().access_token, "gen1");
    assert_eq!(subs.unsaved_count(), 1);
    store.set_fail_writes(false);

    // The retry pauses on its write; meanwhile gen1 is rejected and refreshed to gen2.
    store.arm("put", &key);
    let flush = tokio::spawn({
        let subs = subs.clone();
        async move { subs.flush_unsaved().await }
    });
    store.reached.notified().await;
    let refresh = tokio::spawn({
        let subs = subs.clone();
        let id = id.clone();
        async move { subs.credential(&id, Some("gen1")).await.unwrap() }
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    store.release.notify_one();
    flush.await.unwrap();
    assert_eq!(refresh.await.unwrap().access_token, "gen2");

    let stored: Credential = get_typed(&*store, &key).await.unwrap().unwrap();
    assert_eq!(stored.access_token, "gen2", "vault keeps the newest credential");
    assert_eq!(stored.refresh_token.as_deref(), Some("rt2"));
    assert_eq!(subs.unsaved_count(), 0);
}

#[tokio::test]
async fn api_call_does_not_follow_redirects() {
    let server = MockServer::start().await;
    let elsewhere = MockServer::start().await;
    let (subs, _) = setup(&server, Strategy::FillFirst).await;
    let a = subs.save_account(codex_cred("acct-a", "tok-a")).await.unwrap();
    Mock::given(path("/backend-api/redirect"))
        .respond_with(ResponseTemplate::new(307).insert_header("location", format!("{}/steal", elsewhere.uri())))
        .mount(&server)
        .await;
    Mock::given(path("/steal")).respond_with(ResponseTemplate::new(200)).expect(0).mount(&elsewhere).await;
    let mut h = HashMap::new();
    h.insert("x-probe-token".to_string(), "$TOKEN$".to_string());
    let url = format!("{}/backend-api/redirect", server.uri());
    let (status, _, _) = subs.api_call(&a, "POST", &url, &h, Some("token=$TOKEN$")).await.unwrap();
    assert_eq!(status, 307, "the redirect is returned, not followed");
    assert!(elsewhere.received_requests().await.unwrap().is_empty());
}

// Live Codex streams can arrive without a Content-Type.

#[tokio::test]
async fn codex_headerless_stream_is_accounted() {
    let server = MockServer::start().await;
    let (subs, _) = setup(&server, Strategy::FillFirst).await;
    let a = subs.save_account(codex_cred("acct-a", "tok-a")).await.unwrap();
    Mock::given(path("/backend-api/codex/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(SSE_OK.as_bytes()))
        .mount(&server)
        .await;
    let resp = subs
        .execute(SubProvider::Codex, Method::POST, "/v1/responses", HeaderMap::new(), responses_body(), None, None, &mut Charge::default())
        .await;
    assert_eq!(resp.headers()["content-type"], "text/event-stream");
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    assert!(String::from_utf8_lossy(&body).contains("response.completed"));
    // The request log is written in the background.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let s = subs.db.usage_summary("account", 0, &Default::default()).await.unwrap();
    let row = s.iter().find(|r| r.key.as_deref() == Some(a.id.as_str())).unwrap();
    assert_eq!((row.input_tokens, row.output_tokens), (5, 1));
}

#[tokio::test]
async fn codex_headerless_stream_quota_error_fails_over() {
    let server = MockServer::start().await;
    let (subs, _) = setup(&server, Strategy::FillFirst).await;
    let a = subs.save_account(codex_cred("acct-a", "tok-a")).await.unwrap();
    subs.save_account(codex_cred("acct-b", "tok-b")).await.unwrap();
    subs.db.set_account_priority(&a.id, 10).await.unwrap();
    let in_stream_error =
        "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"usage_limit_reached\",\"resets_in_seconds\":120}}\n\n";
    Mock::given(header("authorization", "Bearer tok-a"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(in_stream_error.as_bytes()))
        .mount(&server)
        .await;
    Mock::given(header("authorization", "Bearer tok-b"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(SSE_OK.as_bytes()))
        .mount(&server)
        .await;
    let (status, body) = run(&subs, SubProvider::Codex, "/v1/responses", HeaderMap::new(), responses_body()).await;
    assert_eq!(status, 200);
    assert!(body.contains("\"delta\":\"hi\""), "{body}");
    assert!(subs.db.get_account(&a.id).await.unwrap().unwrap().cooling_down(now()));
}

// ------------------------------------------------------------------ grok

fn grok_cred(sub: &str, token: &str) -> Credential {
    Credential {
        provider: SubProvider::Grok,
        access_token: token.into(),
        refresh_token: Some(format!("rt-{sub}")),
        expires_at: Some(now() + 3600),
        account_id: Some(sub.into()),
        email: Some(format!("{sub}@example.com")),
        ..Default::default()
    }
}

fn grok_id_token(sub: &str, email: &str) -> String {
    let claims = json!({"sub": sub, "email": email}).to_string();
    format!("h.{}.s", base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, claims))
}

async fn mount_grok_discovery(server: &MockServer) {
    let uri = server.uri();
    Mock::given(method("GET"))
        .and(path("/.well-known/openid-configuration"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "issuer": uri,
            "device_authorization_endpoint": format!("{uri}/oauth2/device/code"),
            "token_endpoint": format!("{uri}/oauth2/token"),
        })))
        .mount(server)
        .await;
}

fn grok_body() -> Bytes {
    Bytes::from(json!({"model": "grok-build", "input": [], "stream": true, "include": ["reasoning.encrypted_content"]}).to_string())
}

const GROK_SSE: &str = "event: response.created\ndata: {\"type\":\"response.created\"}\n\nevent: keepalive\ndata: {\"type\":\"keepalive\",\"ts\":1}\n\nevent: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\ndata: {\"type\":\"keepalive\"}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":7,\"output_tokens\":2}}}\n\n";

#[tokio::test]
async fn grok_device_login_saves_account_by_subject() {
    let server = MockServer::start().await;
    let (subs, secrets) = setup(&server, Strategy::FillFirst).await;
    mount_grok_discovery(&server).await;
    Mock::given(method("POST"))
        .and(path("/oauth2/device/code"))
        .and(wiremock::matchers::body_string_contains("client_id=b1a00492-073a-47ea-816f-4c329264a828"))
        .and(wiremock::matchers::body_string_contains("grok-cli%3Aaccess"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_code": "dev-code", "user_code": "WXYZ-1234",
            "verification_uri": "https://accounts.x.ai/device",
            "verification_uri_complete": "https://accounts.x.ai/device?user_code=WXYZ-1234",
            "expires_in": 600, "interval": 1,
        })))
        .mount(&server)
        .await;
    Mock::given(path("/oauth2/token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": "authorization_pending"})))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(path("/oauth2/token"))
        .and(wiremock::matchers::body_string_contains("device_code=dev-code"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "xai-at", "refresh_token": "xai-rt", "expires_in": 3600,
            "id_token": grok_id_token("user-123", "G@X.ai"),
        })))
        .mount(&server)
        .await;

    let start = subs.start_login(SubProvider::Grok, true).await.unwrap();
    assert_eq!(start.user_code.as_deref(), Some("WXYZ-1234"));
    assert_eq!(start.url, "https://accounts.x.ai/device?user_code=WXYZ-1234");
    assert!(!start.needs_code);
    for _ in 0..50 {
        if let Some(FlowStatus::Complete { account_id, email }) = subs.flow_status(&start.flow_id) {
            assert_eq!(email.as_deref(), Some("g@x.ai"));
            let stored: Credential = get_typed(&*secrets, &format!("accounts/grok/{account_id}")).await.unwrap().unwrap();
            assert_eq!(stored.account_id.as_deref(), Some("user-123"));
            assert_eq!(stored.refresh_token.as_deref(), Some("xai-rt"));
            // Same subject, same account id.
            let again = subs.save_account(grok_cred("user-123", "other")).await.unwrap();
            assert_eq!(again.id, account_id);
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("device flow did not complete: {:?}", subs.flow_status(&start.flow_id));
}

#[tokio::test]
async fn grok_device_interval_is_never_shortened() {
    let server = MockServer::start().await;
    let (_, _) = setup(&server, Strategy::FillFirst).await;
    mount_grok_discovery(&server).await;
    Mock::given(path("/oauth2/device/code"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_code": "d", "user_code": "u", "verification_uri": "https://accounts.x.ai/device", "interval": 90,
        })))
        .mount(&server)
        .await;
    let ep = Endpoints { grok_auth: server.uri(), ..Default::default() };
    let http = crate::proxy::upstream_client(std::time::Duration::from_secs(30)).unwrap();
    let dc = grok::start_device(&http, &ep).await.unwrap();
    assert_eq!(dc.interval, 90);
    assert_eq!(grok::next_interval(dc.interval, &grok::DevicePoll::SlowDown), 95);
}

#[tokio::test]
async fn grok_streams_with_identity_headers_and_rewrites_keepalives() {
    let server = MockServer::start().await;
    let (subs, _) = setup(&server, Strategy::FillFirst).await;
    let a = subs.save_account(grok_cred("user-a", "xai-tok")).await.unwrap();
    Mock::given(method("POST"))
        .and(path("/grok/v1/responses"))
        .and(header("authorization", "Bearer xai-tok"))
        .and(header("x-xai-token-auth", "xai-grok-cli"))
        .and(header("x-grok-client-identifier", "grok-shell"))
        .and(header("x-authenticateresponse", "authenticate-response"))
        .and(header("x-grok-client-version", "1.0.60"))
        .and(header("x-grok-conv-id", "conv-1"))
        .and(body_partial_json(json!({"model": "grok-build", "stream": true})))
        .respond_with(ResponseTemplate::new(200).set_body_raw(GROK_SSE, "text/event-stream"))
        .expect(1)
        .mount(&server)
        .await;
    let mut h = HeaderMap::new();
    h.insert("user-agent", "grok-shell/1.0.60 (linux; x86_64)".parse().unwrap());
    h.insert("x-grok-client-version", "1.0.60".parse().unwrap());
    h.insert("x-grok-conv-id", "conv-1".parse().unwrap());
    let (status, body) = run(&subs, SubProvider::Grok, "/v1/responses", h, grok_body()).await;
    assert_eq!(status, 200);
    assert!(body.contains("\"delta\":\"hi\""), "{body}");
    assert!(!body.contains("\"keepalive\"") && !body.contains("event: keepalive"), "{body}");
    assert_eq!(body.matches(": keepalive\n\n").count(), 2, "{body}");

    // Keepalives don't disturb usage accounting.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let s = subs.db.usage_summary("account", 0, &Default::default()).await.unwrap();
    let row = s.iter().find(|r| r.key.as_deref() == Some(a.id.as_str())).unwrap();
    assert_eq!((row.input_tokens, row.output_tokens), (7, 2));
}

#[tokio::test]
async fn grok_refreshes_on_401_and_retries() {
    let server = MockServer::start().await;
    let (subs, secrets) = setup(&server, Strategy::FillFirst).await;
    let a = subs.save_account(grok_cred("user-a", "stale")).await.unwrap();
    mount_grok_discovery(&server).await;
    Mock::given(path("/grok/v1/responses"))
        .and(header("authorization", "Bearer stale"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({"error": "unauthorized"})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/oauth2/token"))
        .and(wiremock::matchers::body_string_contains("grant_type=refresh_token"))
        .and(wiremock::matchers::body_string_contains("refresh_token=rt-user-a"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"access_token": "fresh", "refresh_token": "rt-new", "expires_in": 3600})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path("/grok/v1/responses"))
        .and(header("authorization", "Bearer fresh"))
        .and(header("x-grok-client-version", "1.0.52"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(GROK_SSE, "text/event-stream"))
        .expect(1)
        .mount(&server)
        .await;
    let (status, _) = run(&subs, SubProvider::Grok, "/v1/responses", HeaderMap::new(), grok_body()).await;
    assert_eq!(status, 200);
    let stored: Credential = get_typed(&*secrets, &format!("accounts/grok/{}", a.id)).await.unwrap().unwrap();
    assert_eq!(stored.access_token, "fresh");
    assert_eq!(stored.refresh_token.as_deref(), Some("rt-new"));
    assert_eq!(stored.account_id.as_deref(), Some("user-a"), "identity kept across refresh");
}

#[tokio::test]
async fn grok_refreshes_before_expiry_and_dead_grant_needs_relogin() {
    let server = MockServer::start().await;
    let (subs, _) = setup(&server, Strategy::FillFirst).await;
    mount_grok_discovery(&server).await;
    let mut cred = grok_cred("user-a", "old");
    cred.expires_at = Some(now() + 10);
    let a = subs.save_account(cred).await.unwrap();
    Mock::given(path("/oauth2/token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": "invalid_grant"})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path("/grok/v1/responses")).respond_with(ResponseTemplate::new(500)).expect(0).mount(&server).await;
    let (status, _) = run(&subs, SubProvider::Grok, "/v1/responses", HeaderMap::new(), grok_body()).await;
    assert_eq!(status, 503);
    assert!(subs.db.get_account(&a.id).await.unwrap().unwrap().auth_failed);
}

#[tokio::test]
async fn grok_426_names_the_client_version_without_failover() {
    let server = MockServer::start().await;
    let (subs, _) = setup(&server, Strategy::FillFirst).await;
    subs.save_account(grok_cred("user-a", "tok-a")).await.unwrap();
    subs.save_account(grok_cred("user-b", "tok-b")).await.unwrap();
    Mock::given(path("/grok/v1/responses"))
        .respond_with(ResponseTemplate::new(426).set_body_json(json!({"error": "upgrade required"})))
        .expect(1)
        .mount(&server)
        .await;
    let (status, body) = run(&subs, SubProvider::Grok, "/v1/responses", HeaderMap::new(), grok_body()).await;
    assert_eq!(status, 426);
    assert!(body.contains("1.0.52") && body.contains("catalog.grok_client_version"), "{body}");
}

#[tokio::test]
async fn grok_free_usage_exhaustion_cools_down_and_fails_over() {
    let server = MockServer::start().await;
    let (subs, _) = setup(&server, Strategy::FillFirst).await;
    let a = subs.save_account(grok_cred("user-a", "tok-a")).await.unwrap();
    subs.save_account(grok_cred("user-b", "tok-b")).await.unwrap();
    subs.db.set_account_priority(&a.id, 10).await.unwrap();
    Mock::given(header("authorization", "Bearer tok-a"))
        .respond_with(ResponseTemplate::new(429).set_body_json(json!({"code": "subscription:free-usage-exhausted", "error": "used up"})))
        .mount(&server)
        .await;
    Mock::given(header("authorization", "Bearer tok-b"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(GROK_SSE, "text/event-stream"))
        .mount(&server)
        .await;
    let (status, _) = run(&subs, SubProvider::Grok, "/v1/responses", HeaderMap::new(), grok_body()).await;
    assert_eq!(status, 200);
    assert!(subs.db.get_account(&a.id).await.unwrap().unwrap().cooldown_until.unwrap() > now() + 23 * 3600);
}

#[tokio::test]
async fn grok_discovers_models_from_the_proxy() {
    let server = MockServer::start().await;
    let (subs, _) = setup(&server, Strategy::FillFirst).await;
    subs.save_account(grok_cred("user-a", "tok-a")).await.unwrap();
    Mock::given(method("GET"))
        .and(path("/grok/v1/models"))
        .and(header("authorization", "Bearer tok-a"))
        .and(header("x-grok-client-version", "1.0.52"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [{"id": "grok-build"}, {"id": "grok-4.7"}]})))
        .mount(&server)
        .await;
    assert_eq!(subs.fetch_models(SubProvider::Grok, "unused").await.unwrap(), vec!["grok-build", "grok-4.7"]);
}

#[tokio::test]
async fn opencode_errors_preserve_reason_without_disabling_on_403() {
    for status in [400, 401, 403, 429, 503] {
        let server = MockServer::start().await;
        let (subs, _) = setup(&server, Strategy::FillFirst).await;
        let a = subs
            .save_account(Credential { provider: SubProvider::OpencodeGo, api_key: Some("test-upstream-key".into()), ..Default::default() })
            .await
            .unwrap();
        if status == 403 {
            subs.db.set_account_priority(&a.id, 10).await.unwrap();
            subs.save_account(Credential {
                provider: SubProvider::OpencodeGo,
                api_key: Some("second-test-key".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        }
        let error = json!({"error": {"code": "RequestRejected", "message": "request refused for test-upstream-key"}});
        Mock::given(path("/zen/go/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(status).set_body_json(&error))
            .expect(1)
            .mount(&server)
            .await;
        let (actual, body) = run(&subs, SubProvider::OpencodeGo, "/v1/chat/completions", HeaderMap::new(), Bytes::from_static(b"{}")).await;
        assert_eq!(actual, status);
        assert_eq!(server.received_requests().await.unwrap().len(), 1, "request failure must not try another account");
        assert_eq!(serde_json::from_str::<Value>(&body).unwrap(), error);
        let row = subs.db.get_account(&a.id).await.unwrap().unwrap();
        assert_eq!(row.auth_failed, status == 401);
        let reason = row.last_error.unwrap();
        assert!(reason.contains(&format!("upstream status {status}: RequestRejected")), "{reason}");
        assert!(reason.contains("[redacted]"));
        assert!(!reason.contains("test-upstream-key"));
        if status == 403 {
            assert!(row.cooldown_until.is_none());
            server.reset().await;
            Mock::given(path("/zen/go/v1/chat/completions"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
                .expect(1)
                .mount(&server)
                .await;
            assert_eq!(run(&subs, SubProvider::OpencodeGo, "/v1/chat/completions", HeaderMap::new(), Bytes::new()).await.0, 200);
            assert!(subs.db.get_account(&a.id).await.unwrap().unwrap().last_error.is_none());
        }
    }
}

#[tokio::test]
async fn claude_import_without_expiry_refreshes_before_use() {
    let server = MockServer::start().await;
    let (subs, secrets) = setup(&server, Strategy::FillFirst).await;
    let cred = claude::import(&json!({"claudeAiOauth": {"accessToken": "old", "refreshToken": "rt"}})).unwrap();
    assert_eq!(cred.expires_at, None);
    let row = subs.save_account(cred).await.unwrap();
    Mock::given(method("POST"))
        .and(path("/v1/oauth/token"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"access_token": "fresh", "refresh_token": "rotated", "expires_in": 3600})),
        )
        .expect(1)
        .mount(&server)
        .await;
    assert_eq!(subs.credential(&row.id, None).await.unwrap().access_token, "fresh");
    assert_eq!(subs.credential(&row.id, None).await.unwrap().access_token, "fresh");
    let stored: Credential = get_typed(&*secrets, &format!("accounts/claude/{}", row.id)).await.unwrap().unwrap();
    assert_eq!(stored.refresh_token.as_deref(), Some("rotated"));
    let known = claude::import(&json!({"accessToken": "at", "refreshToken": "rt", "expiresAt": 2000000000000i64})).unwrap();
    assert_eq!(known.expires_at, Some(2000000000));
    let known = subs.save_account(known).await.unwrap();
    assert_eq!(subs.credential(&known.id, None).await.unwrap().access_token, "at");
    let access_only = claude::import(&json!({"accessToken": "at"})).unwrap();
    assert_eq!(access_only.expires_at, None);
    let access_only = subs.save_account(access_only).await.unwrap();
    assert_eq!(subs.credential(&access_only.id, None).await.unwrap().access_token, "at");
}

#[tokio::test]
async fn uncertain_refresh_is_visible_and_backs_off_without_relogin() {
    let server = MockServer::start().await;
    let (subs, _) = setup(&server, Strategy::FillFirst).await;
    let mut cred = codex_cred("acct-a", "old");
    cred.expires_at = Some(now() - 1);
    let row = subs.save_account(cred).await.unwrap();
    Mock::given(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_string("unusable-response-with-secret"))
        .expect(2)
        .mount(&server)
        .await;
    assert!(subs.credential(&row.id, Some("old")).await.is_err());
    let state = subs.db.get_account(&row.id).await.unwrap().unwrap();
    assert!(!state.auth_failed);
    let error = state.last_error.unwrap();
    assert!(error.contains("outcome unknown"));
    assert!(error.contains("refresh retry in 5s"));
    assert!(!error.contains("unusable-response-with-secret"));
    for _ in 0..3 {
        assert!(subs.credential(&row.id, Some("old")).await.unwrap_err().to_string().contains("retry pending"));
    }
    subs.refresh_retries.lock().get_mut(&row.id).unwrap().after = Instant::now();
    assert!(subs.credential(&row.id, Some("old")).await.is_err());
    assert!(subs.db.get_account(&row.id).await.unwrap().unwrap().last_error.unwrap().contains("refresh retry in 10s"));
    // A new login of the same identity must not inherit the previous backoff.
    subs.save_account(codex_cred("acct-a", "new-login")).await.unwrap();
    assert!(!subs.refresh_retries.lock().contains_key(&row.id));
}

#[tokio::test]
async fn transient_refresh_keeps_valid_token_but_never_reuses_a_rejected_one() {
    let server = MockServer::start().await;
    let (subs, _) = setup(&server, Strategy::FillFirst).await;
    let mut cred = codex_cred("acct-a", "old");
    cred.expires_at = Some(now() + 120);
    let row = subs.save_account(cred).await.unwrap();
    Mock::given(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(503).set_body_json(json!({"error_description": "secret-value"})))
        .expect(2)
        .mount(&server)
        .await;
    assert_eq!(subs.credential(&row.id, None).await.unwrap().access_token, "old");
    assert_eq!(subs.credential(&row.id, None).await.unwrap().access_token, "old");
    assert!(subs.credential(&row.id, Some("old")).await.is_err());
    subs.refresh_retries.lock().get_mut(&row.id).unwrap().after = Instant::now();
    assert!(subs.credential(&row.id, Some("old")).await.is_err());
    let row = subs.db.get_account(&row.id).await.unwrap().unwrap();
    assert!(!row.auth_failed);
    assert!(!row.last_error.unwrap().contains("secret-value"));
}

#[tokio::test]
async fn refresh_transport_distinguishes_unsent_from_lost_response() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let http = reqwest::Client::builder().no_proxy().retry(reqwest::retry::never()).build().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let error = http.post(format!("http://{addr}/token")).send().await.unwrap_err();
    assert!(matches!(RefreshError::from_send_error(error), RefreshError::Transient(m) if m.contains("not sent")));

    // Accept the token request and drop the response, as if the provider rotated it.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        assert!(socket.read(&mut request).await.unwrap() > 0);
        socket.shutdown().await.unwrap();
    });
    let error = http.post(format!("http://{addr}/token")).send().await.unwrap_err();
    assert!(matches!(RefreshError::from_send_error(error), RefreshError::Uncertain(_)));
    task.await.unwrap();

    // A truncated successful response is also uncertain, even after headers arrived.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        assert!(socket.read(&mut request).await.unwrap() > 0);
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\nConnection: close\r\n\r\n{}").await.unwrap();
        socket.shutdown().await.unwrap();
    });
    let response = http.post(format!("http://{addr}/token")).send().await.unwrap();
    assert!(matches!(RefreshError::from_response(response).await, Err(RefreshError::Uncertain(_))));
    task.await.unwrap();
}

#[tokio::test]
async fn rotated_token_is_saved_even_when_sqlite_index_fails() {
    let server = MockServer::start().await;
    let (subs, secrets) = setup(&server, Strategy::FillFirst).await;
    let mut cred = codex_cred("acct-a", "old");
    cred.expires_at = Some(now() - 1);
    let row = subs.save_account(cred).await.unwrap();
    Mock::given(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token": "fresh", "refresh_token": "rotated"})))
        .expect(1)
        .mount(&server)
        .await;
    {
        let mut connection = subs.db.hold().await;
        sqlx::query("DROP TABLE accounts").execute(&mut *connection).await.unwrap();
    }
    assert_eq!(subs.credential(&row.id, None).await.unwrap().access_token, "fresh");
    let stored: Credential = get_typed(&*secrets, &format!("accounts/codex/{}", row.id)).await.unwrap().unwrap();
    assert_eq!(stored.refresh_token.as_deref(), Some("rotated"));
}

#[tokio::test]
async fn refresh_backoff_caps_and_success_clears_it() {
    let server = MockServer::start().await;
    let (subs, _) = setup(&server, Strategy::FillFirst).await;
    let mut cred = codex_cred("acct-a", "old");
    cred.expires_at = Some(now() - 1);
    let row = subs.save_account(cred).await.unwrap();
    Mock::given(path("/oauth/token")).respond_with(ResponseTemplate::new(503)).expect(8).mount(&server).await;
    for delay in [5, 10, 20, 40, 80, 160, 300, 300] {
        assert!(subs.credential(&row.id, Some("old")).await.is_err());
        assert!(subs.db.get_account(&row.id).await.unwrap().unwrap().last_error.unwrap().contains(&format!("retry in {delay}s")));
        subs.refresh_retries.lock().get_mut(&row.id).unwrap().after = Instant::now();
    }
    server.verify().await;
    server.reset().await;
    Mock::given(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token": "fresh", "refresh_token": "rotated"})))
        .expect(1)
        .mount(&server)
        .await;
    assert_eq!(subs.credential(&row.id, None).await.unwrap().access_token, "fresh");
    assert!(!subs.refresh_retries.lock().contains_key(&row.id));
    assert!(subs.db.get_account(&row.id).await.unwrap().unwrap().last_error.is_none());
}

#[tokio::test]
async fn empty_successful_token_is_an_uncertain_refresh() {
    let server = MockServer::start().await;
    Mock::given(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token": "", "refresh_token": "secret-value"})))
        .mount(&server)
        .await;
    let response = reqwest::Client::new().post(format!("{}/token", server.uri())).send().await.unwrap();
    let error = RefreshError::from_response(response).await.unwrap_err();
    assert!(matches!(error, RefreshError::Uncertain(_)));
    assert!(!error.to_string().contains("secret-value"));
}

#[tokio::test]
async fn claude_unknown_expiry_keeps_serving_during_refresh_outage() {
    let server = MockServer::start().await;
    let (subs, _) = setup(&server, Strategy::FillFirst).await;
    let cred = claude::import(&json!({"accessToken": "old", "refreshToken": "rt"})).unwrap();
    let row = subs.save_account(cred).await.unwrap();
    Mock::given(path("/v1/oauth/token")).respond_with(ResponseTemplate::new(503)).expect(1).mount(&server).await;
    assert_eq!(subs.credential(&row.id, None).await.unwrap().access_token, "old");
    assert_eq!(subs.credential(&row.id, None).await.unwrap().access_token, "old");
    assert!(subs.credential(&row.id, Some("old")).await.is_err());
    assert!(!subs.db.get_account(&row.id).await.unwrap().unwrap().auth_failed);
}

#[tokio::test]
async fn unknown_refresh_outcome_survives_proxy_success_and_failure() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let server = MockServer::start().await;
    let (mut subs, _) = setup(&server, Strategy::FillFirst).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    Arc::get_mut(&mut subs).unwrap().cfg.endpoints.codex_auth = format!("http://{addr}");
    Arc::get_mut(&mut subs).unwrap().http = reqwest::Client::builder().no_proxy().retry(reqwest::retry::never()).build().unwrap();
    let dropped_response = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        assert!(socket.read(&mut request).await.unwrap() > 0);
        socket.shutdown().await.unwrap();
    });
    let mut cred = codex_cred("acct-a", "old");
    cred.expires_at = Some(now() + 120);
    let row = subs.save_account(cred).await.unwrap();
    Mock::given(path("/backend-api/codex/responses"))
        .and(header("authorization", "Bearer old"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(SSE_OK, "text/event-stream"))
        .expect(1)
        .mount(&server)
        .await;
    assert_eq!(run(&subs, SubProvider::Codex, "/v1/responses", HeaderMap::new(), responses_body()).await.0, 200);
    dropped_response.await.unwrap();
    let diagnostic = subs.db.get_account(&row.id).await.unwrap().unwrap().last_error.unwrap();
    assert!(diagnostic.contains("outcome unknown"), "{diagnostic}");
    assert_eq!(subs.accounts(None).await.unwrap()[0].row.last_error.as_deref(), Some(diagnostic.as_str()));
    server.verify().await;
    server.reset().await;
    Mock::given(path("/backend-api/codex/responses")).respond_with(ResponseTemplate::new(503)).expect(1).mount(&server).await;
    assert_eq!(run(&subs, SubProvider::Codex, "/v1/responses", HeaderMap::new(), responses_body()).await.0, 503);
    assert_eq!(subs.db.get_account(&row.id).await.unwrap().unwrap().last_error.as_deref(), Some(diagnostic.as_str()));
    assert!(subs.credential(&row.id, Some("old")).await.is_err());
    Arc::get_mut(&mut subs).unwrap().cfg.endpoints.codex_auth = server.uri();
    subs.refresh_retries.lock().get_mut(&row.id).unwrap().after = Instant::now();
    Mock::given(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token": "fresh", "refresh_token": "rotated"})))
        .expect(1)
        .mount(&server)
        .await;
    assert_eq!(subs.credential(&row.id, None).await.unwrap().access_token, "fresh");
    assert!(subs.db.get_account(&row.id).await.unwrap().unwrap().last_error.is_none());
}

#[tokio::test]
async fn codex_reset_credits_are_filtered_persisted_and_survive_headers() {
    let server = MockServer::start().await;
    let (subs, _) = setup(&server, Strategy::FillFirst).await;
    let a = subs.save_account(codex_cred("acct-a", "tok-a")).await.unwrap();
    Mock::given(path("/backend-api/wham/usage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"plan_type": "pro", "rate_limit": {
            "primary_window": {"used_percent": 25, "limit_window_seconds": 18000}}})))
        .mount(&server)
        .await;
    let credit = |id: &str, status: &str, reset_type: &str, expires_at: &str| {
        json!({
            "id": id, "status": status, "reset_type": reset_type, "expires_at": expires_at
        })
    };
    let credits = json!({"credits": [
        credit("first", "available", "codex_rate_limits", "2099-01-01T00:00:00Z"),
        json!({"id": "second", "status": "available", "reset_type": "codex_rate_limits", "granted_at": "2026-06-18T00:00:00Z", "expires_at": null}),
        credit("old", "available", "codex_rate_limits", "2000-01-01T00:00:00Z"),
        credit("used", "redeemed", "codex_rate_limits", "2099-01-01T00:00:00Z"),
        credit("other", "available", "other", "2099-01-01T00:00:00Z")
    ]});
    Mock::given(method("GET"))
        .and(path("/backend-api/wham/rate-limit-reset-credits"))
        .and(header("authorization", "Bearer tok-a"))
        .and(header("chatgpt-account-id", "acct-a"))
        .and(header("openai-beta", "codex-1"))
        .and(header("originator", "Codex Desktop"))
        .respond_with(ResponseTemplate::new(200).set_body_json(credits))
        .expect(1)
        .mount(&server)
        .await;
    let usage = subs.poll_usage(&a.id).await.unwrap();
    assert_eq!(usage.raw.as_ref().unwrap()["reset_credits"], 2);
    assert_eq!(usage.windows[0].used_percent, 25.0);
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("x-codex-primary-used-percent", "40".parse().unwrap());
    headers.insert("x-codex-primary-window-minutes", "300".parse().unwrap());
    subs.observe_headers(&a.id, SubProvider::Codex, &headers).await;
    let accounts = subs.accounts(Some(SubProvider::Codex)).await.unwrap();
    let serialized = serde_json::to_value(&accounts).unwrap();
    assert_eq!(serialized[0]["usage"]["raw"]["reset_credits"], 2);
    assert_eq!(serialized[0]["usage"]["windows"][0]["used_percent"], 40.0);

    server.reset().await;
    Mock::given(path("/backend-api/wham/usage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"rate_limit": {
            "primary_window": {"used_percent": 50, "limit_window_seconds": 18000}}})))
        .mount(&server)
        .await;
    // A failed credit lookup replaces the previous count with unknown, not zero.
    for (response, expected) in [
        (ResponseTemplate::new(503).set_body_json(json!({"credits": []})), serde_json::Value::Null),
        (ResponseTemplate::new(200).set_body_json(json!({"unexpected": []})), serde_json::Value::Null),
        (ResponseTemplate::new(200).set_body_json(json!({"credits": []})), json!(0)),
    ] {
        let guard = Mock::given(path("/backend-api/wham/rate-limit-reset-credits")).respond_with(response).mount_as_scoped(&server).await;
        let usage = subs.poll_usage(&a.id).await.unwrap();
        assert_eq!(usage.windows[0].used_percent, 50.0);
        assert_eq!(usage.raw.unwrap()["reset_credits"], expected);
        drop(guard);
    }
}

#[tokio::test]
async fn codex_reset_credits_refresh_on_401() {
    let server = MockServer::start().await;
    let (subs, _) = setup(&server, Strategy::FillFirst).await;
    let a = subs.save_account(codex_cred("acct-a", "stale")).await.unwrap();
    Mock::given(path("/backend-api/wham/usage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"rate_limit": {}})))
        .mount(&server)
        .await;
    Mock::given(path("/backend-api/wham/rate-limit-reset-credits"))
        .and(header("authorization", "Bearer stale"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path("/oauth/token"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"access_token": "fresh", "refresh_token": "new", "expires_in": 86400})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path("/backend-api/wham/rate-limit-reset-credits"))
        .and(header("authorization", "Bearer fresh"))
        .and(header("chatgpt-account-id", "acct-a"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"credits": []})))
        .expect(1)
        .mount(&server)
        .await;
    assert_eq!(subs.poll_usage(&a.id).await.unwrap().raw.unwrap()["reset_credits"], 0);
}

#[tokio::test]
async fn codex_reset_credits_reject_non_object_usage_without_panicking() {
    let server = MockServer::start().await;
    let (subs, _) = setup(&server, Strategy::FillFirst).await;
    let a = subs.save_account(codex_cred("acct-a", "tok-a")).await.unwrap();
    Mock::given(path("/backend-api/wham/usage")).respond_with(ResponseTemplate::new(200).set_body_json(json!([]))).mount(&server).await;
    assert!(subs.poll_usage(&a.id).await.is_err());
    assert!(subs.db.get_usage(&a.id).await.unwrap().is_none());
}

/// One Codex mock per account token, each answering 200.
async fn mock_codex_ok(server: &MockServer, tokens: &[&str]) {
    for t in tokens {
        Mock::given(method("POST"))
            .and(path("/backend-api/codex/responses"))
            .and(header("authorization", format!("Bearer {t}").as_str()))
            .respond_with(ResponseTemplate::new(200).insert_header("x-served-by", *t).set_body_raw(SSE_OK, "text/event-stream"))
            .mount(server)
            .await;
    }
}

async fn served_by(subs: &Subscriptions, principal: &Principal) -> (u16, Option<String>, Option<String>) {
    let resp = subs
        .execute(
            SubProvider::Codex,
            Method::POST,
            "/v1/responses",
            HeaderMap::new(),
            responses_body(),
            Some(principal),
            None,
            &mut Charge::default(),
        )
        .await;
    let header = |name: &str| resp.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_string);
    (resp.status().as_u16(), header("x-served-by"), header("retry-after"))
}

#[tokio::test]
async fn policy_accounts_choose_the_pool_accounts() {
    let server = MockServer::start().await;
    let (subs, _) = setup(&server, Strategy::FillFirst).await;
    let a = subs.save_account(codex_cred("acct-a", "tok-a")).await.unwrap();
    let b = subs.save_account(codex_cred("acct-b", "tok-b")).await.unwrap();
    subs.db.set_account_priority(&a.id, 10).await.unwrap();
    mock_codex_ok(&server, &["tok-a", "tok-b"]).await;

    let (status, by, _) = served_by(&subs, &principal(any_policy())).await;
    assert_eq!((status, by.as_deref()), (200, Some("tok-a")), "* takes every account");

    let mut p = any_policy();
    p.accounts.insert("codex".into(), vec![b.id.clone()]);
    let (status, by, _) = served_by(&subs, &principal(p)).await;
    assert_eq!((status, by.as_deref()), (200, Some("tok-b")), "only the listed account serves");

    let mut p = any_policy();
    p.accounts.insert("codex".into(), vec!["codex-gone".into()]);
    let (status, ..) = served_by(&subs, &principal(p)).await;
    assert_eq!(status, 403, "the policy uses no account that exists");
    let mut p = any_policy();
    p.accounts.remove("codex");
    let (status, ..) = served_by(&subs, &principal(p)).await;
    assert_eq!(status, 403, "no entry: no account");
}

#[tokio::test]
async fn account_limits_move_requests_to_other_accounts() {
    let server = MockServer::start().await;
    let (subs, _) = setup(&server, Strategy::FillFirst).await;
    let limiter = Arc::new(crate::limits::Limiter::default());
    let subs = Arc::try_unwrap(subs).ok().unwrap().with_limiter(limiter.clone());
    let a = subs.save_account(codex_cred("acct-a", "tok-a")).await.unwrap();
    let b = subs.save_account(codex_cred("acct-b", "tok-b")).await.unwrap();
    subs.db.set_account_priority(&a.id, 10).await.unwrap();
    mock_codex_ok(&server, &["tok-a", "tok-b"]).await;

    let one = crate::limits::Allowance { requests_per_minute: Some(1), ..Default::default() };
    let mut p = any_policy();
    p.limits.accounts.insert(a.id.clone(), one.clone());
    let key = principal(p.clone());
    assert_eq!(served_by(&subs, &key).await.1.as_deref(), Some("tok-a"));
    assert_eq!(served_by(&subs, &key).await.1.as_deref(), Some("tok-b"), "a is at its limit");
    let c = limiter.policy_consumption("p", crate::limits::Meter::Account(&a.id), now());
    assert_eq!(c.requests_per_minute, 1);

    p.limits.accounts.insert(b.id.clone(), one);
    let key = principal(p);
    let (status, by, retry) = served_by(&subs, &key).await;
    assert_eq!((status, by), (429, None), "every account is at its limit");
    assert!(retry.is_some_and(|r| r.parse::<i64>().unwrap() > 0));
}

#[tokio::test]
async fn an_empty_pool_tells_a_missing_grant_from_missing_accounts() {
    let server = MockServer::start().await;
    let (subs, _) = setup(&server, Strategy::FillFirst).await;
    let mut p = any_policy();
    let (status, ..) = served_by(&subs, &principal(p.clone())).await;
    assert_eq!(status, 503, "* with no accounts yet: log in");
    p.accounts.remove("codex");
    let (status, ..) = served_by(&subs, &principal(p.clone())).await;
    assert_eq!(status, 403, "no entry: no account, also in an empty pool");
    p.accounts.insert("codex".into(), vec!["codex-gone".into()]);
    let (status, ..) = served_by(&subs, &principal(p)).await;
    assert_eq!(status, 403, "only accounts that do not exist");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_requests_past_an_account_limit_get_429() {
    let server = MockServer::start().await;
    let (subs, _) = setup(&server, Strategy::FillFirst).await;
    let limiter = Arc::new(crate::limits::Limiter::default());
    let subs = Arc::new(Arc::try_unwrap(subs).ok().unwrap().with_limiter(limiter.clone()));
    let a = subs.save_account(codex_cred("acct-a", "tok-a")).await.unwrap();
    mock_codex_ok(&server, &["tok-a"]).await;
    let mut p = any_policy();
    p.limits.accounts.insert(a.id.clone(), crate::limits::Allowance { requests_per_minute: Some(1), ..Default::default() });
    let key = Arc::new(principal(p));
    // Released together.
    let start = Arc::new(tokio::sync::Barrier::new(16));
    let tasks: Vec<_> = (0..16)
        .map(|_| {
            let (subs, key, start) = (subs.clone(), key.clone(), start.clone());
            tokio::spawn(async move {
                start.wait().await;
                served_by(&subs, &key).await
            })
        })
        .collect();
    let results: Vec<_> = futures::future::join_all(tasks).await.into_iter().map(Result::unwrap).collect();
    let ok = results.iter().filter(|r| r.0 == 200).count();
    let limited: Vec<_> = results.iter().filter(|r| r.0 == 429).collect();
    assert_eq!((ok, limited.len()), (1, 15), "{results:?}");
    assert!(limited.iter().all(|r| r.2.is_some()), "every refusal says when to retry");
}

#[tokio::test]
async fn an_account_filled_since_its_check_is_passed_over() {
    let server = MockServer::start().await;
    let (subs, _) = setup(&server, Strategy::FillFirst).await;
    let limiter = Arc::new(crate::limits::Limiter::default());
    let subs = Arc::try_unwrap(subs).ok().unwrap().with_limiter(limiter.clone());
    let a = subs.save_account(codex_cred("acct-a", "tok-a")).await.unwrap();
    let b = subs.save_account(codex_cred("acct-b", "tok-b")).await.unwrap();
    subs.db.set_account_priority(&a.id, 10).await.unwrap();
    let mut p = any_policy();
    p.limits.requests_per_day = Some(5);
    p.limits.accounts.insert(a.id.clone(), crate::limits::Allowance { requests_per_minute: Some(1), ..Default::default() });
    let key = principal(p);
    let rows = subs.db.list_accounts(Some("codex")).await.unwrap();
    let candidates = || rows.iter().map(|r| (r.clone(), 0.0)).collect::<Vec<_>>();
    // Another request takes a's last slot after a passed its check.
    limiter.admit("p/client", &key.policy, &[Meter::Account(&a.id)], now()).unwrap();

    let mut charge = Charge::new(&[Meter::Total]);
    let mut limited = None;
    let chosen = subs.admit_one(SubProvider::Codex, None, Some(&key), candidates(), &mut charge, &mut limited).unwrap();
    assert_eq!(chosen.map(|r| r.id), Some(b.id.clone()), "a is passed over");
    assert!(charge.is_settled() && limited.as_ref().is_some_and(|e| e.is_on(Meter::Account(&a.id))));
    assert_eq!(limiter.consumption("p/client", now()).requests_per_day, 1, "counted in total with b");

    let only_a: Vec<_> = candidates().into_iter().filter(|(r, _)| r.id == a.id).collect();
    let mut charge = Charge::new(&[Meter::Total]);
    let mut limited = None;
    let chosen = subs.admit_one(SubProvider::Codex, None, Some(&key), only_a, &mut charge, &mut limited).unwrap();
    assert!(chosen.is_none() && limited.is_some_and(|e| e.retry_after > 0), "the refusal says when to retry");
    assert!(!charge.is_settled());
    assert_eq!(limiter.consumption("p/client", now()).requests_per_day, 1, "nothing counted");
}
