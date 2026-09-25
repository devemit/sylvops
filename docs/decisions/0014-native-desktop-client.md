# ADR 0014: Native desktop client beside the terminal UI

- Status: accepted for implementation
- Date: 2026-09-17

## Context

The Ratatui client proves the complete local-first workflow, but its terminal presentation makes mouse discovery, settings, persistent session tabs, and wide mission-control layouts harder for new users. Replacing the daemon or moving supervision into a window process would weaken the existing detach, recovery, and process-containment guarantees.

## Decision

Add `sylvops-desktop`, a native Rust client built with Iced. It connects through the same authenticated, versioned local IPC as every other client and never owns provider processes, PTYs, SQLite, or Git mutations.

The desktop layout uses workspace tabs across the top, separate project, worktree, and session columns, and a dominant Terminal/Changes/Details workspace. Its footer exposes the application version, current workspace, branch, view, connectivity, and key hints. Session attachment remains explicit, normal terminal keys are sent as bounded typed IPC requests, and `Ctrl+]` detaches. Output is rendered from maintained VT state rather than emitting provider escape sequences to a host terminal.

`sylvops` and `sylvops up [PATH]` start or reconnect to the daemon and launch the desktop process. `sylvops open [PATH]` remains a compatibility repository-opening command, and `sylvops tui` keeps the keyboard-first terminal client available. The GUI entry point runs on the operating-system main thread before the CLI constructs a Tokio runtime; its IPC bridge owns a separate bounded worker runtime.

## Consequences

- Closing the window does not stop daemon-owned sessions.
- Desktop, TUI, and scripting clients can coexist without duplicating lifecycle authority.
- Native renderer dependencies increase binary and build size.
- Platform packaging must include native-window smoke coverage in addition to PTY and IPC tests.
- Desktop entity forms and bounded preferences were completed in protocol 1.7. The desktop may forward bounded wheel events through existing session input when an alternate-screen TUI requests mouse reporting; click and drag forwarding and notifications remain incremental work that does not change daemon authority.
