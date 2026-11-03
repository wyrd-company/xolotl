-- relationships:
--   implements: ../docs/design/sessions.md
-- One row per conversation of a client key. `session_key` is a digest, never
-- the value the client sent.
CREATE TABLE sessions (
    client_id         TEXT NOT NULL,
    session_key       TEXT NOT NULL,
    source            TEXT NOT NULL,
    first_seen        INTEGER NOT NULL,
    last_seen         INTEGER NOT NULL,
    turns             INTEGER NOT NULL DEFAULT 0,
    current_tokens    INTEGER NOT NULL DEFAULT 0,
    peak_tokens       INTEGER NOT NULL DEFAULT 0,
    -- 1 once an upstream reported the prompt size; estimates stop counting then.
    tokens_reported   INTEGER NOT NULL DEFAULT 0,
    prompt_tokens     INTEGER NOT NULL DEFAULT 0,
    cached_tokens     INTEGER NOT NULL DEFAULT 0,
    context_window    INTEGER,
    provider          TEXT,
    model             TEXT,
    lock_name         TEXT,
    lock_provider     TEXT,
    lock_model        TEXT,
    lock_account      TEXT,
    locked_at         INTEGER,
    lock_failures     INTEGER NOT NULL DEFAULT 0,
    lock_breaks       INTEGER NOT NULL DEFAULT 0,
    last_break_reason TEXT,
    last_break_at     INTEGER,
    PRIMARY KEY (client_id, session_key)
);
CREATE INDEX sessions_last_seen ON sessions(last_seen);

-- Tool calls found in LLM responses, per client key. MCP calls are read from
-- mcp_call_log.
CREATE TABLE tool_usage (
    client_id TEXT NOT NULL,
    tool      TEXT NOT NULL,
    calls     INTEGER NOT NULL,
    last_used INTEGER NOT NULL,
    PRIMARY KEY (client_id, tool)
);
CREATE INDEX tool_usage_last_used ON tool_usage(last_used);

ALTER TABLE request_log ADD COLUMN session_key TEXT;
ALTER TABLE request_log ADD COLUMN context_tokens INTEGER;
ALTER TABLE request_log ADD COLUMN context_window INTEGER;
