//! A session keeps the models it has used past a policy that excludes them.
//! Nothing here waits for accounting: what a request records about its
//! conversation is visible to the next request as soon as it has answered.
// ---
// relationships:
//   tests: context
// ---
use super::*;
use crate::keys::IssueRequest;
use crate::policy::Rule;

mod clients;

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
async fn a_session_keeps_its_model_after_the_policy_excludes_it() {
    for (how, excluding) in excluding_qwen() {
        let h = answering().await;
        let key = h.key(policy("p")).await;
        assert_eq!(chat_in(&h, &key, "s", first("qwen3", "hello")).await, 200);
        put(&h, excluding).await;

        assert_eq!(chat_in(&h, &key, "s", later("qwen3", "hello")).await, 200, "{how}: the conversation goes on");
        assert_eq!(chat_in(&h, &key, "new", first("qwen3", "hello")).await, 404, "{how}: a new session can't start with it");
        assert_eq!(chat_in(&h, &key, "s", later("busy", "hello")).await, 200, "{how}: the session switches");
        assert_eq!(chat_in(&h, &key, "s", later("qwen3", "hello")).await, 200, "{how}: and goes back");
        assert_eq!(chat_in(&h, &key, "s", later("llama/qwen3", "hello")).await, 200, "{how}: under any name");
        assert_eq!(chat_models(&h).await, ["qwen3", "qwen3", "busy", "qwen3", "qwen3"], "{how}");
    }
}

#[tokio::test]
async fn compactions_and_background_requests_go_on_with_the_models_of_the_session() {
    let h = answering().await;
    let key = h.key(policy("p")).await;
    assert_eq!(chat_in(&h, &key, "s", first("qwen3", "hello")).await, 200);
    put(&h, excluding_qwen().remove(0).1).await;

    let summarize = |model: &str, opening: &str| {
        json!({"model": model, "messages": [
            {"role": "user", "content": opening}, {"role": "assistant", "content": "a"}, {"role": "user", "content": "Summarize the conversation."}]})
    };
    assert_eq!(chat_in(&h, &key, "s", summarize("qwen3", "hello")).await, 200, "a compaction");
    assert_eq!(chat_in(&h, &key, "s", later("busy", "Summary: hello")).await, 200, "its first request already on another model");
    assert_eq!(chat_in(&h, &key, "s", summarize("busy", "Summary: hello")).await, 200, "a second compaction");
    assert_eq!(chat_in(&h, &key, "s", later("qwen3", "Summary: summary")).await, 200, "back on the excluded model after it");
    assert_eq!(chat_in(&h, &key, "s", first("qwen3", "title this")).await, 200, "a background request");
    assert_eq!(chat_in(&h, &key, "s", first("qwen3", "a new subagent")).await, 200, "a subagent");
    assert_eq!(chat_in(&h, &key, "new", first("qwen3", "hello")).await, 404, "another session can't start with it");
}

#[tokio::test]
async fn a_model_the_session_never_used_stays_refused() {
    let h = answering().await;
    let key = h.key(policy("p")).await;
    assert_eq!(chat_in(&h, &key, "s", first("qwen3", "hello")).await, 200);
    put(&h, Policy { rules: vec![Rule::Deny("llama/*".into())], ..policy("p") }).await;

    assert_eq!(chat_in(&h, &key, "s", later("qwen3", "hello")).await, 200);
    assert_eq!(chat_in(&h, &key, "s", later("busy", "hello")).await, 404, "the session never used it");
}

