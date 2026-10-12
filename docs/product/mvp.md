# SylvOps MVP

## Purpose

SylvOps is a local-first terminal application for supervising interactive coding agents in real Git worktrees. Its hierarchy is `Workspace → Project → Worktree → Session`. A session is a pseudo-terminal process owned by the daemon, not by the TUI.

## MVP workflow

1. Start one local daemon and connect a client over authenticated local IPC.
2. Register an existing Git repository.
3. Use the root checkout or create one managed worktree.
4. Start a plain shell in that checkout.
5. Detach while the process continues.
6. Reconnect, replay bounded scrollback, provide input, resize, and stop the complete process tree.
7. Launch an authenticated local Codex CLI with an optional prompt/model/effort.
8. Normalize observational hooks into reliable attention states and capture its external session ID.
9. Reopen the hierarchical TUI or native desktop with the last valid context restored, inspect a bounded diff, attach in the Terminal tab, or resume.
10. Clear obsolete supervision state leaf-first: delete an inactive Session, remove its cleared Project, and delete its empty Workspace, or remove a clean managed Worktree while preserving its branch.

The MVP never commits, pushes, merges, force-removes a worktree, stores credentials, executes repository content automatically, or exposes a network shell.

## Product invariants

- The daemon is the sole authority and owner of child processes.
- The TUI is disposable and replaceable.
- Every destructive action is explicit and revalidates current state.
- Parent removal never cascades through retained children, and only managed Worktree removal deletes a working directory.
- Provider-specific behavior remains behind adapters.
- Terminal text is not the source of truth for provider attention state.
- Controls do not advertise behavior without a tested implementation and recovery path.

## Status

The repository contains the Codex-first workflow plus release-ready usability: atomic Windows ConPTY/Job creation, protocol 1.15 workspace and entity operations, state-bound leaf-first removal, separate bounded TUI/desktop state, a responsive Active Sessions TUI overview with persisted Cards/List presentation, guided management forms, native repository selection, responsive and resizable desktop panels, keyboard and mouse workflows, embedded attachment, dynamic PTY resize, one-command repository opening, redacted diagnostics, and release packaging. `v0.1.0` is the next ordinary application release and remains unpublished until the current tree passes the complete hosted Windows, Linux, and macOS gate with protected publisher credentials. Free unsigned previews are a separate testing lane and do not satisfy that release gate.
