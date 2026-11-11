//! Anthropic and OpenAI API accounts at the route: the endpoints a Claude or
//! Codex subscription serves, policy and alias fallback, usage.
// ---
// relationships:
//   tests: accounts
// ---
use super::*;

/// `anth` (Anthropic API) and `oai` (OpenAI API) provider accounts on the mock server.
async fn api_harness() -> Harness {
    let secrets: Secrets = Arc::new(MemoryStore::default());
    secrets.put("providers/anth", json!({"api_key": "anth-key"}).as_object().unwrap().clone()).await.unwrap();
    secrets.put("providers/oai", json!({"api_key": "oai-key"}).as_object().unwrap().clone()).await.unwrap();
    harness_with_secrets(
        |cfg| {
            let uri = cfg.subscription.endpoints.claude_api.clone();
            let account = |kind, base: String| ProviderConfig { kind, base_url: Some(base), ..Default::default() };
            cfg.gateway.providers.insert("anth".into(), account(ProviderKind::Anthropic, format!("{uri}/anth")));
            cfg.gateway.providers.insert("oai".into(), account(ProviderKind::Openai, format!("{uri}/oai/v1")));
            cfg.catalog.static_models.insert("anth".into(), vec!["claude-haiku-5-5".into()]);
            cfg.catalog.static_models.insert("oai".into(), vec!["gpt-6".into()]);
        },
        secrets,
    )
    .await
}

fn message(text: &str) -> Value {
    json!({"id": "msg_1", "type": "message", "role": "assistant", "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn", "usage": {"input_tokens": 7, "output_tokens": 3, "cache_read_input_tokens": 5}})
}

#[tokio::test]
async fn anthropic_api_accounts_serve_the_claude_code_path() {
    let h = api_harness().await;
    h.account(Credential {
        provider: SubProvider::Claude,
        access_token: "claude-tok".into(),
        expires_at: Some(now() + 3600),
        account_id: Some("u1".into()),
        ..Default::default()
    })
    .await;
    // The subscription is at its limit, so the alias moves on to the API account.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "60")
                .set_body_json(json!({"type": "error", "error": {"type": "rate_limit_error", "message": "limit"}})),
        )
        .mount(&h.upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/anth/v1/messages"))
        .and(header("x-api-key", "anth-key"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(ResponseTemplate::new(200).set_body_json(message("hello")))
        .mount(&h.upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/anth/v1/messages/count_tokens"))
        .and(header("x-api-key", "anth-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"input_tokens": 9})))
        .mount(&h.upstream)
        .await;
    let mut p = policy("p");
    p.aliases.insert("haiku".into(), AliasTargets::Many(vec!["claude/claude-haiku-5-5".into(), "anth/claude-haiku-5-5".into()]));
    let key = h.key(p).await;

    // A Claude Code request, with a cache breakpoint and its betas.
    let system = json!([{"type": "text", "text": "long prompt", "cache_control": {"type": "ephemeral", "ttl": "1h"}}]);
    let body = json!({"model": "haiku", "max_tokens": 8, "system": system, "betas": ["context-1m-2025-08-07"],
        "messages": [{"role": "user", "content": "hi"}]});
    let auth = format!("Bearer {key}");
    let headers = [("authorization", auth.as_str()), ("anthropic-beta", "oauth-2025-04-20,interleaved-thinking-2025-05-14")];
    let (status, out) = h.call_with("POST", "/v1/messages", &headers, Some(body)).await;
    assert_eq!(status, 200, "{out}");
    assert_eq!(out["content"][0]["text"], "hello");

    let reqs = h.upstream.received_requests().await.unwrap();
    let sent = reqs.iter().find(|r| r.url.path() == "/anth/v1/messages").unwrap();
    assert!(sent.headers.get("authorization").is_none(), "the key goes in x-api-key only");
    assert_eq!(sent.headers["anthropic-beta"], "interleaved-thinking-2025-05-14,context-1m-2025-08-07", "no OAuth beta");
    let sent: Value = serde_json::from_slice(&sent.body).unwrap();
    assert_eq!((sent["model"].clone(), sent["system"].clone()), (json!("claude-haiku-5-5"), system), "cache breakpoints are kept");
    assert!(sent.get("betas").is_none());

    // Token counting is native; other formats are translated to Messages.
    let count = json!({"model": "anth/claude-haiku-5-5", "messages": [{"role": "user", "content": "hi"}]});
    let (status, out) = h.call("POST", "/v1/messages/count_tokens", Some(&key), Some(count)).await;
    assert_eq!((status, out["input_tokens"].clone()), (200, json!(9)), "{out}");
    let chat = json!({"model": "anth/claude-haiku-5-5", "messages": [{"role": "user", "content": "hi"}]});
    let (status, out) = h.call("POST", "/v1/chat/completions", Some(&key), Some(chat)).await;
    assert_eq!(status, 200, "{out}");
    assert_eq!(out["choices"][0]["message"]["content"], "hello");

    // Usage is recorded against the account, cached tokens included.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let rows = h.state.db.usage_summary("provider", 0, &Default::default()).await.unwrap();
    let row = rows.iter().find(|r| r.key.as_deref() == Some("anth")).unwrap_or_else(|| panic!("{rows:?}"));
    assert_eq!((row.input_tokens, row.output_tokens, row.cached_tokens), (24, 6, 10));

    // A policy that leaves the account out can't reach it.
    let restricted = h.key(Policy { name: "r".into(), providers: vec!["claude".into()], ..Default::default() }).await;
    let body = json!({"model": "anth/claude-haiku-5-5", "max_tokens": 8, "messages": [{"role": "user", "content": "hi"}]});
    assert_eq!(h.call("POST", "/v1/messages", Some(&restricted), Some(body)).await.0, 404);
}

#[tokio::test]
async fn openai_api_accounts_serve_responses_and_chat() {
    let h = api_harness().await;
    Mock::given(method("POST"))
        .and(path("/oai/v1/responses"))
        .and(header("authorization", "Bearer oai-key"))
        .and(body_partial_json(json!({"model": "gpt-6", "prompt_cache_key": "session-1"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "resp_1", "object": "response", "status": "completed", "model": "gpt-6",
            "output": [{"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "hi"}]}],
            "usage": {"input_tokens": 4, "output_tokens": 2, "input_tokens_details": {"cached_tokens": 3}},
        })))
        .mount(&h.upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/oai/v1/chat/completions"))
        .and(header("authorization", "Bearer oai-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 4, "completion_tokens": 1},
        })))
        .mount(&h.upstream)
        .await;
    let key = h.key(policy("p")).await;

    let body = json!({"model": "gpt-6", "input": "hi", "prompt_cache_key": "session-1"});
    let (status, out) = h.call("POST", "/v1/responses", Some(&key), Some(body)).await;
    assert_eq!(status, 200, "{out}");
    assert_eq!(out["output"][0]["content"][0]["text"], "hi");
    let chat = json!({"model": "gpt-6", "messages": [{"role": "user", "content": "hi"}]});
    let (status, out) = h.call("POST", "/v1/chat/completions", Some(&key), Some(chat)).await;
    assert_eq!(status, 200, "{out}");

    // Both natively; Messages through translation.
    let (_, models) = h.call("GET", "/v1/models", Some(&key), None).await;
    let gpt = models["data"].as_array().unwrap().iter().find(|m| m["id"] == "gpt-6").unwrap().clone();
    assert_eq!(gpt["endpoints"], json!(["/v1/responses", "/v1/chat/completions"]));
    assert_eq!(gpt["translated_endpoints"], json!(["/v1/messages"]));
}
