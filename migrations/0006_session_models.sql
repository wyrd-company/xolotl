-- relationships:
--   implements: ../docs/design/sessions.md
-- The models each session has used, for the key issuance that used them. The
-- session keeps them after its key's policy excludes them.
CREATE TABLE session_models (
    client_id   TEXT NOT NULL,
    key_issued  TEXT NOT NULL,
    session_key TEXT NOT NULL,
    provider    TEXT NOT NULL,
    model       TEXT NOT NULL,
    last_used   INTEGER NOT NULL,
    PRIMARY KEY (client_id, key_issued, session_key, provider, model)
);
CREATE INDEX session_models_last_used ON session_models(last_used);
