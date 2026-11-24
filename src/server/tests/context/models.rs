//! A conversation keeps the model it is on past a policy that excludes it.
//! Nothing here waits for accounting: what a request records about its
//! conversation is visible to the next request as soon as it has answered.
// ---
// relationships:
//   tests: context
// ---
use super::*;
use crate::keys::IssueRequest;
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

/// The first request of a conversation that opens with `opening`.
fn first(model: &str, opening: &str) -> Value {
    json!({"model": model, "messages": [{"role": "user", "content": opening}]})
}

/// A later request of that conversation: it carries the earlier turns.
fn later(model: &str, opening: &str) -> Value {
    json!({"model": model, "messages": [
        {"role": "user", "content": opening}, {"role": "assistant", "content": "a"}, {"role": "user", "content": "go on"}]})
}

async fn chat_in(h: &Harness, key: &str, session: &str, body: Value) -> u16 {
    let auth = format!("Bearer {key}");
    h.call_with("POST", "/v1/chat/completions", &[("authorization", auth.as_str()), ("session_id", session)], Some(body)).await.0
}

#[tokio::test]
async fn a_conversation_keeps_its_model_after_the_policy_excludes_it() {
    for (how, excluding) in excluding_qwen() {
        let h = answering().await;
        let key = h.key(policy("p")).await;
        assert_eq!(chat_in(&h, &key, "s", first("qwen3", "hello")).await, 200);
        put(&h, excluding).await;

        assert_eq!(chat_in(&h, &key, "s", later("qwen3", "hello")).await, 200, "{how}: the conversation goes on");
        assert_eq!(chat_in(&h, &key, "new", first("qwen3", "hello")).await, 404, "{how}: a new session can't start with it");
        assert_eq!(chat_in(&h, &key, "s", first("busy", "title this")).await, 200, "{how}");
        assert_eq!(chat_in(&h, &key, "s", later("qwen3", "hello")).await, 200, "{how}: a background request switches nothing");
        assert_eq!(chat_models(&h).await, ["qwen3", "qwen3", "busy", "qwen3"], "{how}");

        assert_eq!(chat_in(&h, &key, "s", later("busy", "hello")).await, 200, "{how}: the conversation switches");
        assert_eq!(chat_in(&h, &key, "s", later("qwen3", "hello")).await, 404, "{how}: and can't switch back");
        assert_eq!(chat_in(&h, &key, "s", later("llama/qwen3", "hello")).await, 404, "{how}: under any name");
    }
}

#[tokio::test]
async fn another_conversation_of_the_session_does_not_share_the_model() {
    let h = answering().await;
    let key = h.key(policy("p")).await;
    assert_eq!(chat_in(&h, &key, "s", first("qwen3", "hello")).await, 200);
    assert_eq!(chat_in(&h, &key, "s", first("qwen3", "subagent")).await, 200);
    put(&h, excluding_qwen().remove(0).1).await;

    assert_eq!(chat_in(&h, &key, "s", later("qwen3", "other")).await, 404, "a conversation that never ran on it");
    assert_eq!(chat_in(&h, &key, "s", first("qwen3", "hello")).await, 404, "one that starts again with the same opening");

    // Two conversations on one model: one switches away, the other keeps it.
    assert_eq!(chat_in(&h, &key, "s", later("busy", "hello")).await, 200);
    assert_eq!(chat_in(&h, &key, "s", later("qwen3", "hello")).await, 404);
    assert_eq!(chat_in(&h, &key, "s", later("qwen3", "subagent")).await, 200);
}

#[tokio::test]
async fn a_conversation_without_a_session_header_is_told_from_a_new_one_with_the_same_opening() {
    let h = answering().await;
    let key = h.key(policy("p")).await;
    let auth = format!("Bearer {key}");
    let chat = |messages: Value| {
        let auth = auth.clone();
        let h = &h;
        async move {
            let body = json!({"model": "qwen3", "messages": messages});
            h.call_with("POST", "/v1/chat/completions", &[("authorization", auth.as_str())], Some(body)).await.0
        }
    };
    let opening = || vec![json!({"role": "system", "content": "s"}), json!({"role": "user", "content": "hello"})];
    let continued =
        || [opening(), vec![json!({"role": "assistant", "content": "a"}), json!({"role": "user", "content": "go on"})]].concat();
    assert_eq!(chat(json!(opening())).await, 200);
    put(&h, excluding_qwen().remove(0).1).await;
    assert_eq!(chat(json!(continued())).await, 200, "the conversation goes on");
    assert_eq!(chat(json!(opening())).await, 404, "a new one with the same system prompt and opening can't start");
}

