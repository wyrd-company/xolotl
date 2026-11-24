// ---
// relationships:
//   tests: context
// ---
use chrono::TimeZone;
use serde_json::json;

use super::*;

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

const QWEN: (&str, &str) = ("llama", "qwen3");
const BUSY: (&str, &str) = ("llama", "busy");

fn target((provider, model): (&str, &str)) -> Target {
    Target { provider: provider.into(), model: model.into() }
}

fn later(opening: &str) -> Value {
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
fn in_session(db: &Db, body: &Value) -> SessionModels {
    SessionModels::of(db, "p/k", issued(0), &session("s"), body).unwrap()
}

async fn kept(db: &Db) -> Vec<(String, String)> {
    in_session(db, &later("any")).kept().await.into_iter().map(|t| (t.provider, t.model)).collect()
}

fn pairs(targets: &[(&str, &str)]) -> Vec<(String, String)> {
    targets.iter().map(|(p, m)| (p.to_string(), m.to_string())).collect()
}

#[tokio::test]
async fn a_session_keeps_every_model_it_has_used() {
    let db = Db::in_memory().await.unwrap();
    assert_eq!(kept(&db).await, vec![]);
    in_session(&db, &later("hello")).record(&target(QWEN)).await;
    in_session(&db, &later("hello")).record(&target(BUSY)).await;
    in_session(&db, &later("hello")).record(&target(QWEN)).await;
    assert_eq!(kept(&db).await, pairs(&[BUSY, QWEN]), "both, after a switch away and back");
}

#[tokio::test]
async fn every_request_of_a_named_session_reads_it_and_a_guessed_one_needs_earlier_output() {
    let db = Db::in_memory().await.unwrap();
    in_session(&db, &later("hello")).record(&target(QWEN)).await;
    let start = json!({"messages": [{"role": "user", "content": "a new subagent"}]});
    assert_eq!(in_session(&db, &start).kept().await, vec![target(QWEN)], "a new conversation of the session");

    // Without a session header, the session is the system prompt and the first user turn.
    let opening = json!([{"role": "system", "content": "s"}, {"role": "user", "content": "hello"}]);
    let continued =
        json!([{"role": "system", "content": "s"}, {"role": "user", "content": "hello"}, {"role": "assistant", "content": "a"}]);
    let guessed = |messages: &Value| SessionModels::of(&db, "p/k", issued(0), &HeaderMap::new(), &json!({"messages": messages})).unwrap();
    guessed(&opening).record(&target(QWEN)).await;
    assert_eq!(guessed(&opening).kept().await, vec![], "a one-shot request again");
    assert_eq!(guessed(&continued).kept().await, vec![target(QWEN)], "the conversation going on");
}

#[tokio::test]
async fn a_key_issued_again_does_not_inherit_and_other_sessions_and_keys_share_nothing() {
    let db = Db::in_memory().await.unwrap();
    in_session(&db, &later("a")).record(&target(QWEN)).await;
    let again = SessionModels::of(&db, "p/k", issued(1), &session("s"), &later("a")).unwrap();
    assert_eq!(again.kept().await, vec![], "a key issued again under the name");
    let other_key = SessionModels::of(&db, "p/other", issued(0), &session("s"), &later("a")).unwrap();
    assert_eq!(other_key.kept().await, vec![]);
    let other_session = SessionModels::of(&db, "p/k", issued(0), &session("t"), &later("a")).unwrap();
    assert_eq!(other_session.kept().await, vec![]);
    let unknown = SessionModels::of(&db, "p/k", None, &session("s"), &later("a")).unwrap();
    assert_eq!(unknown.kept().await, vec![], "a key with no issue time is its own issuance");
}

#[tokio::test]
async fn the_models_of_a_session_are_pruned_with_it() {
    let db = Db::in_memory().await.unwrap();
    in_session(&db, &later("a")).record(&target(QWEN)).await;
    assert_eq!(db.prune(1).await.unwrap(), 0);
    sqlx::query("UPDATE session_models SET last_used = ?").bind(now() - 2 * 86_400).execute(&db.pool).await.unwrap();
    assert_eq!(db.prune(1).await.unwrap(), 1);
    assert_eq!(kept(&db).await, vec![]);
}
