# ADR-0012: Beta management workflow and embedded terminal

> **Historical context:** release-phase terminology below records when this decision was made. Current delivery uses ordinary semantic `0.x` application releases.

## Status

Accepted for the cross-platform beta candidate; hosted acceptance remains required.

## Context

The MVP daemon could supervise sessions and the initial TUI could browse state, but common setup and management still required separate CLI commands. Interactive attachment also left the four-panel interface, obscuring the surrounding project and attention context.

## Decision

Protocol 1.5 adds authoritative workspace opening and metadata-only project, worktree, and session renames. All mutations remain transactional, revisioned, audited, and daemon-owned. Repository paths, worktree directories, and Git branches are never changed by a metadata rename.

The TUI uses explicit navigation, modal, palette, and attached-terminal modes. It creates and manages entities only through typed IPC. Attached output is parsed into VT state and rendered by Ratatui; untrusted provider escape sequences are never copied directly to the host terminal. `Ctrl+]` is the sole detach key, while ordinary keys remain provider input.

The default `sylvops` command opens the TUI. `sylvops open [PATH]` idempotently selects or registers a Git repository before opening it, and creates a provider session only when the user supplies explicit provider flags. `sylvops doctor` performs bounded, redacted checks including a real PTY spawn/output/reap probe.

## Consequences

- The daemon remains the only owner of PTYs, persisted state, and Git mutations.
- The command palette searches known entities and actions; it is not an arbitrary command launcher.
- Project deletion, root-checkout deletion, and branch deletion remain unavailable.
- Closing the TUI detaches the client without stopping supervised sessions.
- Protocol 1.5 intentionally exact-matches local clients and daemons.
