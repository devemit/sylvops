# ADR-0005: SQLite behind a dedicated actor

- Status: accepted for Phase 1
- Date: 2026-09-15

## Decision

Use `rusqlite` with embedded numbered migrations. A dedicated database actor owns connections and executes short requests. Enable foreign keys, WAL, busy timeout, and appropriate indexes.

No transaction remains open while awaiting Git, a provider, a hook, or a PTY. Cross-filesystem/database operations use validation, external mutation, verification, then a short persistence transaction and audit event.

## Consequences

The daemon has a clear serialization point for state transitions without blocking Tokio workers. Filesystem and database changes cannot be atomically committed together, so startup reconciliation and conservative failure handling are mandatory.
