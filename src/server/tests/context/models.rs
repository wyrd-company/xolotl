//! A session keeps the models it is on past a policy that excludes them.
// ---
// relationships:
//   tests: context
// ---
use super::*;
use crate::policy::Rule;

async fn put(h: &Harness, p: Policy) {
    h.state.keys.put_policy(p, &h.state.catalog.known_providers(), &h.state.mcp.server_names()).await.unwrap();
}

/// The policy `p` with `llama/qwen3` excluded, each way a policy can.
fn excluding_qwen() -> Vec<(&'static str, Policy)> {
    let rule = Policy { rules: vec![Rule::Deny("llama/qwen3".into())], ..policy("p") };
    let mut switch = policy("p");
    switch.models.insert("llama/qwen3".into(), false);
    let others = Policy { rules: vec![Rule::Allow("llama/busy".into())], deny_other_models: true, ..policy("p") };
    vec![("a rule", rule), ("a switch", switch), ("deny_other_models", others)]
}

/// A gateway that answers every Chat request.
async fn answering() -> Harness {
    let h = harness().await;
    Mock::given(path("/llama/v1/chat/completions")).respond_with(chat_answer(10)).mount(&h.upstream).await;
    h
}

async fn turns(h: &Harness) -> i64 {
    h.state.db.sessions(Some("p/client"), 100).await.unwrap().iter().map(|r| r.turns).sum()
}