#[tokio::test]
async fn a_session_without_a_session_header_keeps_its_model_only_for_requests_that_go_on() {
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
async fn a_rotated_key_keeps_its_sessions_and_a_key_issued_again_does_not() {
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

fn streamed(chunks: &[&str]) -> ResponseTemplate {
    let body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
}

fn streamed_answer() -> ResponseTemplate {
    streamed(&[
        r#"{"id":"c","object":"chat.completion.chunk","created":1,"model":"busy","choices":[{"index":0,"delta":{"role":"assistant","content":"ok"},"finish_reason":null}]}"#,
        r#"{"id":"c","object":"chat.completion.chunk","created":1,"model":"busy","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
        "[DONE]",
    ])
}

fn streamed_failure() -> ResponseTemplate {
    streamed(&[r#"{"error":{"message":"boom","type":"server_error"}}"#, "[DONE]"])
}

fn streaming(body: Value) -> Value {
    let mut body = body;
    body["stream"] = json!(true);
    body
}

#[tokio::test]
async fn a_model_whose_answer_fails_is_not_used() {
    let h = harness().await;
    let key = h.key(policy("p")).await;
    chat_mock(&h, "qwen3", chat_answer(10)).await;
    chat_mock(&h, "busy", streamed_failure()).await;
    assert_eq!(chat_in(&h, &key, "s", first("qwen3", "hello")).await, 200);
    let auth = format!("Bearer {key}");
    let (status, body) = {
        let headers = [("authorization", auth.as_str()), ("session_id", "s")];
        h.call_with("POST", "/v1/chat/completions", &headers, Some(streaming(later("busy", "hello")))).await
    };
    assert_eq!(status, 200, "the provider answered 200 and then an error event");
    assert!(body.to_string().contains("boom"), "{body}");
    // Translated for a Messages client, and through a passthrough.
    let claude = [("authorization", auth.as_str()), ("x-claude-code-session-id", "s")];
    let messages = json!({"model": "busy", "stream": true, "messages": later("busy", "hello")["messages"]});
    let (status, body) = h.call_with("POST", "/v1/messages", &claude, Some(messages)).await;
    assert_eq!(status, 200);
    assert!(body.to_string().contains("event: error"), "{body}");
    let passthrough = [("authorization", auth.as_str()), ("session_id", "s")];
    assert_eq!(h.call_with("POST", "/llama/v1/chat/completions", &passthrough, Some(streaming(later("busy", "hello")))).await.0, 200);

    put(&h, Policy { rules: vec![Rule::Deny("llama/*".into())], ..policy("p") }).await;
    assert_eq!(chat_in(&h, &key, "s", later("qwen3", "hello")).await, 200);
    assert_eq!(chat_in(&h, &key, "s", later("busy", "hello")).await, 404, "no answer from it completed");
}

#[tokio::test]
async fn a_streamed_answer_counts_as_used_before_its_end_reaches_the_client() {
    let h = harness().await;
    let key = h.key(policy("p")).await;
    chat_mock(&h, "busy", streamed_answer()).await;
    assert_eq!(chat_in(&h, &key, "s", streaming(first("busy", "hello"))).await, 200);
    // Straight after the stream ends, with nothing waited for.
    put(&h, Policy { rules: vec![Rule::Deny("llama/busy".into())], ..policy("p") }).await;
    assert_eq!(chat_in(&h, &key, "s", streaming(later("busy", "hello"))).await, 200);
}

#[tokio::test]
async fn a_model_the_policy_never_allowed_is_refused_in_every_session() {
    let h = answering().await;
    let mut p = Policy { rules: vec![Rule::Deny("llama/qwen3".into())], ..policy("p") };
    p.aliases.insert("fast".into(), AliasTargets::One("llama/qwen3".into()));
    let key = h.key(p).await;
    assert_eq!(chat_in(&h, &key, "s", first("qwen3", "hello")).await, 404);
    assert_eq!(chat_in(&h, &key, "s", first("busy", "hello")).await, 200);
    assert_eq!(chat_in(&h, &key, "s", later("qwen3", "hello")).await, 404);

    // An alias reaches it, which doesn't make it the session's to keep.
    assert_eq!(chat_in(&h, &key, "s", later("fast", "hello")).await, 200);
    assert_eq!(chat_in(&h, &key, "s", later("qwen3", "hello")).await, 404);
    assert_eq!(chat_in(&h, &key, "s", later("llama/qwen3", "hello")).await, 404);
    assert_eq!(chat_models(&h).await, ["busy", "qwen3"]);
}

#[tokio::test]
async fn a_session_does_not_keep_a_provider_the_policy_drops() {
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

    let headers = [("authorization", auth.as_str()), ("session_id", "s")];

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
async fn a_locked_session_stays_on_its_excluded_model_and_account() {
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

    // Counting tokens on another model breaks nothing.
    Mock::given(path("/v1/messages/count_tokens"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"input_tokens": 5})))
        .mount(&h.upstream)
        .await;
    let auth = format!("Bearer {key}");
    let headers = [("authorization", auth.as_str()), (SESSION, "s")];
    assert_eq!(h.call_with("POST", "/v1/messages/count_tokens", &headers, Some(later(HAIKU, "hello"))).await.0, 200);
    assert_eq!(claude_in(&h, &key, "s", later(OPUS, "hello")).await, 200);

    // Another name is a side request to the lock; the session can switch to it and back.
    assert_eq!(claude_in(&h, &key, "s", later(HAIKU, "hello")).await, 200);
    assert_eq!(claude_in(&h, &key, "s", later(OPUS, "hello")).await, 200);
}

#[tokio::test]
async fn counting_tokens_on_a_model_is_not_using_it() {
    let (h, _, _) = pool(|_| {}).await;
    let key = h.key(policy("p")).await;
    reset_to(&h, answer(5), answer(5)).await;
    Mock::given(path("/v1/messages/count_tokens"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"input_tokens": 5})))
        .mount(&h.upstream)
        .await;
    assert_eq!(claude_in(&h, &key, "s", first(OPUS, "hello")).await, 200);
    let auth = format!("Bearer {key}");
    let headers = [("authorization", auth.as_str()), (SESSION, "s")];
    assert_eq!(h.call_with("POST", "/v1/messages/count_tokens", &headers, Some(later(HAIKU, "hello"))).await.0, 200);
    let mut p = policy("p");
    p.models.insert(HAIKU.into(), false);
    put(&h, p).await;
    assert_eq!(claude_in(&h, &key, "s", later(HAIKU, "hello")).await, 404, "the session only counted tokens on it");
    assert_eq!(claude_in(&h, &key, "s", later(OPUS, "hello")).await, 200);
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
