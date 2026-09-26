# ADR 0013: Hierarchical TUI and bounded navigation state

> **Historical context:** release-phase terminology below records when this decision was made. Current delivery uses ordinary semantic `0.x` application releases.

## Status

Accepted for the pre-beta UX milestone.

## Context

The original four equal panels exposed the domain structure but gave terminal output too little space and made first-run actions difficult to discover. Async daemon calls were also interleaved with rendering and input decisions, making behavior harder to test.

## Decision

Use one collapsible `Workspace → Project → Worktree → Session` explorer beside a larger main workspace with Terminal, Changes, and Details tabs. A synchronous reducer updates navigation state and yields explicit async effects; rendering performs no IPC or Git operations. Forms, terminal encoding, theme semantics, input mapping, and rendering are separate modules.

The TUI supports semantic focus zones, responsive single-pane behavior below 72 columns, keyboard and mouse hit-testing, explicit attachment, guided provider-aware forms, inline failures, and four-second success/info feedback. Labels and symbols remain authoritative when color is unavailable.

Protocol 1.6 adds bounded `GetTuiState` and `SaveTuiState` operations. The daemon stores only selected entity IDs and the selected main tab in `ui_state/navigation.v1`. Writes are debounced by 500 ms, occur again on clean exit, and do not change the entity revision. Invalid JSON, stale IDs, and hierarchy mismatches fall back to the first valid visible context.

## Consequences

- The terminal receives substantially more usable space without weakening daemon ownership.
- Selecting an entity remains read-only; attach, stop, and removal stay explicit.
- Form text, searches, terminal input, scrollback, and credentials are not recoverable because they are intentionally never persisted.
- Mouse clicks are supported for TUI controls, while terminal mouse-protocol forwarding remains deferred.
- Exact protocol matching means 1.5 clients and 1.6 daemons must be upgraded together.
