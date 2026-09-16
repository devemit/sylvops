CREATE TABLE workspaces (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 200),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    last_opened_at INTEGER,
    is_open INTEGER NOT NULL DEFAULT 0 CHECK (is_open IN (0, 1))
) STRICT;

CREATE TABLE projects (
    id TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 200),
    repository_path TEXT NOT NULL,
    canonical_repository_path TEXT NOT NULL UNIQUE,
    default_branch TEXT,
    remote_url TEXT,
    created_at INTEGER NOT NULL,
    last_activity_at INTEGER NOT NULL
) STRICT;

CREATE TABLE worktrees (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE RESTRICT,
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 200),
    path TEXT NOT NULL,
    canonical_path TEXT NOT NULL UNIQUE,
    branch TEXT,
    base_ref TEXT NOT NULL,
    base_commit TEXT NOT NULL,
    is_root_checkout INTEGER NOT NULL DEFAULT 0 CHECK (is_root_checkout IN (0, 1)),
    status TEXT NOT NULL CHECK (status IN (
        'active', 'creating', 'removing', 'removed', 'missing', 'invalid'
    )),
    created_at INTEGER NOT NULL,
    last_activity_at INTEGER NOT NULL,
    removed_at INTEGER
) STRICT;

CREATE TABLE provider_profiles (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL CHECK (kind IN ('shell', 'codex', 'claude', 'cursor', 'pi')),
    display_name TEXT NOT NULL CHECK (length(display_name) BETWEEN 1 AND 200),
    executable_path TEXT,
    default_model TEXT,
    default_effort TEXT,
    enabled INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    capabilities_json TEXT NOT NULL DEFAULT '{}',
    last_probe_status TEXT,
    last_probe_at INTEGER
) STRICT;

CREATE TABLE sessions (
    id TEXT PRIMARY KEY,
    worktree_id TEXT NOT NULL REFERENCES worktrees(id) ON DELETE RESTRICT,
    provider_profile_id TEXT REFERENCES provider_profiles(id) ON DELETE SET NULL,
    provider_kind TEXT NOT NULL CHECK (provider_kind IN (
        'shell', 'codex', 'claude', 'cursor', 'pi'
    )),
    display_name TEXT NOT NULL CHECK (length(display_name) BETWEEN 1 AND 200),
    state TEXT NOT NULL CHECK (state IN (
        'fresh', 'starting', 'running', 'needs_feedback', 'finished_unseen',
        'finished_seen', 'failed', 'terminated', 'disconnected'
    )),
    process_id INTEGER,
    process_identity TEXT,
    external_session_id TEXT,
    command TEXT NOT NULL,
    arguments_json TEXT NOT NULL DEFAULT '[]',
    cwd TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    started_at INTEGER,
    ended_at INTEGER,
    last_activity_at INTEGER NOT NULL,
    last_seen_output_sequence INTEGER NOT NULL DEFAULT 0 CHECK (last_seen_output_sequence >= 0),
    exit_code INTEGER,
    failure_reason TEXT
) STRICT;

CREATE TABLE session_prompts (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    submitted_at INTEGER NOT NULL,
    prompt_text TEXT NOT NULL,
    byte_length INTEGER NOT NULL CHECK (byte_length >= 0),
    retention_class TEXT NOT NULL CHECK (retention_class IN ('recent', 'pinned'))
) STRICT;

CREATE TABLE pull_request_links (
    id TEXT PRIMARY KEY,
    worktree_id TEXT NOT NULL REFERENCES worktrees(id) ON DELETE CASCADE,
    provider TEXT NOT NULL CHECK (provider = 'github'),
    repository_identity TEXT NOT NULL,
    number INTEGER,
    url TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE (worktree_id, url)
) STRICT;

CREATE TABLE audit_events (
    id TEXT PRIMARY KEY,
    occurred_at INTEGER NOT NULL,
    actor_kind TEXT NOT NULL CHECK (actor_kind IN ('client', 'daemon', 'hook', 'recovery')),
    actor_id TEXT,
    action TEXT NOT NULL,
    entity_kind TEXT,
    entity_id TEXT,
    outcome TEXT NOT NULL CHECK (outcome IN ('attempted', 'succeeded', 'refused', 'failed')),
    details_json TEXT NOT NULL DEFAULT '{}'
) STRICT;

CREATE TABLE settings (
    key TEXT PRIMARY KEY,
    value_json TEXT NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;

CREATE TABLE ui_state (
    client_scope TEXT NOT NULL,
    key TEXT NOT NULL,
    value_json TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (client_scope, key)
) STRICT;

CREATE INDEX idx_projects_workspace_activity
    ON projects(workspace_id, last_activity_at DESC);
CREATE INDEX idx_worktrees_project_activity
    ON worktrees(project_id, is_root_checkout DESC, last_activity_at DESC);
CREATE INDEX idx_sessions_worktree_activity
    ON sessions(worktree_id, last_activity_at DESC);
CREATE INDEX idx_sessions_attention
    ON sessions(state, last_activity_at DESC);
CREATE INDEX idx_session_prompts_retention
    ON session_prompts(session_id, submitted_at DESC);
CREATE INDEX idx_pull_request_repository
    ON pull_request_links(repository_identity, number);
CREATE INDEX idx_audit_entity_time
    ON audit_events(entity_kind, entity_id, occurred_at DESC);
CREATE INDEX idx_audit_action_time
    ON audit_events(action, occurred_at DESC);
