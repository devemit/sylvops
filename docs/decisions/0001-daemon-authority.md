# ADR-0001: Daemon authority and replaceable clients

- Status: accepted
- Date: 2026-09-15

## Decision

The daemon is the only owner of persistent application state, PTYs, child processes, Git mutations, provider hooks, and lifecycle transitions. TUI and non-interactive CLI processes are disposable IPC clients.

Each PTY session has a dedicated owner task and bounded communication channels. SQLite operations are isolated from the async executor. A TUI closure only releases its attachment; it never signals the child.

## Consequences

Sessions can continue while no UI exists, and multiple clients can reconnect consistently. Daemon crash recovery is a distinct problem: processes are not adopted from untrusted persisted PIDs and become disconnected unless a future secure identity mechanism is introduced.
