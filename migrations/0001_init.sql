-- Subscription accounts. Credentials live in OpenBao; this is runtime state.
CREATE TABLE accounts (
    id              TEXT PRIMARY KEY,
    provider        TEXT NOT NULL,
    label           TEXT NOT NULL DEFAULT '',
    email           TEXT,
    external_id     TEXT,
    plan            TEXT,
    enabled         INTEGER NOT NULL DEFAULT 1,
    priority        INTEGER NOT NULL DEFAULT 0,
    cooldown_until  INTEGER,
    cooldown_reason TEXT,
    auth_failed     INTEGER NOT NULL DEFAULT 0,
    last_error      TEXT,
    last_used_at    INTEGER,
    created_at      INTEGER NOT NULL
);
CREATE INDEX accounts_provider ON accounts(provider);

-- Latest usage snapshot per account (normalized windows + raw provider JSON).
CREATE TABLE account_usage (
    account_id  TEXT PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
    fetched_at  INTEGER NOT NULL,
    windows     TEXT NOT NULL,
    raw         TEXT
);

-- One row per proxied request, for usage accounting.
CREATE TABLE request_log (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    ts             INTEGER NOT NULL,
    client_id      TEXT,
    provider       TEXT NOT NULL,
    account_id     TEXT,
    model          TEXT,
    endpoint       TEXT NOT NULL,
    status         INTEGER NOT NULL,
    input_tokens   INTEGER,
    output_tokens  INTEGER,
    cached_tokens  INTEGER,
    latency_ms     INTEGER NOT NULL,
    trace_id       TEXT
);
CREATE INDEX request_log_ts ON request_log(ts);
CREATE INDEX request_log_client ON request_log(client_id, ts);
CREATE INDEX request_log_account ON request_log(account_id, ts);
