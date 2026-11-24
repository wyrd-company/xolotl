-- relationships:
--   implements: ../docs/design/sessions.md
-- The models a session is on: each one that served it, which it keeps after
-- its key's policy excludes it. `opening` is a digest of the first user turn of
-- the model's last request, so that a switch of that conversation to another
-- model can drop it.
CREATE TABLE session_models (
    client_id   TEXT NOT NULL,
    session_key TEXT NOT NULL,
    provider    TEXT NOT NULL,
    model       TEXT NOT NULL,
    opening     TEXT,
    last_used   INTEGER NOT NULL,
    PRIMARY KEY (client_id, session_key, provider, model)
);
CREATE INDEX session_models_last_used ON session_models(last_used);
