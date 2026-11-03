//! Session identity, context accounting and the context lock at the route.
// ---
// relationships:
//   tests: context
// ---
use std::time::{Duration, Instant};

use super::*;
use crate::context::{Lock, SessionRow, Turn};
use crate::db::UsageWindow;

const SESSION: &str = "x-claude-code-session-id";
const OPUS: &str = "claude/claude-opus-5";
const HAIKU: &str = "claude/claude-haiku-5-5";

/// Two Claude accounts, tokens `tok-a` and `tok-b`. Without affinity and with
/// round robin, only the lock keeps a session on one account.
async fn pool(tweak: impl FnOnce(&mut Config)) -> (Harness, String, String) {
    let h = harness_with(|cfg| {
        cfg.subscription.session_affinity_secs = 0;
        cfg.sessions.lock_tokens = 1000;
        tweak(cfg);
    })
    .await;
    let mut ids = Vec::new();
    for name in ["a", "b"] {
        let cred = Credential {
            provider: SubProvider::Claude,
            access_token: format!("tok-{name}"),
            refresh_token: Some("rt".into()),
            expires_at: Some(now() + 3600),
            account_id: Some(format!("acct-{name}")),
            device_id: Some("dev".into()),
            ..Default::default()
        };
        ids.push(h.account(cred).await);
    }
    let b = ids.pop().unwrap();
    let a = ids.pop().unwrap();
    (h, a, b)
}

async fn messages_mock(h: &Harness, token: &str, response: ResponseTemplate) {
    Mock::given(path("/v1/messages"))
        .and(header("authorization", format!("Bearer {token}").as_str()))
        .respond_with(response)
        .mount(&h.upstream)
        .await;
}

fn answer(input: i64) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({"content": [], "usage": {"input_tokens": input, "output_tokens": 1}}))
}

async fn reset_to(h: &Harness, a: ResponseTemplate, b: ResponseTemplate) {
    h.upstream.reset().await;
    messages_mock(h, "tok-a", a).await;
    messages_mock(h, "tok-b", b).await;
}

async fn send(h: &Harness, key: &str, model: &str, session: &str) -> (u16, Value) {
    let auth = format!("Bearer {key}");
    let body = json!({"model": model, "messages": [{"role": "user", "content": "x"}]});
    h.call_with("POST", "/v1/messages", &[("authorization", auth.as_str()), (SESSION, session)], Some(body)).await
}

/// The bearer token of every Claude request the upstream got, oldest first.
async fn tokens(h: &Harness) -> Vec<String> {
    let requests = h.upstream.received_requests().await.unwrap();
    requests
        .iter()
        .filter(|r| r.url.path() == "/v1/messages")
        .map(|r| r.headers["authorization"].to_str().unwrap().trim_start_matches("Bearer tok-").to_string())
        .collect()
}

