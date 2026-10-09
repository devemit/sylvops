ALTER TABLE workspaces
ADD COLUMN removal_generation INTEGER NOT NULL DEFAULT 0
CHECK (removal_generation >= 0);

CREATE INDEX idx_projects_workspace_removal
    ON projects(workspace_id, id);

CREATE TRIGGER workspace_deletion_generation_exhausted
BEFORE UPDATE OF removal_generation ON workspaces
WHEN OLD.removal_generation = 9223372036854775807
BEGIN
    SELECT RAISE(ABORT, 'workspace deletion generation exhausted');
END;

CREATE TRIGGER workspace_deletion_project_insert
AFTER INSERT ON projects
BEGIN
    UPDATE workspaces
    SET removal_generation = removal_generation + 1
    WHERE id = NEW.workspace_id;
END;

CREATE TRIGGER workspace_deletion_project_update
AFTER UPDATE OF workspace_id ON projects
BEGIN
    UPDATE workspaces
    SET removal_generation = removal_generation + 1
    WHERE id = OLD.workspace_id;
    UPDATE workspaces
    SET removal_generation = removal_generation + 1
    WHERE id = NEW.workspace_id AND NEW.workspace_id <> OLD.workspace_id;
END;

CREATE TRIGGER workspace_deletion_project_delete
AFTER DELETE ON projects
BEGIN
    UPDATE workspaces
    SET removal_generation = removal_generation + 1
    WHERE id = OLD.workspace_id;
END;
