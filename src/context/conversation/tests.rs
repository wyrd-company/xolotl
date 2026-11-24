// ---
// relationships:
//   tests: context
// ---
use chrono::TimeZone;
use serde_json::json;

use super::*;

#[test]
fn the_opening_follows_a_conversation_across_models() {
    let marked = json!([{"type": "text", "text": "hello", "cache_control": {"type": "ephemeral"}}]);
    let unmarked = json!([{"type": "text", "text": "hello"}]);
    let turn1 = json!({"model": "a", "system": "s1", "messages": [{"role": "user", "content": marked}]});
    let turn2 = json!({"model": "b", "system": "s2", "messages": [
        {"role": "user", "content": unmarked}, {"role": "assistant", "content": "hi"}, {"role": "user", "content": "more"}]});
    let other = json!({"model": "a", "system": "s1", "messages": [{"role": "user", "content": "title this: hello"}]});
    let first = opening(&turn1).unwrap();
    assert_eq!(Some(&first), opening(&turn2).as_ref(), "the model, the system prompt and cache markers aside");
    assert_ne!(Some(first), opening(&other), "a background request has its own");
    assert_eq!(opening(&json!({"messages": [{"role": "assistant", "content": "x"}]})), None);
    for body in [json!({"input": "a"}), json!({"input": [{"role": "developer", "content": "d"}, {"role": "user", "content": "a"}]})] {
        assert!(opening(&body).is_some(), "{body}");
    }
}

#[test]
fn a_request_continues_a_conversation_when_it_carries_earlier_output() {
    let first = [
        json!({"messages": [{"role": "system", "content": "s"}, {"role": "user", "content": "a"}]}),
        json!({"input": "a"}),
        json!({"input": [{"role": "developer", "content": "d"}, {"role": "user", "content": "a"}, {"role": "user", "content": "b"}]}),
        json!({"input": [{"type": "function_call_output", "call_id": "c", "output": "x"}]}),
    ];
    for body in first {
        assert!(!continues(&body), "{body}");
    }
    let later = [
        json!({"messages": [{"role": "user", "content": "a"}, {"role": "assistant", "content": "b"}]}),
        json!({"input": [{"role": "user", "content": "a"}, {"type": "message", "role": "assistant", "content": []}]}),
        json!({"input": [{"role": "user", "content": "a"}, {"type": "function_call", "call_id": "c", "name": "f"}]}),
        json!({"input": [{"role": "user", "content": "a"}, {"type": "reasoning", "summary": []}]}),
        json!({"input": [{"role": "user", "content": "a"}, {"type": "compaction", "encrypted_content": "e"}]}),
    ];
    for body in later {
        assert!(continues(&body), "{body}");
    }
}

#[test]
fn sequence_numbers_only_grow() {
    let mut last = sequence();
    for _ in 0..1000 {
        let next = sequence();
        assert!(next > last);
        last = next;
    }
    assert!(last >= Utc::now().timestamp_micros() - 1_000_000, "they follow the clock");
}

const QWEN: (&str, &str) = ("llama", "qwen3");
const BUSY: (&str, &str) = ("llama", "busy");

fn target((provider, model): (&str, &str)) -> Target {
    Target { provider: provider.into(), model: model.into() }
}

fn body(opening: &str) -> Value {
    json!({"messages": [{"role": "user", "content": opening}, {"role": "assistant", "content": "a"}, {"role": "user", "content": "go on"}]})
}

fn issued(second: u32) -> Option<DateTime<Utc>> {
    Some(Utc.with_ymd_and_hms(2026, 11, 24, 0, 0, second).unwrap())
}

fn session(id: &str) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert("session_id", id.parse().unwrap());
    h
}

/// A request of the key `p/k` issued at `issued(0)`, in session `s`.
fn conversation(db: &Db, opening: &str) -> Conversation {
    Conversation::of(db, "p/k", issued(0), &session("s"), &body(opening)).unwrap()
}

async fn on(db: &Db, opening: &str) -> Option<(String, String)> {
    conversation(db, opening).kept().await.map(|t| (t.provider, t.model))
}

fn pair((provider, model): (&str, &str)) -> Option<(String, String)> {
    Some((provider.into(), model.into()))
}

#[tokio::test]
async fn each_conversation_is_on_its_own_model() {
    let db = Db::in_memory().await.unwrap();
    conversation(&db, "a").served(Some(&target(QWEN))).await;
    conversation(&db, "b").served(Some(&target(QWEN))).await;
    conversation(&db, "a").served(Some(&target(BUSY))).await;
    assert_eq!(on(&db, "a").await, pair(BUSY), "a switched");
    assert_eq!(on(&db, "b").await, pair(QWEN), "b is still on its model");
    assert_eq!(on(&db, "c").await, None, "a conversation that never ran keeps nothing");

    // A target it can't keep ends the one it switched from.
    conversation(&db, "b").served(None).await;
    assert_eq!(on(&db, "b").await, None);
}

#[tokio::test]
async fn a_request_that_starts_a_conversation_keeps_nothing() {
    let db = Db::in_memory().await.unwrap();
    conversation(&db, "a").served(Some(&target(QWEN))).await;
    let start = Conversation::of(&db, "p/k", issued(0), &session("s"), &json!({"messages": [{"role": "user", "content": "a"}]})).unwrap();
    assert_eq!(start.kept().await, None);
    assert_eq!(on(&db, "a").await, pair(QWEN), "the conversation is still on it");
}

#[tokio::test]
async fn an_older_request_that_finishes_late_does_not_move_the_conversation_back() {
    let db = Db::in_memory().await.unwrap();
    let older = conversation(&db, "a");
    let newer = conversation(&db, "a");
    newer.served(Some(&target(BUSY))).await;
    older.served(Some(&target(QWEN))).await;
    assert_eq!(on(&db, "a").await, pair(BUSY));
    let older = conversation(&db, "a");
    conversation(&db, "a").served(None).await;
    older.served(Some(&target(QWEN))).await;
    assert_eq!(on(&db, "a").await, None, "nor past a target it can't keep");
}

#[tokio::test]
async fn a_key_issued_again_does_not_inherit_and_other_sessions_and_keys_share_nothing() {
    let db = Db::in_memory().await.unwrap();
    conversation(&db, "a").served(Some(&target(QWEN))).await;
    let again = Conversation::of(&db, "p/k", issued(1), &session("s"), &body("a")).unwrap();
    assert_eq!(again.kept().await, None, "a key issued again under the name");
    let other_key = Conversation::of(&db, "p/other", issued(0), &session("s"), &body("a")).unwrap();
    assert_eq!(other_key.kept().await, None);
    let other_session = Conversation::of(&db, "p/k", issued(0), &session("t"), &body("a")).unwrap();
    assert_eq!(other_session.kept().await, None);
    let unknown = Conversation::of(&db, "p/k", None, &session("s"), &body("a")).unwrap();
    assert_eq!(unknown.kept().await, None, "a key with no issue time is its own issuance");
}

#[tokio::test]
async fn conversations_are_pruned_with_the_sessions() {
    let db = Db::in_memory().await.unwrap();
    conversation(&db, "a").served(Some(&target(QWEN))).await;
    assert_eq!(db.prune(1).await.unwrap(), 0);
    sqlx::query("UPDATE session_conversations SET last_used = ?").bind(now() - 2 * 86_400).execute(&db.pool).await.unwrap();
    assert_eq!(db.prune(1).await.unwrap(), 1);
    assert_eq!(on(&db, "a").await, None);
}
