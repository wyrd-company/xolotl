//! Each client's own request shapes, as measured on 2026-11-24 with Claude
//! Code 2.1.330, Codex 0.171.0, Grok 1.0.92 and OpenCode 1.19.6 against a
//! mock provider: the session header each sends, a first user turn that stays
//! put across turns and model switches, earlier output on later requests,
//! background requests and subagents with their own first turn, and what a
//! compaction does. The bodies keep only the parts xolotl reads. In each
//! test, the session goes on with an excluded model through all of it.
//! Another session of the same client can't start with it, and once the
//! conversation, compacted, switches away, it can't switch back.
// ---
// relationships:
//   tests: context
// ---
use super::*;

async fn post(h: &Harness, key: &str, path: &str, headers: &[(&str, &str)], body: Value) -> u16 {
    let auth = format!("Bearer {key}");
    let mut all = vec![("authorization", auth.as_str())];
    all.extend_from_slice(headers);
    h.call_with("POST", path, &all, Some(body)).await.0
}

async fn excluded_after_first(h: &Harness) -> String {
    h.key(policy("p")).await
}

fn text(t: &str) -> Value {
    json!([{"type": "text", "text": t}])
}

#[tokio::test]
async fn claude_code() {
    let h = answering().await;
    let key = excluded_after_first(&h).await;
    let session = [("x-claude-code-session-id", "53e1c9ac-1cf4-4ee4-9af0-bf24a3d55ca0")];
    let reminder = || json!({"role": "user", "content": [{"type": "text", "text": "<system-reminder>context</system-reminder>"}, {"type": "text", "text": "hello there"}]});
    // The first turn carries the cache marker on its only user turn.
    let mut first = reminder();
    first["content"][1]["cache_control"] = json!({"type": "ephemeral"});
    let messages = |extra: Vec<Value>| [vec![reminder()], extra].concat();
    let turn = |model: &str, messages: Vec<Value>| json!({"model": model, "stream": true, "messages": messages});
    let said = |t: &str| json!({"role": "assistant", "content": text(t)});
    let asked = |t: &str| json!({"role": "user", "content": text(t)});
    assert_eq!(post(&h, &key, "/v1/messages", &session, turn("qwen3", vec![first])).await, 200);
    put(&h, excluding_qwen().remove(0).1).await;

    let later = messages(vec![said("ok"), asked("again please")]);
    assert_eq!(post(&h, &key, "/v1/messages", &session, turn("qwen3", later.clone())).await, 200, "the next turn");
    let compact = [later.clone(), vec![said("ok"), asked("CRITICAL: Respond with TEXT ONLY. Summarize the conversation.")]].concat();
    assert_eq!(post(&h, &key, "/v1/messages", &session, turn("qwen3", compact)).await, 200, "the compaction");
    let compacted =
        vec![asked("This session is being continued from a previous conversation. Summary: ..."), said("ok"), asked("after compaction")];
    assert_eq!(post(&h, &key, "/v1/messages", &session, turn("qwen3", compacted.clone())).await, 200, "the turn after it");
    let agent = [session[0], ("x-claude-code-agent-id", "aa90a739fc2754e4c")];
    assert_eq!(post(&h, &key, "/v1/messages", &agent, turn("qwen3", vec![asked("Say hi from the subagent.")])).await, 200, "a subagent");
    let switched = vec![compacted[0].clone(), said("ok"), asked("after compaction"), said("ok"), asked("switching model")];
    assert_eq!(post(&h, &key, "/v1/messages", &session, turn("busy", switched.clone())).await, 200, "a switch");
    assert_eq!(post(&h, &key, "/v1/messages", &session, turn("qwen3", switched)).await, 404, "and back");
    let other = [("x-claude-code-session-id", "another-session")];
    assert_eq!(post(&h, &key, "/v1/messages", &other, turn("qwen3", vec![reminder()])).await, 404);
}