/// The client's one session, once it has `turns` turns. Sessions are written
/// after the response finishes.
async fn session_row(h: &Harness, client: &str, turns: i64) -> SessionRow {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let rows = h.state.db.sessions(Some(client), 10).await.unwrap();
        assert!(rows.len() <= 1, "{rows:?}");
        if let Some(row) = rows.into_iter().next().filter(|r| r.turns >= turns) {
            return row;
        }
        assert!(Instant::now() < deadline, "no session with {turns} turns");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// A session of the key that holds a lock, as if earlier requests formed it.
async fn locked(h: &Harness, client: &str, session: &str, lock: &Lock) {
    let headers = {
        let mut m = HeaderMap::new();
        m.insert(SESSION, session.parse().unwrap());
        m
    };
    let id = crate::context::resolve(&headers, &json!({})).unwrap();
    let turn = Turn {
        client_id: client.into(),
        session_key: id.key.clone(),
        source: id.source.into(),
        tokens: 5000,
        reported: true,
        provider: lock.provider.clone(),
        model: lock.model.clone(),
        ..Default::default()
    };
    h.state.db.record_turn(&turn).await.unwrap();
    assert!(h.state.db.form_lock(client, &id.key, lock).await.unwrap());
}

fn opus_on(account: &str) -> Lock {
    Lock { name: OPUS.into(), provider: "claude".into(), model: "claude-opus-5".into(), account: Some(account.into()) }
}

#[tokio::test]
async fn a_lock_forms_only_after_a_success_past_the_threshold() {
    let (h, a, b) = pool(|_| {}).await;
    let key = h.key(policy("p")).await;
    reset_to(&h, answer(999), answer(999)).await;
    assert_eq!(send(&h, &key, OPUS, "s").await.0, 200);
    let row = session_row(&h, "p/client", 1).await;
    assert_eq!((row.current_tokens, row.lock_account.clone()), (999, None), "below the threshold");

    // A failed request forms nothing, whatever its size.
    let refused = || {
        ResponseTemplate::new(400)
            .set_body_json(json!({"error": {"type": "invalid_request_error", "message": "no"}, "usage": {"input_tokens": 5000}}))
    };
    reset_to(&h, refused(), refused()).await;
    assert_eq!(send(&h, &key, OPUS, "s").await.0, 400);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let row = session_row(&h, "p/client", 1).await;
    assert_eq!((row.lock_account, row.current_tokens), (None, 999));

    reset_to(&h, answer(1000), answer(1000)).await;
    assert_eq!(send(&h, &key, OPUS, "s").await.0, 200);
    let row = session_row(&h, "p/client", 2).await;
    let served = if tokens(&h).await == ["a"] { &a } else { &b };
    assert_eq!(row.lock_account.as_ref(), Some(served));
    assert_eq!(
        (row.lock_name.as_deref(), row.lock_provider.as_deref(), row.lock_model.as_deref()),
        (Some(OPUS), Some("claude"), Some("claude-opus-5"))
    );
}

#[tokio::test]
async fn a_locked_session_stays_on_its_account_past_the_soft_limit() {
    let (h, a, _) = pool(|_| {}).await;
    let key = h.key(policy("p")).await;
    locked(&h, "p/client", "s", &opus_on(&a)).await;
    reset_to(&h, answer(5000), answer(5000)).await;
    let full = UsageWindow { name: "5h".into(), used_percent: 99.0, resets_at: None, window_secs: None };
    h.state
        .db
        .put_usage(&crate::db::AccountUsage { account_id: a.clone(), fetched_at: now(), windows: vec![full], raw: None })
        .await
        .unwrap();
    for _ in 0..4 {
        assert_eq!(send(&h, &key, OPUS, "s").await.0, 200);
    }
    assert_eq!(tokens(&h).await, ["a", "a", "a", "a"], "round robin would alternate; the soft limit would move it");

    // Another session is not pinned.
    reset_to(&h, answer(1), answer(1)).await;
    for _ in 0..2 {
        assert_eq!(send(&h, &key, OPUS, "other").await.0, 200);
    }
    assert_eq!(tokens(&h).await, ["b", "b"], "the account above the soft limit is used last");
}

#[tokio::test]
async fn upstream_errors_stay_on_the_lock_until_max_attempts_in_a_row() {
    let (h, a, b) = pool(|_| {}).await;
    let key = h.key(policy("p")).await;
    locked(&h, "p/client", "s", &opus_on(&a)).await;
    let failing = || ResponseTemplate::new(500).set_body_json(json!({"error": {"type": "api_error", "message": "boom"}}));

    reset_to(&h, failing(), answer(5000)).await;
    for _ in 0..2 {
        assert_eq!(send(&h, &key, OPUS, "s").await.0, 500, "the client sees the error");
    }
    assert_eq!(tokens(&h).await, ["a", "a"], "no failover while the lock holds");
    assert_eq!(session_row(&h, "p/client", 1).await.lock_failures, 2);

    // A success ends the run: two more failures still don't move it.
    reset_to(&h, answer(5000), answer(5000)).await;
    assert_eq!(send(&h, &key, OPUS, "s").await.0, 200);
    let row = session_row(&h, "p/client", 2).await;
    assert_eq!((row.lock_failures, row.lock_breaks), (0, 0));
    reset_to(&h, failing(), answer(5000)).await;
    for _ in 0..2 {
        assert_eq!(send(&h, &key, OPUS, "s").await.0, 500);
    }
    assert_eq!(tokens(&h).await, ["a", "a"]);

    // The third in a row breaks the lock and fails over, in the same request.
    assert_eq!(send(&h, &key, OPUS, "s").await.0, 200);
    assert_eq!(tokens(&h).await, ["a", "a", "a", "b"]);
    let row = session_row(&h, "p/client", 3).await;
    assert_eq!((row.lock_breaks, row.last_break_reason.as_deref()), (1, Some("upstream_errors")));
    assert_eq!(row.lock_account.as_ref(), Some(&b), "the session is past the threshold, so it locks where it landed");
    assert_eq!(row.lock_failures, 0);
}

#[tokio::test]
async fn a_transport_error_counts_like_an_upstream_error() {
    let (h, a, _) = pool(|cfg| cfg.subscription.endpoints.claude_api = "http://127.0.0.1:9".into()).await;
    let key = h.key(policy("p")).await;
    locked(&h, "p/client", "s", &opus_on(&a)).await;
    assert_eq!(send(&h, &key, OPUS, "s").await.0, 502);
    let row = session_row(&h, "p/client", 1).await;
    assert_eq!((row.lock_failures, row.lock_breaks), (1, 0), "one attempt, on the locked account");
    assert_eq!(row.lock_account.as_ref(), Some(&a));
}

#[tokio::test]
async fn a_refused_request_says_nothing_about_the_lock() {
    let (h, a, _) = pool(|_| {}).await;
    let key = h.key(policy("p")).await;
    locked(&h, "p/client", "s", &opus_on(&a)).await;
    let refused = || ResponseTemplate::new(400).set_body_json(json!({"error": {"type": "invalid_request_error", "message": "no"}}));
    reset_to(&h, refused(), answer(5000)).await;
    for _ in 0..4 {
        assert_eq!(send(&h, &key, OPUS, "s").await.0, 400);
    }
    assert_eq!(tokens(&h).await, ["a", "a", "a", "a"]);
    let row = session_row(&h, "p/client", 1).await;
    assert_eq!((row.lock_failures, row.lock_breaks, row.lock_account.as_ref()), (0, 0, Some(&a)));
}

#[tokio::test]
async fn an_exhausted_quota_breaks_the_lock_and_fails_over() {
    let (h, a, b) = pool(|_| {}).await;
    let key = h.key(policy("p")).await;
    locked(&h, "p/client", "s", &opus_on(&a)).await;
    let exhausted = ResponseTemplate::new(429)
        .insert_header("anthropic-ratelimit-unified-5h-status", "rejected")
        .insert_header("anthropic-ratelimit-unified-5h-reset", (now() + 3600).to_string().as_str())
        .set_body_json(json!({"error": {"type": "rate_limit_error", "message": "limit"}}));
    reset_to(&h, exhausted, answer(5000)).await;
    assert_eq!(send(&h, &key, OPUS, "s").await.0, 200);
    assert_eq!(tokens(&h).await, ["a", "b"]);
    assert!(h.state.db.get_account(&a).await.unwrap().unwrap().cooling_down(now()), "the quota cooled the account");
    let row = session_row(&h, "p/client", 2).await;
    assert_eq!((row.last_break_reason.as_deref(), row.lock_account.as_ref()), (Some("cooldown"), Some(&b)));
}

#[tokio::test]
async fn a_credential_that_cannot_be_refreshed_breaks_the_lock() {
    let (h, a, b) = pool(|_| {}).await;
    let key = h.key(policy("p")).await;
    locked(&h, "p/client", "s", &opus_on(&a)).await;
    // The 401 forces a refresh, and the token endpoint isn't there.
    reset_to(&h, ResponseTemplate::new(401), answer(5000)).await;
    assert_eq!(send(&h, &key, OPUS, "s").await.0, 200);
    assert_eq!(tokens(&h).await, ["a", "b"]);
    let row = session_row(&h, "p/client", 2).await;
    assert_eq!((row.last_break_reason.as_deref(), row.lock_account.as_ref()), (Some("account_lost"), Some(&b)));
}

#[tokio::test]
async fn a_cooldown_during_the_request_breaks_the_lock_and_fails_over() {
    let (h, a, b) = pool(|_| {}).await;
    let key = h.key(policy("p")).await;
    locked(&h, "p/client", "s", &opus_on(&a)).await;
    reset_to(
        &h,
        ResponseTemplate::new(429)
            .insert_header("retry-after", "30")
            .set_body_json(json!({"error": {"type": "rate_limit_error", "message": "slow"}})),
        answer(5000),
    )
    .await;
    assert_eq!(send(&h, &key, OPUS, "s").await.0, 200);
    assert_eq!(tokens(&h).await, ["a", "b"]);
    assert!(h.state.db.get_account(&a).await.unwrap().unwrap().cooling_down(now()), "the 429 cooled the account");
    let row = session_row(&h, "p/client", 2).await;
    assert_eq!((row.lock_breaks, row.last_break_reason.as_deref(), row.lock_account.as_ref()), (1, Some("cooldown"), Some(&b)));
}

#[tokio::test]
async fn an_account_that_rejects_its_credential_breaks_the_lock() {
    let (h, a, b) = pool(|_| {}).await;
    let key = h.key(policy("p")).await;
    locked(&h, "p/client", "s", &opus_on(&a)).await;
    reset_to(&h, ResponseTemplate::new(401), answer(5000)).await;
    Mock::given(path("/v1/oauth/token"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"access_token": "tok-a", "refresh_token": "rt2", "expires_in": 3600})),
        )
        .mount(&h.upstream)
        .await;
    assert_eq!(send(&h, &key, OPUS, "s").await.0, 200);
    assert_eq!(tokens(&h).await, ["a", "a", "b"], "the forced refresh retries once on the account");
    assert!(h.state.db.get_account(&a).await.unwrap().unwrap().auth_failed);
    let row = session_row(&h, "p/client", 2).await;
    assert_eq!((row.last_break_reason.as_deref(), row.lock_account.as_ref()), (Some("account_lost"), Some(&b)));
}

