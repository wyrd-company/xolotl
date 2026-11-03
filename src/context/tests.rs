// ---
// relationships:
//   tests: context
// ---
use serde_json::json;

use super::*;
use crate::db::{McpCallRecord, now};

fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
    let mut h = HeaderMap::new();
    for (k, v) in pairs {
        h.insert(*k, v.parse().unwrap());
    }
    h
}

fn source(pairs: &[(&'static str, &str)], body: Value) -> Option<&'static str> {
    resolve(&headers(pairs), &body).map(|s| s.source)
}

#[test]
fn identity_comes_from_each_client_signal_in_order() {
    let body = json!({"prompt_cache_key": "pck", "messages": [{"role": "user", "content": "x"}]});
    assert_eq!(source(&[("x-claude-code-session-id", "a"), ("session_id", "b")], body.clone()), Some("claude-code"));
    let metadata = json!({"metadata": {"user_id": "{\"session_id\":\"s\"}"}, "prompt_cache_key": "pck"});
    assert_eq!(source(&[], metadata), Some("claude-code"));
    assert_eq!(source(&[("session_id", "b"), ("x-grok-conv-id", "c")], body.clone()), Some("codex"));
    assert_eq!(source(&[("thread-id", "b")], body.clone()), Some("codex"));
    assert_eq!(source(&[("x-grok-session-id", "c"), ("x-opencode-session", "d")], body.clone()), Some("grok"));
    assert_eq!(source(&[("x-opencode-session", "d")], body.clone()), Some("opencode"));
    assert_eq!(source(&[], body.clone()), Some("prompt-cache-key"));
    // A blank signal is no signal.
    assert_eq!(source(&[("session_id", "  ")], body), Some("prompt-cache-key"));
}

#[test]
fn the_content_fallback_follows_a_conversation_and_tells_two_apart() {
    let turn1 = json!({"system": "be brief", "messages": [{"role": "user", "content": "hello"}]});
    let turn2 = json!({"system": "be brief", "messages": [
        {"role": "user", "content": "hello"}, {"role": "assistant", "content": "hi"}, {"role": "user", "content": "more"}]});
    let other = json!({"system": "be brief", "messages": [{"role": "user", "content": "goodbye"}]});
    let other_system = json!({"system": "be long", "messages": [{"role": "user", "content": "hello"}]});
    let id = resolve(&HeaderMap::new(), &turn1).unwrap();
    assert_eq!(id.source, "content");
    assert_eq!(id, resolve(&HeaderMap::new(), &turn2).unwrap());
    assert_ne!(id, resolve(&HeaderMap::new(), &other).unwrap());
    assert_ne!(id, resolve(&HeaderMap::new(), &other_system).unwrap());
}

#[test]
fn the_content_fallback_reads_every_wire_format() {
    let responses = json!({"instructions": "i", "input": [{"role": "user", "content": "a"}]});
    let responses_string = json!({"input": "a"});
    let responses_developer = json!({"input": [{"role": "developer", "content": "d"}, {"role": "user", "content": "a"}]});
    let chat = json!({"messages": [{"role": "system", "content": "s"}, {"role": "user", "content": "a"}]});
    for body in [responses, responses_string, responses_developer, chat] {
        assert_eq!(source(&[], body.clone()), Some("content"), "{body}");
    }
    let chat_one = json!({"messages": [{"role": "system", "content": "s1"}, {"role": "user", "content": "a"}]});
    let chat_two = json!({"messages": [{"role": "system", "content": "s2"}, {"role": "user", "content": "a"}]});
    assert_ne!(resolve(&HeaderMap::new(), &chat_one), resolve(&HeaderMap::new(), &chat_two));
    let developer_one = json!({"input": [{"role": "developer", "content": "d1"}, {"role": "user", "content": "a"}]});
    let developer_two = json!({"input": [{"role": "developer", "content": "d2"}, {"role": "user", "content": "a"}]});
    assert_ne!(resolve(&HeaderMap::new(), &developer_one), resolve(&HeaderMap::new(), &developer_two));
}

#[test]
fn a_body_without_a_conversation_has_no_session() {
    assert_eq!(resolve(&HeaderMap::new(), &json!({"model": "m"})), None);
    assert_eq!(resolve(&HeaderMap::new(), &json!({"messages": [{"role": "assistant", "content": "x"}]})), None);
}

#[test]
fn a_key_holds_neither_the_sent_value_nor_the_prompt() {
    let sent = resolve(&headers(&[("session_id", "sample-session-value")]), &json!({})).unwrap();
    assert!(!sent.key.contains("sample-session-value"));
    assert_eq!(sent.key.len(), 32);
    // The same value from two sources is two sessions.
    let other = resolve(&headers(&[("x-grok-conv-id", "sample-session-value")]), &json!({})).unwrap();
    assert_ne!(sent.key, other.key);
}

#[test]
fn the_estimate_reads_every_wire_format() {
    let text = "x".repeat(300);
    for body in [json!({"instructions": text}), json!({"input": text}), json!({"system": text}), json!({"messages": text})] {
        assert!(crate::metadata::estimate_tokens(&body) >= 100, "{body}");
    }
}

fn turn(tokens: i64, reported: bool) -> Turn {
    Turn {
        client_id: "p/k".into(),
        session_key: "s".into(),
        source: "codex".into(),
        tokens,
        reported,
        prompt_tokens: if reported { tokens } else { 0 },
        window: Some(1000),
        provider: "codex".into(),
        model: "m".into(),
        account: Some("a".into()),
        ..Default::default()
    }
}

async fn row(db: &Db) -> SessionRow {
    db.sessions(Some("p/k"), 10).await.unwrap().remove(0)
}

#[tokio::test]
async fn turns_track_current_peak_and_cache() {
    let db = Db::in_memory().await.unwrap();
    assert_eq!(db.record_turn(&Turn { cached_tokens: 50, ..turn(100, true) }).await.unwrap(), 100);
    assert_eq!(db.record_turn(&Turn { cached_tokens: 150, ..turn(300, true) }).await.unwrap(), 300);
    assert_eq!(db.record_turn(&turn(200, true)).await.unwrap(), 200);
    let r = row(&db).await;
    assert_eq!((r.turns, r.current_tokens, r.peak_tokens), (3, 200, 300));
    assert_eq!((r.prompt_tokens, r.cached_tokens), (600, 200));
    assert_eq!(r.context_window, Some(1000));
}

#[tokio::test]
async fn an_estimate_stops_counting_once_an_upstream_has_reported() {
    let db = Db::in_memory().await.unwrap();
    assert_eq!(db.record_turn(&turn(900, false)).await.unwrap(), 900, "no report yet: the estimate counts");
    assert_eq!(db.record_turn(&turn(100, true)).await.unwrap(), 100, "a report replaces it");
    assert_eq!(db.record_turn(&turn(5000, false)).await.unwrap(), 100, "a later estimate doesn't");
    let r = row(&db).await;
    assert_eq!((r.current_tokens, r.turns), (100, 3));
    // The first estimate remains the peak: it is what the session showed then.
    assert_eq!(r.peak_tokens, 900);
}

#[tokio::test]
async fn a_side_request_leaves_the_context_figures_alone() {
    let db = Db::in_memory().await.unwrap();
    db.record_turn(&turn(500, true)).await.unwrap();
    let side = Turn { side: true, model: "small".into(), provider: "other".into(), window: Some(9), ..turn(10, true) };
    assert_eq!(db.record_turn(&side).await.unwrap(), 500);
    let r = row(&db).await;
    assert_eq!((r.current_tokens, r.peak_tokens, r.turns), (500, 500, 2));
    assert_eq!((r.model.as_deref(), r.provider.as_deref(), r.context_window), (Some("m"), Some("codex"), Some(1000)));
}

fn lock(account: &str) -> Lock {
    Lock { name: "n".into(), provider: "codex".into(), model: "m".into(), account: Some(account.into()) }
}

#[tokio::test]
async fn a_lock_has_one_owner() {
    let db = Db::in_memory().await.unwrap();
    db.record_turn(&turn(500, true)).await.unwrap();
    assert!(db.form_lock("p/k", "s", &lock("a")).await.unwrap());
    assert!(!db.form_lock("p/k", "s", &lock("b")).await.unwrap(), "the first holds it");
    assert_eq!(db.session_lock("p/k", "s").await.unwrap(), Some(lock("a")));
    assert_eq!(db.session_lock("q/k", "s").await.unwrap(), None, "client keys don't share sessions");

    // A stale attempt can neither release nor count on a lock it doesn't hold.
    assert!(!db.break_lock("p/k", "s", &lock("b"), reason::COOLDOWN).await.unwrap());
    assert_eq!(db.count_lock_failure("p/k", "s", &lock("b")).await.unwrap(), 0);
    assert_eq!(db.session_lock("p/k", "s").await.unwrap(), Some(lock("a")));
    assert_eq!(row(&db).await.lock_breaks, 0);

    assert!(db.break_lock("p/k", "s", &lock("a"), reason::COOLDOWN).await.unwrap());
    let r = row(&db).await;
    assert_eq!((r.lock_breaks, r.last_break_reason.as_deref()), (1, Some("cooldown")));
    assert!(r.last_break_at.is_some());
    assert_eq!(db.session_lock("p/k", "s").await.unwrap(), None);
    assert!(db.form_lock("p/k", "s", &lock("b")).await.unwrap(), "a new lock forms after a break");
}

#[tokio::test]
async fn a_success_on_the_lock_ends_a_run_of_failures() {
    let db = Db::in_memory().await.unwrap();
    db.record_turn(&turn(500, true)).await.unwrap();
    db.form_lock("p/k", "s", &lock("a")).await.unwrap();
    assert_eq!(db.count_lock_failure("p/k", "s", &lock("a")).await.unwrap(), 1);
    assert_eq!(db.count_lock_failure("p/k", "s", &lock("a")).await.unwrap(), 2);
    db.record_turn(&Turn { account: Some("b".into()), ..turn(500, true) }).await.unwrap();
    assert_eq!(row(&db).await.lock_failures, 2, "another account's success isn't the lock's");
    db.record_turn(&turn(500, true)).await.unwrap();
    assert_eq!(row(&db).await.lock_failures, 0);
}

#[tokio::test]
async fn tool_usage_joins_llm_and_mcp_calls_per_key() {
    let db = Db::in_memory().await.unwrap();
    db.count_tools("p/k", &["read".to_string(), "read".to_string(), "write".to_string()]).await.unwrap();
    db.count_tools("p/other", &["read".to_string()]).await.unwrap();
    for client in [Some("p/k"), Some("p/k"), None] {
        let call = McpCallRecord {
            client_id: client.map(str::to_string),
            server: "srv".into(),
            tool: "lookup".into(),
            status: "ok".into(),
            ..Default::default()
        };
        db.log_mcp_call(&call).await.unwrap();
    }
    let rows = db.tool_usage(Some("p/k")).await.unwrap();
    let got: Vec<(&str, &str, i64)> = rows.iter().map(|r| (r.tool.as_str(), r.source.as_str(), r.calls)).collect();
    assert_eq!(got, [("read", "llm", 2), ("srv/lookup", "mcp", 2), ("write", "llm", 1)]);
    assert_eq!(db.tool_usage(None).await.unwrap().len(), 4, "every key, and no row for the admin token's MCP call");
}

#[tokio::test]
async fn prune_removes_old_sessions_and_tool_counts_with_the_request_log() {
    let db = Db::in_memory().await.unwrap();
    db.record_turn(&turn(1, true)).await.unwrap();
    db.count_tools("p/k", &["read".to_string()]).await.unwrap();
    assert_eq!(db.prune(1).await.unwrap(), 0, "recent rows stay");
    assert_eq!(db.sessions(None, 10).await.unwrap().len(), 1);
    assert_eq!(db.tool_usage(None).await.unwrap().len(), 1);

    let old = now() - 2 * 86_400;
    sqlx::query("UPDATE sessions SET last_seen = ?").bind(old).execute(&db.pool).await.unwrap();
    sqlx::query("UPDATE tool_usage SET last_used = ?").bind(old).execute(&db.pool).await.unwrap();
    assert_eq!(db.prune(1).await.unwrap(), 2);
    assert!(db.sessions(None, 10).await.unwrap().is_empty());
    assert!(db.tool_usage(None).await.unwrap().is_empty());
}