#[tokio::test]
async fn codex() {
    let h = answering().await;
    let key = excluded_after_first(&h).await;
    let thread = "01a20e37-1985-71f2-b2d6-8c59c6d66c60";
    let session = [("session-id", thread), ("thread-id", thread)];
    let msg = |role: &str, t: &str| json!({"type": "message", "role": role, "content": [{"type": if role == "assistant" { "output_text" } else { "input_text" }, "text": t}]});
    let opening = || {
        vec![
            msg("developer", "You are a coding agent running in the Codex CLI."),
            msg("developer", "<skills_instructions>"),
            msg("user", "<environment_context><cwd>/work</cwd></environment_context>"),
        ]
    };
    let turn = |model: &str, input: Vec<Value>| json!({"model": model, "stream": true, "prompt_cache_key": thread, "input": input});
    let first = [opening(), vec![msg("user", "hello there")]].concat();
    assert_eq!(post(&h, &key, "/v1/responses", &session, turn("qwen3", first.clone())).await, 200);
    put(&h, excluding_qwen().remove(0).1).await;

    let later = [first, vec![msg("assistant", "ok"), msg("user", "again please")]].concat();
    assert_eq!(post(&h, &key, "/v1/responses", &session, turn("qwen3", later.clone())).await, 200, "the next turn");
    let compact = [later, vec![msg("assistant", "ok"), msg("user", "You are performing a CONTEXT CHECKPOINT COMPACTION.")]].concat();
    assert_eq!(post(&h, &key, "/v1/responses", &session, turn("qwen3", compact)).await, 200, "the compaction");
    // After it, the history is rebuilt: the first user turn moves, and no earlier output is left.
    let compacted = vec![
        msg("developer", "You are a coding agent running in the Codex CLI."),
        msg("user", "hello there"),
        msg("user", "again please"),
        msg("user", "Another language model started to solve this problem..."),
        msg("user", "after compaction"),
    ];
    assert_eq!(post(&h, &key, "/v1/responses", &session, turn("qwen3", compacted.clone())).await, 200, "the turn after it");
    let switched = [compacted, vec![msg("assistant", "ok"), msg("developer", "<model_switch>"), msg("user", "switching model")]].concat();
    assert_eq!(post(&h, &key, "/v1/responses", &session, turn("busy", switched.clone())).await, 200, "a switch");
    assert_eq!(post(&h, &key, "/v1/responses", &session, turn("qwen3", switched)).await, 404, "and back");
    let other = [("session-id", "another-thread"), ("thread-id", "another-thread")];
    let start = [opening(), vec![msg("user", "hello there")]].concat();
    assert_eq!(post(&h, &key, "/v1/responses", &other, turn("qwen3", start)).await, 404);
}

#[tokio::test]
async fn grok() {
    let h = answering().await;
    let key = excluded_after_first(&h).await;
    let id = "112204cb-506c-44e5-9084-a9e8f8856e14";
    let session = [("x-grok-session-id", id), ("x-grok-conv-id", id), ("x-grok-client-identifier", "grok-shell")];
    let msg = |role: &str, t: &str| json!({"type": "message", "role": role, "content": t});
    let opening = || {
        vec![
            msg("system", "You are Grok released by xAI."),
            msg("user", "<user_info> OS Version: linux </user_info>"),
            msg("user", "<system-reminder> workflows </system-reminder>"),
        ]
    };
    let turn =
        |model: &str, cache: &str, input: Vec<Value>| json!({"model": model, "stream": true, "prompt_cache_key": cache, "input": input});
    let first = [opening(), vec![msg("user", "<user_query> hello there </user_query>")]].concat();
    assert_eq!(post(&h, &key, "/v1/responses", &session, turn("qwen3", id, first.clone())).await, 200);
    put(&h, excluding_qwen().remove(0).1).await;

    let later = [first, vec![msg("assistant", "ok"), msg("user", "<user_query> again please </user_query>")]].concat();
    assert_eq!(post(&h, &key, "/v1/responses", &session, turn("qwen3", id, later.clone())).await, 200, "the next turn");
    // A turn summary: the same session, another conversation, on the conversation's model.
    let summary = [("x-grok-session-id", id), ("x-grok-conv-id", "turn-summary-5a491466")];
    let line = vec![msg("system", "Write an ultra-short dashboard line."), msg("user", "<user_message> hello there </user_message>")];
    assert_eq!(post(&h, &key, "/v1/responses", &summary, turn("qwen3", "turn-summary-5a491466", line)).await, 200);
    // A title refresh carries the conversation on its model.
    let title = [("x-grok-session-id", id), ("x-grok-conv-id", "title-refresh-fcb31d47")];
    let refresh =
        [later.clone(), vec![msg("assistant", "ok"), msg("user", "<system-reminder>Generate a session title</system-reminder>")]].concat();
    assert_eq!(post(&h, &key, "/v1/responses", &title, turn("qwen3", id, refresh)).await, 200);
    // After a compaction the first user turn stays, and no earlier output is left.
    let compacted = [
        opening(),
        vec![
            msg("user", "This session is being continued from a previous conversation."),
            msg("user", "<user_query> after compaction </user_query>"),
        ],
    ]
    .concat();
    assert_eq!(post(&h, &key, "/v1/responses", &session, turn("qwen3", id, compacted.clone())).await, 200, "the turn after a compaction");
    let summary_after =
        vec![msg("system", "Write an ultra-short dashboard line."), msg("user", "<user_message> after compaction </user_message>")];
    assert_eq!(post(&h, &key, "/v1/responses", &summary, turn("qwen3", "turn-summary-89267f56", summary_after)).await, 200);
    let switched = [compacted, vec![msg("assistant", "ok"), msg("user", "<user_query> switching model </user_query>")]].concat();
    assert_eq!(post(&h, &key, "/v1/responses", &session, turn("busy", id, switched.clone())).await, 200, "a switch");
    assert_eq!(
        post(&h, &key, "/v1/responses", &session, turn("qwen3", id, switched)).await,
        404,
        "and back: the turn summaries came before it"
    );
    let other = [("x-grok-session-id", "another-session"), ("x-grok-conv-id", "another-session")];
    let start = [opening(), vec![msg("user", "<user_query> hello there </user_query>")]].concat();
    assert_eq!(post(&h, &key, "/v1/responses", &other, turn("qwen3", "another-session", start)).await, 404);
}