#[tokio::test]
async fn an_unavailable_account_breaks_the_lock_with_its_reason() {
    enum Loss {
        Cooling,
        Disabled,
        NeedsLogin,
        Removed,
    }
    for (loss, want) in
        [(Loss::Cooling, "cooldown"), (Loss::Disabled, "account_lost"), (Loss::NeedsLogin, "account_lost"), (Loss::Removed, "account_lost")]
    {
        let (h, a, _) = pool(|_| {}).await;
        let key = h.key(policy("p")).await;
        locked(&h, "p/client", "s", &opus_on(&a)).await;
        reset_to(&h, answer(5000), answer(5000)).await;
        match loss {
            Loss::Cooling => h.state.db.set_cooldown(&a, now() + 100, "quota").await.unwrap(),
            Loss::Disabled => {
                h.state.db.set_account_enabled(&a, false).await.unwrap();
            }
            Loss::NeedsLogin => h.state.db.mark_auth_failed(&a, true, Some("rejected")).await.unwrap(),
            Loss::Removed => {
                h.state.subs.remove_account(&a).await.unwrap();
            }
        }
        assert_eq!(send(&h, &key, OPUS, "s").await.0, 200);
        assert_eq!(tokens(&h).await, ["b"]);
        let row = session_row(&h, "p/client", 2).await;
        assert_eq!((row.last_break_reason.as_deref(), row.lock_breaks), (Some(want), 1));
    }
}