/// A turn of the conversation that opened with `opening`, in session
/// `session`. A success returns once it is recorded: the next request reads
/// the models it recorded.
async fn turn(h: &Harness, key: &str, session: &str, opening: &str, model: &str) -> u16 {
    let before = turns(h).await;
    let auth = format!("Bearer {key}");
    let body = json!({"model": model, "messages": [
        {"role": "user", "content": opening}, {"role": "assistant", "content": "a"}, {"role": "user", "content": "go on"}]});
    let status =
        h.call_with("POST", "/v1/chat/completions", &[("authorization", auth.as_str()), ("session_id", session)], Some(body)).await.0;
    let deadline = Instant::now() + Duration::from_secs(10);
    while status == 200 && turns(h).await == before {
        assert!(Instant::now() < deadline, "the turn was not recorded");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    status
}

#[tokio::test]
async fn a_session_keeps_its_model_after_the_policy_excludes_it() {
    for (how, excluding) in excluding_qwen() {
        let h = answering().await;
        let key = h.key(policy("p")).await;
        assert_eq!(turn(&h, &key, "s", "hello", "qwen3").await, 200);
        put(&h, excluding).await;

        assert_eq!(turn(&h, &key, "s", "hello", "qwen3").await, 200, "{how}: the conversation goes on");
        assert_eq!(turn(&h, &key, "new", "hello", "qwen3").await, 404, "{how}: a new session can't start with it");
        assert_eq!(turn(&h, &key, "s", "title this", "busy").await, 200, "{how}");
        assert_eq!(turn(&h, &key, "s", "hello", "qwen3").await, 200, "{how}: a background request switches nothing");
        assert_eq!(chat_models(&h).await, ["qwen3", "qwen3", "busy", "qwen3"], "{how}");

        assert_eq!(turn(&h, &key, "s", "hello", "busy").await, 200, "{how}: the conversation switches");
        assert_eq!(turn(&h, &key, "s", "hello", "qwen3").await, 404, "{how}: and can't switch back");
        assert_eq!(turn(&h, &key, "s", "hello", "llama/qwen3").await, 404, "{how}: under any name");
    }
}

#[tokio::test]
async fn a_model_the_policy_never_allowed_is_refused_in_every_session() {
    let h = answering().await;
    let mut p = Policy { rules: vec![Rule::Deny("llama/qwen3".into())], ..policy("p") };
    p.aliases.insert("fast".into(), AliasTargets::One("llama/qwen3".into()));
    let key = h.key(p).await;
    assert_eq!(turn(&h, &key, "s", "hello", "qwen3").await, 404);
    assert_eq!(turn(&h, &key, "s", "hello", "busy").await, 200);
    assert_eq!(turn(&h, &key, "s", "hello", "qwen3").await, 404);

    // An alias reaches it, which doesn't make it the session's to keep.
    assert_eq!(turn(&h, &key, "s", "hello", "fast").await, 200);
    assert_eq!(turn(&h, &key, "s", "hello", "qwen3").await, 404);
    assert_eq!(turn(&h, &key, "s", "hello", "llama/qwen3").await, 404);
    assert_eq!(chat_models(&h).await, ["busy", "qwen3"]);
}

#[tokio::test]
async fn a_session_does_not_keep_a_provider_the_policy_drops() {
    let h = answering().await;
    let key = h.key(policy("p")).await;
    assert_eq!(turn(&h, &key, "s", "hello", "qwen3").await, 200);
    let claude_only = BTreeMap::from([("claude".to_string(), vec!["*".to_string()])]);
    put(&h, Policy { providers: vec!["claude".into()], accounts: claude_only, ..policy("p") }).await;
    assert_eq!(turn(&h, &key, "s", "hello", "qwen3").await, 404);
}

#[tokio::test]
async fn counting_tokens_and_passthroughs_keep_the_model_too() {
    let h = answering().await;
    let key = h.key(policy("p")).await;
    assert_eq!(turn(&h, &key, "s", "hello", "qwen3").await, 200);
    put(&h, excluding_qwen().remove(0).1).await;
    let auth = format!("Bearer {key}");
    let body = json!({"model": "qwen3", "messages": [{"role": "user", "content": "hello"}]});
    let call = |uri: &'static str, session: &'static str| {
        let headers = [("authorization", auth.as_str()), ("session_id", session)];
        let body = body.clone();
        let h = &h;
        async move { h.call_with("POST", uri, &headers, Some(body)).await.0 }
    };
    assert_eq!(call("/v1/messages/count_tokens", "s").await, 200);
    assert_eq!(call("/v1/messages/count_tokens", "new").await, 404);
    assert_eq!(call("/llama/v1/chat/completions", "s").await, 200);
    assert_eq!(call("/llama/v1/chat/completions", "new").await, 403);

    // Another endpoint has no conversation, so it has nothing to keep.
    Mock::given(path("/llama/v1/embeddings")).respond_with(chat_answer(5)).mount(&h.upstream).await;
    let embed = json!({"model": "llama/qwen3", "input": "hello"});
    let headers = [("authorization", auth.as_str()), ("session_id", "s")];
    assert_eq!(h.call_with("POST", "/v1/embeddings", &headers, Some(embed.clone())).await.0, 404);
    assert_eq!(h.call_with("POST", "/llama/v1/embeddings", &headers, Some(embed)).await.0, 403);
}

#[tokio::test]
async fn a_locked_session_stays_on_its_excluded_model_and_account() {
    let (h, _, _) = pool(|_| {}).await;
    let key = h.key(policy("p")).await;
    reset_to(&h, answer(5000), answer(5000)).await;
    assert_eq!(send(&h, &key, OPUS, "s").await.0, 200);
    let account = session_row(&h, "p/client", 1).await.lock_account.expect("a lock");
    let mut p = policy("p");
    p.models.insert(OPUS.into(), false);
    put(&h, p).await;

    for _ in 0..2 {
        assert_eq!(send(&h, &key, OPUS, "s").await.0, 200);
    }
    let row = session_row(&h, "p/client", 3).await;
    assert_eq!((row.lock_breaks, row.lock_account), (0, Some(account)), "the lock holds");
    let served = tokens(&h).await;
    assert!(served.iter().all(|t| *t == served[0]), "{served:?}");
    assert_eq!(send(&h, &key, OPUS, "other").await.0, 404, "a new session can't start with it");

    // Another name is a side request to the lock, and it can still carry the conversation away.
    assert_eq!(send(&h, &key, HAIKU, "s").await.0, 200);
    session_row(&h, "p/client", 4).await;
    assert_eq!(send(&h, &key, OPUS, "s").await.0, 404, "so it can't switch back");
}

#[tokio::test]
async fn a_kept_model_keeps_its_name_when_another_provider_serves_it() {
    let (h, _, _) = pool(|_| {}).await;
    let key = h.key(policy("p")).await;
    reset_to(&h, answer(5), answer(5)).await;
    assert_eq!(send(&h, &key, "claude-haiku-5-5", "s").await.0, 200);
    session_row(&h, "p/client", 1).await;
    put(&h, Policy { rules: vec![Rule::Deny("claude/claude-haiku-*".into())], ..policy("p") }).await;

    assert_eq!(send(&h, &key, "claude-haiku-5-5", "s").await.0, 200);
    assert_eq!(tokens(&h).await.len(), 2, "both on Claude");
    // A new session gets OpenCode Go's model of that name, which has no account here.
    assert_ne!(send(&h, &key, "claude-haiku-5-5", "new").await.0, 200);
    assert_eq!(tokens(&h).await.len(), 2);
}
