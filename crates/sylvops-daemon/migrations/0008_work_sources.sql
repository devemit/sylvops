CREATE TABLE work_sources (
    worktree_id TEXT PRIMARY KEY REFERENCES worktrees(id) ON DELETE CASCADE,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    host TEXT NOT NULL CHECK (length(host) BETWEEN 1 AND 253),
    owner TEXT NOT NULL CHECK (length(owner) BETWEEN 1 AND 100),
    repository TEXT NOT NULL CHECK (length(repository) BETWEEN 1 AND 100),
    item_kind TEXT NOT NULL CHECK (item_kind IN ('issue', 'pull-request')),
    item_number INTEGER NOT NULL CHECK (item_number > 0),
    canonical_url TEXT NOT NULL CHECK (length(canonical_url) BETWEEN 1 AND 4096),
    created_at INTEGER NOT NULL
) STRICT;

CREATE INDEX idx_work_sources_item
    ON work_sources(project_id, host, owner, repository, item_kind, item_number, created_at);
