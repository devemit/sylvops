# Persistence and initial schema

SQLite stores operational session metadata, not prompts, provider transcripts, terminal input, or unbounded terminal output. Initial prompts are passed only to the launched provider process; they are excluded from both prompt history and the recorded argument vector. UUIDv7 identifiers are canonical text and timestamps are UTC Unix milliseconds. The initial migration is `crates/sylvops-daemon/migrations/0001_initial.sql`.

The schema contains `schema_migrations`, `workspaces`, `projects`, `worktrees`, `provider_profiles`, `sessions`, `session_prompts`, `pull_request_links`, `audit_events`, `settings`, and `ui_state`, with foreign keys, status constraints, uniqueness constraints, and activity/attention indexes.

Migrations are embedded, checksum-validated, and applied before accepting IPC connections. Applied migrations are immutable. Migration 2 deletes legacy prompt rows and clears historical Codex argument arrays that could contain an untagged prompt while preserving session identity, lifecycle, outcome, and verified resume metadata. Startup refuses an unknown newer schema rather than attempting a destructive downgrade.

Multi-entity state transitions use short transactions. No transaction spans a subprocess. Worktree mutation therefore follows validate → external Git command → verify → transactional record/audit, with conservative recovery if the final persistence step fails.

Configuration is separate from the database: versioned TOML plus a machine-local override, atomically written beside the target and renamed after flush. Unknown fields are retained where possible.

## Phase 1 implementation

One bounded actor channel feeds a dedicated operating-system thread that exclusively owns the SQLite connection. Startup enables foreign keys, WAL, a five-second busy timeout, and `NORMAL` synchronous mode before applying migrations. The daemon does not accept authenticated requests until migration and restart reconciliation finish.

On every daemon start, sessions persisted as `starting`, `running`, or `needs_feedback` become `disconnected`, their PID is cleared, and a recovery audit event is inserted in the same transaction. A PID alone is never used to adopt a process.
