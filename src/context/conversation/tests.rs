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

/// What the session `s` of `p/k` keeps, as a later request sees it.
async fn kept(db: &Db) -> Vec<(String, String)> {
    conversation(db, "any").kept().await.into_iter().map(|t| (t.provider, t.model)).collect()
}

fn pairs(targets: &[(&str, &str)]) -> Vec<(String, String)> {
    targets.iter().map(|(p, m)| (p.to_string(), m.to_string())).collect()
}

/// What the session `s` of `p/k` keeps for a request of the conversation that opened with `opening`.
async fn kept_for(db: &Db, opening: &str) -> Vec<(String, String)> {
    conversation(db, opening).kept().await.into_iter().map(|t| (t.provider, t.model)).collect()
}

#[tokio::test]
async fn a_session_keeps_each_model_one_of_its_conversations_is_on() {
    let db = Db::in_memory().await.unwrap();
    conversation(&db, "a").served(Some(&target(QWEN))).await;
    conversation(&db, "b").served(Some(&target(QWEN))).await;
    assert_eq!(kept(&db).await, pairs(&[QWEN]));

    conversation(&db, "a").served(Some(&target(BUSY))).await;
    assert_eq!(kept_for(&db, "b").await, pairs(&[BUSY, QWEN]), "b is still on qwen3");
    assert_eq!(kept_for(&db, "a").await, pairs(&[BUSY]), "a left it, and no one has used it since");
    assert_eq!(kept(&db).await, pairs(&[BUSY]), "nor for a new conversation");

    conversation(&db, "b").served(Some(&target(QWEN))).await;
    assert_eq!(kept_for(&db, "a").await, pairs(&[BUSY, QWEN]), "b used it after a left");

    // A target it can't keep ends the one it switched from.
    conversation(&db, "b").served(None).await;
    assert_eq!(kept(&db).await, pairs(&[BUSY]), "no conversation is on qwen3 now");
}

#[tokio::test]
async fn a_compacted_conversation_goes_on_with_its_model_and_cannot_switch_back_once_it_leaves() {
    let db = Db::in_memory().await.unwrap();
    conversation(&db, "hello").served(Some(&target(QWEN))).await;
    conversation(&db, "hello").served(Some(&target(QWEN))).await;
    // After the compaction, the conversation opens with its summary.
    assert_eq!(kept_for(&db, "summary").await, pairs(&[QWEN]), "it goes on with the model");
    conversation(&db, "summary").served(Some(&target(QWEN))).await;
    assert_eq!(kept_for(&db, "summary").await, pairs(&[QWEN]));
    conversation(&db, "summary").served(Some(&target(BUSY))).await;
    assert_eq!(kept_for(&db, "summary").await, pairs(&[BUSY]), "the conversation before the compaction doesn't keep it");
    assert_eq!(kept_for(&db, "a subagent").await, pairs(&[BUSY]));
}

#[tokio::test]
async fn a_compacted_conversation_that_switches_at_once_cannot_switch_back() {
    let db = Db::in_memory().await.unwrap();
    conversation(&db, "hello").served(Some(&target(QWEN))).await;
    conversation(&db, "hello").served(Some(&target(QWEN))).await;
    assert_eq!(kept_for(&db, "summary").await, pairs(&[QWEN]), "its first request could still use the model");
    conversation(&db, "summary").served(Some(&target(BUSY))).await;
    assert_eq!(kept_for(&db, "summary").await, pairs(&[BUSY]), "the conversation before it went quiet before it began");
}

#[tokio::test]
async fn every_request_of_a_named_session_reads_it_and_a_guessed_one_needs_earlier_output() {
    let db = Db::in_memory().await.unwrap();
    conversation(&db, "a").served(Some(&target(QWEN))).await;
    let start = json!({"messages": [{"role": "user", "content": "a new subagent"}]});
    let named = Conversation::of(&db, "p/k", issued(0), &session("s"), &start).unwrap();
    assert_eq!(named.kept().await, vec![target(QWEN)], "a new conversation of the session");

    // Without a session header, the session is the system prompt and the first user turn.
    let opening = json!([{"role": "system", "content": "s"}, {"role": "user", "content": "hello"}]);
    let continued =
        json!([{"role": "system", "content": "s"}, {"role": "user", "content": "hello"}, {"role": "assistant", "content": "a"}]);
    let guessed = |messages: &Value| Conversation::of(&db, "p/k", issued(0), &HeaderMap::new(), &json!({"messages": messages})).unwrap();
    guessed(&opening).served(Some(&target(QWEN))).await;
    assert_eq!(guessed(&opening).kept().await, vec![], "a one-shot request again");
    assert_eq!(guessed(&continued).kept().await, vec![target(QWEN)], "the conversation going on");
}

#[tokio::test]
async fn an_older_request_that_finishes_late_does_not_move_the_conversation_back() {
    let db = Db::in_memory().await.unwrap();
    let older = conversation(&db, "a");
    let newer = conversation(&db, "a");
    newer.served(Some(&target(BUSY))).await;
    older.served(Some(&target(QWEN))).await;
    assert_eq!(kept(&db).await, pairs(&[BUSY]));
    let older = conversation(&db, "a");
    conversation(&db, "a").served(None).await;
    older.served(Some(&target(QWEN))).await;
    assert_eq!(kept(&db).await, vec![], "nor past a target it can't keep");
}

#[tokio::test]
async fn a_key_issued_again_does_not_inherit_and_other_sessions_and_keys_share_nothing() {
    let db = Db::in_memory().await.unwrap();
    conversation(&db, "a").served(Some(&target(QWEN))).await;
    let again = Conversation::of(&db, "p/k", issued(1), &session("s"), &body("a")).unwrap();
    assert_eq!(again.kept().await, vec![], "a key issued again under the name");
    let other_key = Conversation::of(&db, "p/other", issued(0), &session("s"), &body("a")).unwrap();
    assert_eq!(other_key.kept().await, vec![]);
    let other_session = Conversation::of(&db, "p/k", issued(0), &session("t"), &body("a")).unwrap();
    assert_eq!(other_session.kept().await, vec![]);
    let unknown = Conversation::of(&db, "p/k", None, &session("s"), &body("a")).unwrap();
    assert_eq!(unknown.kept().await, vec![], "a key with no issue time is its own issuance");
}

#[tokio::test]
async fn conversations_are_pruned_with_the_sessions() {
    let db = Db::in_memory().await.unwrap();
    conversation(&db, "a").served(Some(&target(QWEN))).await;
    assert_eq!(db.prune(1).await.unwrap(), 0);
    sqlx::query("UPDATE session_conversations SET last_used = ?").bind(now() - 2 * 86_400).execute(&db.pool).await.unwrap();
    assert_eq!(db.prune(1).await.unwrap(), 1);
    assert_eq!(kept(&db).await, vec![]);
}