#[tokio::test]
async fn an_account_without_a_loaded_credential_breaks_the_lock() {
    let (h, _, _) = pool(|_| {}).await;
    let key = h.key(policy("p")).await;
    let row = crate::db::AccountRow {
        id: "claude-unloaded".into(),
        provider: "claude".into(),
        label: "unloaded".into(),
        email: None,
        external_id: None,
        plan: None,
        enabled: true,
        priority: 0,
        cooldown_until: None,
        cooldown_reason: None,
        auth_failed: false,
        last_error: None,
        last_used_at: None,
        created_at: now(),
    };
    h.state.db.upsert_account(&row).await.unwrap();
    locked(&h, "p/client", "s", &opus_on(&row.id)).await;
    reset_to(&h, answer(5000), answer(5000)).await;
    assert_eq!(send(&h, &key, OPUS, "s").await.0, 200);
    assert_eq!(session_row(&h, "p/client", 2).await.last_break_reason.as_deref(), Some("account_lost"));
}

#[tokio::test]
async fn a_policy_that_drops_the_account_breaks_the_lock() {
    let (h, a, b) = pool(|_| {}).await;
    let p = Policy { accounts: BTreeMap::from([("claude".to_string(), vec![b.clone()])]), ..policy("p") };
    let key = h.key(p).await;
    locked(&h, "p/client", "s", &opus_on(&a)).await;
    reset_to(&h, answer(5000), answer(5000)).await;
    assert_eq!(send(&h, &key, OPUS, "s").await.0, 200);
    assert_eq!(tokens(&h).await, ["b"]);
    assert_eq!(session_row(&h, "p/client", 2).await.last_break_reason.as_deref(), Some("policy"));
}

#[tokio::test]
async fn an_account_at_its_policy_limit_breaks_the_lock() {
    let (h, a, _) = pool(|_| {}).await;
    let mut p = policy("p");
    p.limits.accounts.insert(a.clone(), crate::limits::Allowance { requests_per_minute: Some(1), ..Default::default() });
    let key = h.key(p).await;
    locked(&h, "p/client", "s", &opus_on(&a)).await;
    reset_to(&h, answer(5000), answer(5000)).await;
    assert_eq!(send(&h, &key, OPUS, "s").await.0, 200);
    assert_eq!(send(&h, &key, OPUS, "s").await.0, 200);
    assert_eq!(tokens(&h).await, ["a", "b"]);
    assert_eq!(session_row(&h, "p/client", 3).await.last_break_reason.as_deref(), Some("limit"));
}

#[tokio::test]
async fn another_name_routes_normally_and_leaves_the_lock_alone() {
    let (h, a, _) = pool(|_| {}).await;
    let key = h.key(policy("p")).await;
    locked(&h, "p/client", "s", &opus_on(&a)).await;
    reset_to(&h, answer(7), answer(7)).await;
    for _ in 0..3 {
        assert_eq!(send(&h, &key, HAIKU, "s").await.0, 200);
    }
    assert_eq!(tokens(&h).await, ["a", "a", "a"], "it stays on the locked account of the same provider");
    let row = session_row(&h, "p/client", 4).await;
    assert_eq!((row.current_tokens, row.lock_breaks), (5000, 0), "it doesn't change the session's size");
    assert_eq!(row.lock_model.as_deref(), Some("claude-opus-5"));

    // Failing or unavailable, its account only loses the preference.
    let failing = ResponseTemplate::new(500).set_body_json(json!({"error": {"type": "api_error", "message": "boom"}}));
    reset_to(&h, failing, answer(7)).await;
    assert_eq!(send(&h, &key, HAIKU, "s").await.0, 200);
    assert_eq!(tokens(&h).await, ["a", "b"], "it fails over");
    let row = session_row(&h, "p/client", 5).await;
    assert_eq!((row.lock_failures, row.lock_breaks, row.lock_account.as_ref()), (0, 0, Some(&a)));

    h.state.db.set_cooldown(&a, now() + 100, "quota").await.unwrap();
    reset_to(&h, answer(7), answer(7)).await;
    assert_eq!(send(&h, &key, HAIKU, "s").await.0, 200);
    assert_eq!(tokens(&h).await, ["b"]);
    let row = session_row(&h, "p/client", 6).await;
    assert_eq!((row.lock_breaks, row.lock_account.as_ref()), (0, Some(&a)));
}

