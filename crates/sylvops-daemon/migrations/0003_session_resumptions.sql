CREATE TABLE session_resumptions (
    source_session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE RESTRICT,
    successor_session_id TEXT NOT NULL UNIQUE REFERENCES sessions(id) ON DELETE RESTRICT,
    created_at INTEGER NOT NULL
) STRICT;
