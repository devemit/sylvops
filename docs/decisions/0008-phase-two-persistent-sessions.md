# ADR-0008: Phase 2 persistent terminal sessions

## Status

Accepted for the Phase 2 implementation.

## Decision

Phase 2 uses the CLI as the first interactive client and pulls forward only read-only repository/root-checkout registration. The daemon owns every PTY and retains bounded, sequenced raw output plus maintained VT100 parser state. One attached client controls input and resize; additional clients observe. `Ctrl+]` detaches without stopping the process.

Protocol 1.1 separates correlated control frames (opcode 10) from events (opcode 11). Per-client queues are bounded by item count and writer time. Output loss is isolated to the slow client and repaired with a parser-derived resynchronization snapshot.

Shells receive an allowlisted environment and cannot be selected by an IPC client. Git and shell processes use structured argument APIs. Session state changes and related activity/audit records are transactional, while spawning and Git inspection occur outside database transactions.

## Consequences

- Scrollback survives client disconnect but not daemon restart.
- The Ratatui mission-control interface and managed worktree mutations remain later phases.
- The CLI renders only parser-generated terminal state, preventing raw OSC/clipboard controls from reaching the host terminal.
- The Windows Job Object is assigned immediately after `portable-pty` spawn. This known containment race blocks a release-hardening claim and real-provider rollout until resolved.
