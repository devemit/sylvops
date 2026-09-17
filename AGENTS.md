# SylvOps agent guide

## Ownership

- `sylvops-core`: domain types, validated IDs/paths, configuration, provider contracts, status state machine, and the versioned IPC protocol.
- `sylvops-daemon`: authoritative state, SQLite, local IPC server, PTY/process ownership, scrollback, Git operations, provider adapters, hooks, recovery, and audit events.
- `sylvops-tui`: replaceable Ratatui/Crossterm client. It renders daemon state and never owns agent processes or mutates Git directly.
- `sylvops-cli`: the `sylvops` executable, daemon lifecycle, TUI entry point, and non-interactive commands.
- `sylvops-test-support`: temporary repositories, fake executables, PTY/IPC fixtures, bounded waits, and cleanup helpers.

## Trust boundaries

Repository files, provider output, hook payloads, Git/GitHub text, executable discovery results, configuration, and IPC clients are untrusted. Canonicalize and revalidate paths, bound all input/output and queues, apply timeouts, redact diagnostics, and fail closed for unknown protocol, approval, or hook types. Never store credentials.

## Never invoke through a shell

Git, `gh`, provider CLIs, hooks, editors, browsers, and repository-defined commands must be launched with an executable plus structured argument array. Do not create concatenated shell command strings. Repository-defined commands always require a visible user action.

## Concurrency and persistence

The daemon is authoritative. A dedicated actor owns each mutable PTY session. Do not hold a global lock or database transaction across an async wait or subprocess. Use bounded channels and cancellation tokens. Treat cleanup as idempotent. Never trust a persisted PID after daemon restart.

## Migrations

Migrations live in `crates/sylvops-daemon/migrations`, are embedded into the daemon, checksum-validated, and run transactionally before IPC accepts clients. Never edit an applied migration; add a new numbered migration. Do not wait on Git or another process inside a transaction.

## Provider fixtures

Default tests use `sylvops-test-support` fake binaries and must not require real provider or GitHub authentication. Real-provider smoke tests must be explicit, opt-in, bounded, and must not print environment values.

## Tests

Run:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets
```

Process-tree and local-transport behavior is platform-specific; changes there require Windows and Unix coverage.

## Feature status

Cross-platform beta implementation candidate: protocol 1.6, authenticated local IPC, SQLite state, repository/worktree lifecycle and reconciliation, daemon-owned plain-shell and Codex PTYs, provider probing, bounded scrollback and VT snapshots, authenticated observational Codex hooks, deterministic attention states, resume, read-only bounded diff, hierarchical explorer plus Terminal/Changes/Details workspace, guided management forms, mouse controls, restored navigation context, embedded terminal attachment, `sylvops open`, `doctor`, and portable release workflows. Worktree removal is non-forced, refuses tracked/untracked/ignored content, and preserves branches.

Windows uses an application-owned ConPTY launch with pseudoconsole and Job Object attributes applied atomically by `CreateProcessW`; Unix retains `portable-pty` plus process groups. The prior beta baseline passed Windows/Linux/macOS; UX changes must pass the same hosted matrix before release. Planned after beta: explicit external-worktree import, Claude, GitHub, commit/push/PR actions, history/file-finder/Git-grep tools, notifications, remote execution, and signed/native installers. UI controls and documentation must not claim planned behavior is available.