async fn chat_mock(h: &Harness, model: &str, response: ResponseTemplate) {
    Mock::given(path("/llama/v1/chat/completions"))
        .and(body_partial_json(json!({"model": model})))
        .respond_with(response)
        .mount(&h.upstream)
        .await;
}

fn chat_answer(prompt: i64) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({"choices": [], "usage": {"prompt_tokens": prompt, "completion_tokens": 1}}))
}

async fn chat(h: &Harness, key: &str, model: &str) -> u16 {
    let auth = format!("Bearer {key}");
    let body = json!({"model": model, "messages": [{"role": "user", "content": "x"}]});
    h.call_with("POST", "/v1/chat/completions", &[("authorization", auth.as_str()), ("session_id", "s")], Some(body)).await.0
}

async fn chat_models(h: &Harness) -> Vec<String> {
    let requests = h.upstream.received_requests().await.unwrap();
    requests.iter().map(|r| r.body_json::<Value>().unwrap()["model"].as_str().unwrap().to_string()).collect()
}

fn fast_policy(name: &str, targets: &[&str]) -> Policy {
    let mut p = policy(name);
    p.aliases.insert("fast".into(), AliasTargets::Many(targets.iter().map(|t| t.to_string()).collect()));
    p
}

#[tokio::test]
async fn an_alias_stays_on_its_locked_target_until_the_lock_breaks() {
    let h = harness_with(|cfg| cfg.sessions.lock_tokens = 1000).await;
    let key = h.key(fast_policy("p", &["llama/busy", "llama/qwen3"])).await;
    chat_mock(&h, "busy", ResponseTemplate::new(500)).await;
    chat_mock(&h, "qwen3", chat_answer(5000)).await;
    assert_eq!(chat(&h, &key, "fast").await, 200);
    let row = session_row(&h, "p/client", 1).await;
    assert_eq!((row.lock_model.as_deref(), row.lock_account.as_deref()), (Some("qwen3"), None));

    // The first target recovers: the session stays where its cache is.
    h.upstream.reset().await;
    chat_mock(&h, "busy", chat_answer(5000)).await;
    chat_mock(&h, "qwen3", chat_answer(5000)).await;
    assert_eq!(chat(&h, &key, "fast").await, 200);
    assert_eq!(chat_models(&h).await, ["qwen3"]);
    assert_eq!(session_row(&h, "p/client", 2).await.lock_failures, 0);

    // Its target fails: the client sees it, up to max_attempts in a row.
    h.upstream.reset().await;
    chat_mock(&h, "busy", chat_answer(5000)).await;
    chat_mock(&h, "qwen3", ResponseTemplate::new(503)).await;
    assert_eq!(chat(&h, &key, "fast").await, 503);
    assert_eq!(chat(&h, &key, "fast").await, 503);
    assert_eq!(chat_models(&h).await, ["qwen3", "qwen3"]);
    assert_eq!(session_row(&h, "p/client", 2).await.lock_failures, 2);
    assert_eq!(chat(&h, &key, "fast").await, 200);
    assert_eq!(chat_models(&h).await, ["qwen3", "qwen3", "qwen3", "busy"]);
    let row = session_row(&h, "p/client", 3).await;
    assert_eq!((row.last_break_reason.as_deref(), row.lock_model.as_deref()), (Some("upstream_errors"), Some("busy")));
}

#[tokio::test]
async fn a_target_the_name_no_longer_offers_breaks_the_lock() {
    let h = harness_with(|cfg| cfg.sessions.lock_tokens = 1000).await;
    let key = h.key(fast_policy("p", &["llama/qwen3"])).await;
    let gone = Lock { name: "fast".into(), provider: "llama".into(), model: "busy".into(), account: None };
    let id = crate::context::resolve(
        &{
            let mut m = HeaderMap::new();
            m.insert("session_id", "s".parse().unwrap());
            m
        },
        &json!({}),
    )
    .unwrap();
    let turn = Turn {
        client_id: "p/client".into(),
        session_key: id.key.clone(),
        source: id.source.into(),
        tokens: 5000,
        reported: true,
        ..Default::default()
    };
    h.state.db.record_turn(&turn).await.unwrap();
    h.state.db.form_lock("p/client", &id.key, &gone).await.unwrap();
    chat_mock(&h, "qwen3", chat_answer(5000)).await;
    assert_eq!(chat(&h, &key, "fast").await, 200);
    assert_eq!(chat_models(&h).await, ["qwen3"]);
    let row = session_row(&h, "p/client", 2).await;
    assert_eq!((row.last_break_reason.as_deref(), row.lock_model.as_deref()), (Some("target_unavailable"), Some("qwen3")));
}

