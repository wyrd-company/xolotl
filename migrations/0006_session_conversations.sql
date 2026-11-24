-- relationships:
--   implements: ../docs/design/sessions.md
-- The model each conversation of a session is on, for the key issuance that
-- had it. The session keeps those models after its key's policy excludes
-- them. `opening` is a digest of the conversation's first user turn.
-- `provider` and `model` are NULL while it is on a target it can't keep, one
-- only an alias reaches. `started` is its first recorded request, and
-- `sequence` its latest, so that an older request finishing late can't move it
-- back.
CREATE TABLE session_conversations (
    client_id   TEXT NOT NULL,
    key_issued  TEXT NOT NULL,
    session_key TEXT NOT NULL,
    opening     TEXT NOT NULL,
    provider    TEXT,
    model       TEXT,
    started     INTEGER NOT NULL,
    sequence    INTEGER NOT NULL,
    last_used   INTEGER NOT NULL,
    PRIMARY KEY (client_id, key_issued, session_key, opening)
);
CREATE INDEX session_conversations_last_used ON session_conversations(last_used);

-- The models a session's conversations switched away from: `sequence` is the
-- latest request that moved a conversation off the model. A conversation's
-- use of the model before then no longer keeps it for the rest of the session.
CREATE TABLE session_left (
    client_id   TEXT NOT NULL,
    key_issued  TEXT NOT NULL,
    session_key TEXT NOT NULL,
    provider    TEXT NOT NULL,
    model       TEXT NOT NULL,
    sequence    INTEGER NOT NULL,
    last_used   INTEGER NOT NULL,
    PRIMARY KEY (client_id, key_issued, session_key, provider, model)
);
CREATE INDEX session_left_last_used ON session_left(last_used);
