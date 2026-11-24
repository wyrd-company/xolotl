-- relationships:
--   implements: ../docs/design/sessions.md
-- The model each conversation of a session is on, for the key issuance that
-- had it. A conversation keeps that model after its key's policy excludes it.
-- `opening` is a digest of the conversation's first user turn. `provider` and
-- `model` are NULL while it is on a target it can't keep, one only an alias
-- reaches. `sequence` orders its requests, so that an older request finishing
-- late can't move it back.
CREATE TABLE session_conversations (
    client_id   TEXT NOT NULL,
    key_issued  TEXT NOT NULL,
    session_key TEXT NOT NULL,
    opening     TEXT NOT NULL,
    provider    TEXT,
    model       TEXT,
    sequence    INTEGER NOT NULL,
    last_used   INTEGER NOT NULL,
    PRIMARY KEY (client_id, key_issued, session_key, opening)
);
CREATE INDEX session_conversations_last_used ON session_conversations(last_used);