#[tokio::test]
async fn a_provider_limit_on_the_locked_target_breaks_the_lock_when_the_alias_can_move() {
    let h = harness_with(|cfg| cfg.sessions.lock_tokens = 1000).await;
    let mut p = fast_policy("p", &["llama/qwen3", "codex/gpt-6-codex"]);
    p.limits.providers.insert("llama".into(), crate::limits::Allowance { requests_per_minute: Some(1), ..Default::default() });
    let key = h.key(p).await;
    chat_mock(&h, "qwen3", chat_answer(5000)).await;
    assert_eq!(chat(&h, &key, "fast").await, 200);
    assert_eq!(session_row(&h, "p/client", 1).await.lock_provider.as_deref(), Some("llama"));
    // llama is at its limit, and codex has no accounts here.
    assert_eq!(chat(&h, &key, "fast").await, 503);
    let row = session_row(&h, "p/client", 1).await;
    assert_eq!((row.last_break_reason.as_deref(), row.lock_provider), (Some("limit"), None));
}

#[tokio::test]
async fn lock_tokens_of_zero_disables_the_lock() {
    let (h, a, _) = pool(|cfg| cfg.sessions.lock_tokens = 0).await;
    let key = h.key(policy("p")).await;
    reset_to(&h, answer(500_000), answer(500_000)).await;
    assert_eq!(send(&h, &key, OPUS, "s").await.0, 200);
    let row = session_row(&h, "p/client", 1).await;
    assert_eq!((row.current_tokens, row.lock_provider), (500_000, None), "accounting stays; no lock forms");

    // A lock formed before it was disabled doesn't apply.
    locked(&h, "p/client", "t", &opus_on(&a)).await;
    h.upstream.reset().await;
    reset_to(&h, answer(1), answer(1)).await;
    for _ in 0..2 {
        assert_eq!(send(&h, &key, OPUS, "t").await.0, 200);
    }
    let mut served = tokens(&h).await;
    served.sort();
    assert_eq!(served, ["a", "b"], "round robin");
}

async fn log_row(h: &Harness) -> (Option<String>, Option<i64>, Option<i64>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let row: Option<(Option<String>, Option<i64>, Option<i64>)> =
            sqlx::query_as("SELECT session_key, context_tokens, context_window FROM request_log ORDER BY id DESC LIMIT 1")
                .fetch_optional(&h.state.db.pool)
                .await
                .unwrap();
        if let Some(row) = row {
            return row;
        }
        assert!(Instant::now() < deadline, "no request log row");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn the_request_log_holds_the_session_and_its_size_against_the_window() {
    let h = harness_with(|cfg| {
        let values = crate::metadata::Metadata { max_input_tokens: Some(8000), ..Default::default() };
        cfg.catalog.metadata.insert("llama/qwen3".into(), crate::metadata::Override { values, ..Default::default() });
    })
    .await;
    let key = h.key(policy("p")).await;
    // Cached tokens are part of the prompt.
    chat_mock(&h, "qwen3", ResponseTemplate::new(200).set_body_json(json!({"choices": [], "usage": {"prompt_tokens": 4000, "prompt_tokens_details": {"cached_tokens": 1000}, "completion_tokens": 1}}))).await;
    assert_eq!(chat(&h, &key, "llama/qwen3").await, 200);
    let row = session_row(&h, "p/client", 1).await;
    assert_eq!(log_row(&h).await, (Some(row.session_key.clone()), Some(4000), Some(8000)));
    assert_eq!((row.current_tokens, row.context_window, row.prompt_tokens, row.cached_tokens), (4000, Some(8000), 4000, 1000));

    // Without a report, the estimate is the size in the log.
    let h = harness().await;
    let key = h.key(policy("p")).await;
    chat_mock(&h, "qwen3", ResponseTemplate::new(200).set_body_json(json!({"choices": []}))).await;
    assert_eq!(chat(&h, &key, "llama/qwen3").await, 200);
    let row = session_row(&h, "p/client", 1).await;
    let (_, size, _) = log_row(&h).await;
    assert!(size.unwrap() > 0, "{size:?}");
    assert_eq!(row.current_tokens, size.unwrap());
}

#[tokio::test]
async fn accounting_leaves_the_forwarded_bytes_and_upstream_calls_alone() {
    let h = harness_with(|cfg| cfg.sessions.lock_tokens = 1).await;
    let key = h.key(policy("p")).await;
    Mock::given(path("/llama/v1/chat/completions")).respond_with(chat_answer(5)).mount(&h.upstream).await;
    let raw = "{ \"model\" : \"qwen3\",\n \"messages\":[ {\"role\":\"user\",\"content\":\"x\"} ] }";
    let send_raw = |uri: &str, key: &str| {
        Request::builder()
            .method("POST")
            .uri(uri)
            .header("authorization", format!("Bearer {key}"))
            .header("content-type", "application/json")
            .body(Body::from(raw))
            .unwrap()
    };
    // A passthrough forwards the client's bytes as they came.
    let app = router(h.state.clone());
    assert_eq!(app.clone().oneshot(send_raw("/llama/v1/chat/completions", &key)).await.unwrap().status(), 200);
    // A routed request forwards the same bytes with a session as without one:
    // the admin token has none.
    let routed = json!({"model": "llama/qwen3", "messages": [{"role": "user", "content": "x"}]});
    for token in [key.as_str(), "boot"] {
        assert_eq!(h.call("POST", "/v1/chat/completions", Some(token), Some(routed.clone())).await.0, 200);
    }
    let received = h.upstream.received_requests().await.unwrap();
    assert_eq!(received.len(), 3, "one upstream call per request, and no others");
    assert_eq!(String::from_utf8_lossy(&received[0].body), raw);
    assert_eq!(received[1].body, received[2].body);
}

#[tokio::test]
async fn only_a_client_keys_conversations_have_sessions() {
    let (h, _, _) = pool(|_| {}).await;
    let key = h.key(policy("p")).await;
    reset_to(&h, answer(5), answer(5)).await;
    // The admin token has no client key.
    assert_eq!(send(&h, "boot", OPUS, "s").await.0, 200);
    // Counting tokens is not a turn.
    Mock::given(path("/v1/messages/count_tokens"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"input_tokens": 5})))
        .mount(&h.upstream)
        .await;
    let auth = format!("Bearer {key}");
    let body = json!({"model": OPUS, "messages": [{"role": "user", "content": "x"}]});
    assert_eq!(
        h.call_with("POST", "/v1/messages/count_tokens", &[("authorization", auth.as_str()), (SESSION, "s")], Some(body)).await.0,
        200
    );
    // Other endpoints have no conversation.
    Mock::given(path("/llama/v1/embeddings")).respond_with(chat_answer(5)).mount(&h.upstream).await;
    assert_eq!(h.call("POST", "/v1/embeddings", Some(&key), Some(json!({"model": "llama/qwen3", "input": "x"}))).await.0, 200);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(h.state.db.sessions(None, 10).await.unwrap().is_empty());
}

