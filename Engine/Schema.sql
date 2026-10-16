-- Rune local schema v19. Runtime migrations are maintained in src/store/mod.rs.
-- Generated from an isolated integration-test database; contains no user data.

CREATE TABLE checkpoints (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id  INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    turn        INTEGER NOT NULL,
    created_at  INTEGER NOT NULL,
    manifest    TEXT    NOT NULL,
    UNIQUE(session_id, turn)
);

CREATE TABLE compactions (
    session_id  INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    through_id  INTEGER NOT NULL,
    summary     TEXT    NOT NULL,
    created_at  INTEGER NOT NULL,
    PRIMARY KEY (session_id, through_id)
);

CREATE TABLE messages (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id    INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    turn          INTEGER NOT NULL,
    role          TEXT    NOT NULL,
    content       TEXT    NOT NULL,
    tool_call_id  TEXT,
    tool_name     TEXT,
    created_at    INTEGER NOT NULL
, tool_calls TEXT, reasoning TEXT, media TEXT, continuation TEXT);

CREATE TABLE permissions (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    project     TEXT    NOT NULL,
    tool        TEXT    NOT NULL,
    pattern     TEXT    NOT NULL,
    created_at  INTEGER NOT NULL,
    UNIQUE(project, tool, pattern)
);

CREATE TABLE sessions (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    project     TEXT    NOT NULL,
    title       TEXT,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
, mode TEXT NOT NULL DEFAULT 'ask', settings   TEXT, active_seconds INTEGER NOT NULL DEFAULT 0);

CREATE TABLE tasks (
    session_id  INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    position    INTEGER NOT NULL,
    text        TEXT    NOT NULL,
    status      TEXT    NOT NULL,
    PRIMARY KEY (session_id, position)
);

CREATE TABLE token_usage (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id    INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    turn          INTEGER NOT NULL,
    timestamp     INTEGER NOT NULL,
    tok_in        INTEGER NOT NULL,
    tok_out       INTEGER NOT NULL,
    cache_in      INTEGER NOT NULL,
    context_used  INTEGER NOT NULL,
    cost          REAL    NOT NULL,
    provider      TEXT,
    model         TEXT
, role TEXT NOT NULL DEFAULT 'agent', details TEXT, cost_known INTEGER NOT NULL DEFAULT 0, estimated_in INTEGER NOT NULL DEFAULT 0);

CREATE TABLE toolcalls (
    id          TEXT    PRIMARY KEY,
    session_id  INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    turn        INTEGER NOT NULL,
    name        TEXT    NOT NULL,
    args        TEXT    NOT NULL,
    ok          INTEGER,
    summary     TEXT,
    started_at  INTEGER NOT NULL,
    ended_at    INTEGER
, wire_id TEXT);

CREATE INDEX idx_messages_session ON messages(session_id, id);

CREATE INDEX idx_sessions_project ON sessions(project, updated_at DESC);

CREATE INDEX idx_token_usage_session ON token_usage(session_id, timestamp);

CREATE INDEX idx_token_usage_time ON token_usage(timestamp);

CREATE INDEX idx_toolcalls_session ON toolcalls(session_id, started_at DESC);

CREATE TABLE IF NOT EXISTS session_files (
    session_id  INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    path        TEXT NOT NULL,
    first_blob  TEXT,
    first_turn  INTEGER NOT NULL,
    last_turn   INTEGER NOT NULL,
    added       INTEGER NOT NULL DEFAULT 0,
    removed     INTEGER NOT NULL DEFAULT 0,
    change      TEXT NOT NULL DEFAULT 'modified',
    PRIMARY KEY (session_id, path)
);

CREATE TABLE IF NOT EXISTS provider_models (
    provider    TEXT NOT NULL,
    id          TEXT NOT NULL,
    info        TEXT NOT NULL,
    updated_at  INTEGER NOT NULL,
    PRIMARY KEY (provider, id)
);

CREATE TABLE IF NOT EXISTS model_settings (
    provider    TEXT NOT NULL,
    model       TEXT NOT NULL,
    settings    TEXT NOT NULL,
    updated_at  INTEGER NOT NULL,
    PRIMARY KEY (provider, model)
);

CREATE TABLE IF NOT EXISTS counters (
    name        TEXT PRIMARY KEY,
    value       INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS promotions (
    token       TEXT PRIMARY KEY,
    session_id  INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    journal     TEXT NOT NULL,
    complete    INTEGER NOT NULL DEFAULT 0,
    started     INTEGER NOT NULL DEFAULT 0
);

PRAGMA user_version = 19;
