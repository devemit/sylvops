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
9. Reopen the four-panel TUI, select attention, inspect a bounded diff, attach, or resume.

The MVP never commits, pushes, merges, force-removes a worktree, stores credentials, executes repository content automatically, or exposes a network shell.

## Product invariants

- The daemon is the sole authority and owner of child processes.
- The TUI is disposable and replaceable.
- Every destructive action is explicit and revalidates current state.
- Provider-specific behavior remains behind adapters.
- Terminal text is not the source of truth for provider attention state.
- Controls do not advertise behavior without a tested implementation and recovery path.

## Status

The repository contains an implementation candidate for the Codex-first workflow: Phase 3 worktree reconciliation, provider adapters, Codex launch/probe/resume, authenticated lifecycle hooks, attention states, a four-panel TUI, bounded Git diff preview, and a fake-Codex hook/resume end-to-end scenario. The complete gate and that executable scenario pass in a Linux Docker environment, and Windows-target strict Clippy passes with the GNU toolchain. It is not yet an accepted cross-platform MVP: hosted CI has not run this change set, the native Windows host toolchain is incomplete, and Windows process containment still has a documented post-spawn Job Object race. See `README.md` for the exact implementation and validation boundary.