#[tokio::test]
async fn passthroughs_are_counted_and_only_a_pool_can_lock() {
    let h = harness_with(|cfg| cfg.sessions.lock_tokens = 1000).await;
    let key = h.key(policy("p")).await;
    Mock::given(path("/llama/v1/chat/completions")).respond_with(chat_answer(5000)).mount(&h.upstream).await;
    let body = json!({"model": "qwen3", "messages": [{"role": "user", "content": "x"}]});
    assert_eq!(h.call("POST", "/llama/v1/chat/completions", Some(&key), Some(body.clone())).await.0, 200);
    let row = session_row(&h, "p/client", 1).await;
    assert_eq!((row.current_tokens, row.lock_provider), (5000, None), "nothing routes a gateway passthrough");

    let h = harness_with(|cfg| cfg.sessions.lock_tokens = 1000).await;
    let key = h.key(policy("p")).await;
    let id = h.account(Credential { provider: SubProvider::OpencodeGo, api_key: Some("ocg-key".into()), ..Default::default() }).await;
    Mock::given(path("/zen/go/v1/chat/completions")).respond_with(chat_answer(5000)).mount(&h.upstream).await;
    let body = json!({"model": "kimi-k3", "messages": [{"role": "user", "content": "x"}]});
    assert_eq!(h.call("POST", "/opencode-go/v1/chat/completions", Some(&key), Some(body)).await.0, 200);
    let row = session_row(&h, "p/client", 1).await;
    assert_eq!((row.lock_account.as_ref(), row.lock_name.as_deref()), (Some(&id), Some("opencode-go/kimi-k3")));
}

async fn tools_of(h: &Harness) -> Vec<(String, String, i64)> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let rows = h.state.db.tool_usage(Some("p/client")).await.unwrap();
        if !rows.is_empty() {
            return rows.into_iter().map(|r| (r.tool, r.source, r.calls)).collect();
        }
        assert!(Instant::now() < deadline, "no tool counts");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn a_messages_response_counts_its_tools_and_a_failed_one_does_not() {
    let (h, _, _) = pool(|_| {}).await;
    let key = h.key(policy("p")).await;
    let tool_use = ResponseTemplate::new(200)
        .set_body_json(json!({"content": [{"type": "tool_use", "name": "read"}], "usage": {"input_tokens": 1, "output_tokens": 1}}));
    reset_to(&h, tool_use.clone(), tool_use).await;
    assert_eq!(send(&h, &key, OPUS, "s").await.0, 200);
    assert_eq!(tools_of(&h).await, [("read".to_string(), "llm".to_string(), 1)]);

    let failing = ResponseTemplate::new(400).set_body_json(
        json!({"error": {"type": "invalid_request_error", "message": "no"}, "content": [{"type": "tool_use", "name": "bad"}]}),
    );
    reset_to(&h, failing.clone(), failing).await;
    assert_eq!(send(&h, &key, OPUS, "s").await.0, 400);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(tools_of(&h).await.len(), 1);
}

