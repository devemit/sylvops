CREATE TABLE project_github_associations (
    project_id TEXT PRIMARY KEY REFERENCES projects(id) ON DELETE CASCADE,
    remote_name TEXT NOT NULL CHECK (length(remote_name) BETWEEN 1 AND 128),
    host TEXT NOT NULL CHECK (length(host) BETWEEN 1 AND 253),
    owner TEXT NOT NULL CHECK (length(owner) BETWEEN 1 AND 100),
    repository TEXT NOT NULL CHECK (length(repository) BETWEEN 1 AND 100),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;

CREATE INDEX idx_project_github_associations_host
    ON project_github_associations(host, owner, repository);
