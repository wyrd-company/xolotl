-- One row per upstream MCP tool call made by code mode.
CREATE TABLE mcp_call_log (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    ts          INTEGER NOT NULL,
    client_id   TEXT,
    server      TEXT NOT NULL,
    tool        TEXT NOT NULL,
    -- ok | tool_error (the tool ran and reported an error) | error (no result)
    status      TEXT NOT NULL,
    error       TEXT,
    latency_ms  INTEGER NOT NULL,
    trace_id    TEXT
);
CREATE INDEX mcp_call_log_ts ON mcp_call_log(ts);