/// The key `p/client` as the vault holds it now.
async fn current_key(h: &Harness) -> String {
    let path = h.state.keys.get("p/client").unwrap().vault_path;
    h.state.secrets.get(&path).await.unwrap().unwrap()["api_key"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn a_rotated_key_keeps_its_conversations_and_a_key_issued_again_does_not() {
    let h = answering().await;
    let key = h.key(policy("p")).await;
    assert_eq!(chat_in(&h, &key, "s", first("qwen3", "hello")).await, 200);
    put(&h, excluding_qwen().remove(0).1).await;

    h.state.keys.rotate("p/client", 0).await.unwrap();
    let rotated = current_key(&h).await;
    assert_ne!(rotated, key);
    assert_eq!(chat_in(&h, &rotated, "s", later("qwen3", "hello")).await, 200, "a rotation is the same issuance");

    h.state.keys.revoke("p/client").await.unwrap();
    h.state.keys.issue(IssueRequest { policy: "p".into(), name: "client".into(), ..Default::default() }).await.unwrap();
    let again = current_key(&h).await;
    assert_eq!(chat_in(&h, &again, "s", later("qwen3", "hello")).await, 404, "a key issued again under the name starts over");
}

#[tokio::test]
async fn an_older_request_that_finishes_after_a_switch_does_not_undo_it() {
    let h = harness().await;
    let key = h.key(policy("p")).await;
    chat_mock(&h, "qwen3", chat_answer(10)).await;
    assert_eq!(chat_in(&h, &key, "s", first("qwen3", "hello")).await, 200);
    put(&h, excluding_qwen().remove(0).1).await;

    h.upstream.reset().await;
    chat_mock(&h, "qwen3", chat_answer(10).set_delay(Duration::from_millis(500))).await;
    chat_mock(&h, "busy", chat_answer(10)).await;
    let (slow, switch) = tokio::join!(chat_in(&h, &key, "s", later("qwen3", "hello")), async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        chat_in(&h, &key, "s", later("busy", "hello")).await
    });
    assert_eq!((slow, switch), (200, 200));
    assert_eq!(chat_in(&h, &key, "s", later("qwen3", "hello")).await, 404, "the switch stands");
}

#[tokio::test]
async fn a_model_the_policy_never_allowed_is_refused_in_every_conversation() {
    let h = answering().await;
    let mut p = Policy { rules: vec![Rule::Deny("llama/qwen3".into())], ..policy("p") };
    p.aliases.insert("fast".into(), AliasTargets::One("llama/qwen3".into()));
    let key = h.key(p).await;
    assert_eq!(chat_in(&h, &key, "s", first("qwen3", "hello")).await, 404);
    assert_eq!(chat_in(&h, &key, "s", first("busy", "hello")).await, 200);
    assert_eq!(chat_in(&h, &key, "s", later("qwen3", "hello")).await, 404);

    // An alias reaches it, which doesn't make it the conversation's to keep.
    assert_eq!(chat_in(&h, &key, "s", later("fast", "hello")).await, 200);
    assert_eq!(chat_in(&h, &key, "s", later("qwen3", "hello")).await, 404);
    assert_eq!(chat_in(&h, &key, "s", later("llama/qwen3", "hello")).await, 404);
    assert_eq!(chat_models(&h).await, ["busy", "qwen3"]);
}

#[tokio::test]
async fn a_conversation_does_not_keep_a_provider_the_policy_drops() {
    let h = answering().await;
    let key = h.key(policy("p")).await;
    assert_eq!(chat_in(&h, &key, "s", first("qwen3", "hello")).await, 200);
    let claude_only = BTreeMap::from([("claude".to_string(), vec!["*".to_string()])]);
    put(&h, Policy { providers: vec!["claude".into()], accounts: claude_only, ..policy("p") }).await;
    assert_eq!(chat_in(&h, &key, "s", later("qwen3", "hello")).await, 404);
}

#[tokio::test]
async fn counting_tokens_and_passthroughs_keep_the_model_too() {
    let h = answering().await;
    let key = h.key(policy("p")).await;
    assert_eq!(chat_in(&h, &key, "s", first("qwen3", "hello")).await, 200);
    put(&h, excluding_qwen().remove(0).1).await;
    let auth = format!("Bearer {key}");
    let call = |uri: &'static str, session: &'static str| {
        let headers = [("authorization", auth.as_str()), ("session_id", session)];
        let h = &h;
        async move { h.call_with("POST", uri, &headers, Some(later("qwen3", "hello"))).await.0 }
    };
    assert_eq!(call("/v1/messages/count_tokens", "s").await, 200);
    assert_eq!(call("/v1/messages/count_tokens", "new").await, 404);
    assert_eq!(call("/llama/v1/chat/completions", "s").await, 200);
    assert_eq!(call("/llama/v1/chat/completions", "new").await, 403);

    // A passthrough switch is the conversation's switch too.
    let body = json!({"model": "busy", "messages": later("busy", "hello")["messages"]});
    let headers = [("authorization", auth.as_str()), ("session_id", "s")];
    assert_eq!(h.call_with("POST", "/llama/v1/chat/completions", &headers, Some(body)).await.0, 200);
    assert_eq!(call("/llama/v1/chat/completions", "s").await, 403);

    // Another endpoint has no conversation, so it has nothing to keep.
    Mock::given(path("/llama/v1/embeddings")).respond_with(chat_answer(5)).mount(&h.upstream).await;
    let embed = json!({"model": "llama/qwen3", "input": "hello"});
    assert_eq!(h.call_with("POST", "/v1/embeddings", &headers, Some(embed.clone())).await.0, 404);
    assert_eq!(h.call_with("POST", "/llama/v1/embeddings", &headers, Some(embed)).await.0, 403);
}

async fn claude_in(h: &Harness, key: &str, session: &str, body: Value) -> u16 {
    let auth = format!("Bearer {key}");
    h.call_with("POST", "/v1/messages", &[("authorization", auth.as_str()), (SESSION, session)], Some(body)).await.0
}

#[tokio::test]
async fn a_locked_conversation_stays_on_its_excluded_model_and_account() {
    let (h, _, _) = pool(|_| {}).await;
    let key = h.key(policy("p")).await;
    reset_to(&h, answer(5000), answer(5000)).await;
    assert_eq!(claude_in(&h, &key, "s", first(OPUS, "hello")).await, 200);
    let account = session_row(&h, "p/client", 1).await.lock_account.expect("a lock");
    let mut p = policy("p");
    p.models.insert(OPUS.into(), false);
    put(&h, p).await;

    for _ in 0..2 {
        assert_eq!(claude_in(&h, &key, "s", later(OPUS, "hello")).await, 200);
    }
    let row = session_row(&h, "p/client", 3).await;
    assert_eq!((row.lock_breaks, row.lock_account), (0, Some(account)), "the lock holds");
    let served = tokens(&h).await;
    assert!(served.iter().all(|t| *t == served[0]), "{served:?}");
    assert_eq!(claude_in(&h, &key, "other", first(OPUS, "hello")).await, 404, "a new session can't start with it");

    // Another name is a side request to the lock, and it can still carry the conversation away.
    assert_eq!(claude_in(&h, &key, "s", later(HAIKU, "hello")).await, 200);
    assert_eq!(claude_in(&h, &key, "s", later(OPUS, "hello")).await, 404, "so it can't switch back");
}

#[tokio::test]
async fn a_kept_model_keeps_its_name_when_another_provider_serves_it() {
    let (h, _, _) = pool(|_| {}).await;
    let key = h.key(policy("p")).await;
    reset_to(&h, answer(5), answer(5)).await;
    assert_eq!(claude_in(&h, &key, "s", first("claude-haiku-5-5", "hello")).await, 200);
    put(&h, Policy { rules: vec![Rule::Deny("claude/claude-haiku-*".into())], ..policy("p") }).await;

    assert_eq!(claude_in(&h, &key, "s", later("claude-haiku-5-5", "hello")).await, 200);
    assert_eq!(tokens(&h).await.len(), 2, "both on Claude");
    // A new session gets OpenCode Go's model of that name, which has no account here.
    assert_ne!(claude_in(&h, &key, "new", first("claude-haiku-5-5", "hello")).await, 200);
    assert_eq!(tokens(&h).await.len(), 2);
}
