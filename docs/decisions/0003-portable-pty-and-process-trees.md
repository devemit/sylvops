# ADR-0003: Portable PTY with platform process-tree control

- Status: provisional pending Phase 0 test results
- Date: 2026-09-15

## Context

SylvOps needs a common PTY interface, but termination and daemon detachment have materially different OS semantics.

## Decision

Use `portable-pty` for the initial PTY spike. Keep process-tree ownership behind a platform module:

- Unix validates that the PTY root leads a process group/session and signals the group.
- Windows assigns the root to a Job Object with kill-on-close semantics.

## Acceptance criteria

The fake agent spawns a descendant, the controlling client detaches, retained output replays, and explicit stop removes both root and descendant within a bounded deadline.

## Risk

Assigning a Windows process to a Job Object after `portable-pty` returns has a short escape race. If its spawn API cannot support create-suspended/assign/resume, production Windows sessions need a focused ConPTY/spawn wrapper. The spike must not be described as race-free.