#[tokio::test]
async fn a_failed_response_through_the_tap_counts_no_tools() {
    let h = harness().await;
    let key = h.key(policy("p")).await;
    let named = json!({"error": "boom", "choices": [{"message": {"tool_calls": [{"function": {"name": "bad"}}]}}]});
    chat_mock(&h, "qwen3", ResponseTemplate::new(500).set_body_json(named.clone())).await;
    chat_mock(
        &h,
        "busy",
        ResponseTemplate::new(200).set_body_json(json!({"choices": [{"message": {"tool_calls": [{"function": {"name": "good"}}]}}]})),
    )
    .await;
    assert_eq!(chat(&h, &key, "llama/qwen3").await, 500);
    assert_eq!(chat(&h, &key, "llama/busy").await, 200);
    // Written after each response finishes; the later success shows the writes have run.
    assert_eq!(tools_of(&h).await, [("good".to_string(), "llm".to_string(), 1)]);
}

#[tokio::test]
async fn a_stream_counts_each_tool_once() {
    let h = harness().await;
    let key = h.key(policy("p")).await;
    h.codex_account().await;
    let item = json!({"type": "function_call", "name": "write"});
    let stream = format!(
        "data: {}\n\ndata: {}\n\ndata: {}\n\n",
        json!({"type": "response.output_item.added", "item": item}),
        json!({"type": "response.output_item.done", "item": item}),
        json!({"type": "response.completed", "response": {"output": [item], "usage": {"input_tokens": 1, "output_tokens": 1}}})
    );
    Mock::given(path("/backend-api/codex/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(stream, "text/event-stream"))
        .mount(&h.upstream)
        .await;
    let (status, _) =
        h.call("POST", "/v1/responses", Some(&key), Some(json!({"model": "gpt-6-codex", "input": "x", "stream": true}))).await;
    assert_eq!(status, 200);
    assert_eq!(tools_of(&h).await, [("write".to_string(), "llm".to_string(), 1)]);
}

#[tokio::test]
async fn the_admin_api_reports_sessions_and_tools() {
    let h = harness_with(|cfg| {
        cfg.sessions.lock_tokens = 1000;
        let values = crate::metadata::Metadata { max_input_tokens: Some(8000), ..Default::default() };
        cfg.catalog.metadata.insert("llama/qwen3".into(), crate::metadata::Override { values, ..Default::default() });
    })
    .await;
    let key = h.key(policy("p")).await;
    chat_mock(
        &h,
        "qwen3",
        ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"tool_calls": [{"function": {"name": "read"}}]}}],
            "usage": {"prompt_tokens": 4000, "prompt_tokens_details": {"cached_tokens": 1000}, "completion_tokens": 1}
        })),
    )
    .await;
    assert_eq!(chat(&h, &key, "llama/qwen3").await, 200);
    session_row(&h, "p/client", 1).await;

    assert_eq!(h.call("GET", "/admin/sessions", None, None).await.0, 401);
    assert_eq!(h.call("GET", "/admin/tools", None, None).await.0, 401);
    assert_eq!(h.call("GET", "/admin/sessions", Some(&key), None).await.0, 403, "a proxy key can't read them");

    let (status, body) = h.call("GET", "/admin/sessions?client=p/client", Some("boot"), None).await;
    assert_eq!(status, 200, "{body}");
    let s = &body[0];
    assert_eq!((s["current_tokens"].as_i64(), s["peak_tokens"].as_i64(), s["turns"].as_i64()), (Some(4000), Some(4000), Some(1)));
    assert_eq!(s["utilization"], 0.5);
    assert_eq!(s["peak_utilization"], 0.5);
    assert_eq!(s["cache_hit_ratio"], 0.25);
    assert_eq!(s["lock_provider"], "llama");
    assert_eq!(h.call("GET", "/admin/sessions?client=other", Some("boot"), None).await.1, json!([]));

    let deadline = Instant::now() + Duration::from_secs(10);
    let tools = loop {
        let (_, body) = h.call("GET", "/admin/tools?client=p/client", Some("boot"), None).await;
        if body.as_array().is_some_and(|a| !a.is_empty()) {
            break body;
        }
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    assert_eq!((tools[0]["tool"].as_str(), tools[0]["source"].as_str(), tools[0]["calls"].as_i64()), (Some("read"), Some("llm"), Some(1)));
}