#[tokio::test]
async fn opencode() {
    let h = answering().await;
    let key = excluded_after_first(&h).await;
    let id = "ses_df1c75960ffesHJ5syizK0ftz6";
    let session = [("x-opencode-session-id", id), ("x-session-id", id), ("x-session-affinity", id)];
    let msg = |role: &str, t: &str| json!({"role": role, "content": t});
    let turn = |model: &str, messages: Vec<Value>| json!({"model": model, "stream": true, "messages": messages});
    let first = vec![msg("system", "You are opencode, an interactive CLI tool."), msg("user", "\"hello there\"")];
    // The title is generated beside the first turn, on the small model.
    let title = vec![
        msg("system", "You are a title generator."),
        msg("user", "Generate a title for this conversation: "),
        msg("user", "\"hello there\""),
    ];
    assert_eq!(post(&h, &key, "/v1/chat/completions", &session, turn("busy", title)).await, 200);
    assert_eq!(post(&h, &key, "/v1/chat/completions", &session, turn("qwen3", first.clone())).await, 200);
    put(&h, excluding_qwen().remove(0).1).await;

    let later = [first, vec![msg("assistant", "ok"), msg("user", "again please")]].concat();
    assert_eq!(post(&h, &key, "/v1/chat/completions", &session, turn("qwen3", later)).await, 200, "the next turn");
    // A compaction starts over from a summary: a new first user turn.
    let compact = vec![msg("system", "You are opencode."), msg("user", "Summarize the conversation so far.")];
    assert_eq!(post(&h, &key, "/v1/chat/completions", &session, turn("qwen3", compact)).await, 200, "the compaction");
    let compacted = vec![
        msg("system", "You are opencode."),
        msg("user", "What did we do so far? ..."),
        msg("assistant", "ok"),
        msg("user", "after compaction"),
    ];
    assert_eq!(post(&h, &key, "/v1/chat/completions", &session, turn("qwen3", compacted.clone())).await, 200, "the turn after it");
    let switched = [compacted, vec![msg("assistant", "ok"), msg("user", "switching model")]].concat();
    assert_eq!(post(&h, &key, "/v1/chat/completions", &session, turn("busy", switched.clone())).await, 200, "a switch");
    assert_eq!(post(&h, &key, "/v1/chat/completions", &session, turn("qwen3", switched)).await, 404, "and back");
    let other = [("x-opencode-session-id", "ses_another")];
    let start = vec![msg("system", "You are opencode, an interactive CLI tool."), msg("user", "\"hello there\"")];
    assert_eq!(post(&h, &key, "/v1/chat/completions", &other, turn("qwen3", start)).await, 404);
}
