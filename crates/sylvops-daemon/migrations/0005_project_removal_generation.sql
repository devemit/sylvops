ALTER TABLE projects
ADD COLUMN removal_generation INTEGER NOT NULL DEFAULT 0
CHECK (removal_generation >= 0);

CREATE INDEX idx_worktrees_project_removal
    ON worktrees(project_id, is_root_checkout, status, id);

CREATE TRIGGER project_removal_generation_exhausted
BEFORE UPDATE OF removal_generation ON projects
WHEN OLD.removal_generation = 9223372036854775807
BEGIN
    SELECT RAISE(ABORT, 'project removal generation exhausted');
END;

CREATE TRIGGER project_removal_worktree_insert
AFTER INSERT ON worktrees
BEGIN
    UPDATE projects
    SET removal_generation = removal_generation + 1
    WHERE id = NEW.project_id;
END;

CREATE TRIGGER project_removal_worktree_update
AFTER UPDATE ON worktrees
BEGIN
    UPDATE projects
    SET removal_generation = removal_generation + 1
    WHERE id = OLD.project_id;
    UPDATE projects
    SET removal_generation = removal_generation + 1
    WHERE id = NEW.project_id AND NEW.project_id <> OLD.project_id;
END;

CREATE TRIGGER project_removal_worktree_delete
AFTER DELETE ON worktrees
BEGIN
    UPDATE projects
    SET removal_generation = removal_generation + 1
    WHERE id = OLD.project_id;
END;

CREATE TRIGGER project_removal_session_insert
AFTER INSERT ON sessions
BEGIN
    UPDATE projects
    SET removal_generation = removal_generation + 1
    WHERE id = (
        SELECT project_id FROM worktrees WHERE id = NEW.worktree_id
    );
END;

CREATE TRIGGER project_removal_session_delete
AFTER DELETE ON sessions
BEGIN
    UPDATE projects
    SET removal_generation = removal_generation + 1
    WHERE id = (
        SELECT project_id FROM worktrees WHERE id = OLD.worktree_id
    );
END;

CREATE TRIGGER project_removal_pull_request_insert
AFTER INSERT ON pull_request_links
BEGIN
    UPDATE projects
    SET removal_generation = removal_generation + 1
    WHERE id = (
        SELECT project_id FROM worktrees WHERE id = NEW.worktree_id
    );
END;

CREATE TRIGGER project_removal_pull_request_update
AFTER UPDATE ON pull_request_links
BEGIN
    UPDATE projects
    SET removal_generation = removal_generation + 1
    WHERE id = (
        SELECT project_id FROM worktrees WHERE id = OLD.worktree_id
    );
    UPDATE projects
    SET removal_generation = removal_generation + 1
    WHERE id = (
        SELECT project_id FROM worktrees WHERE id = NEW.worktree_id
    ) AND NEW.worktree_id <> OLD.worktree_id;
END;

CREATE TRIGGER project_removal_pull_request_delete
AFTER DELETE ON pull_request_links
BEGIN
    UPDATE projects
    SET removal_generation = removal_generation + 1
    WHERE id = (
        SELECT project_id FROM worktrees WHERE id = OLD.worktree_id
    );
END;
