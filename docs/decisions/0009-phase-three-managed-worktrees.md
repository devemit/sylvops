# ADR-0009: Phase 3 managed Git worktrees

## Status

Accepted for the Phase 3 implementation candidate.

## Decision

SylvOps creates worktrees only below a canonical private managed root, using opaque project and worktree IDs for directory names. The client supplies a branch and optional base ref, but never a destination path or executable. The daemon validates the branch with both local policy and `git check-ref-format`, resolves the base to a commit, and invokes system Git with structured arguments and bounded I/O.

Git mutations are serialized per project. Session creation and removal also share a per-worktree operation lock. These are entity-scoped locks; the global lock maps are released before subprocess or database awaits. SQLite transactions never encompass Git execution.

Removal is intentionally two-step. Status includes tracked, untracked, and ignored entries and returns a confirmation token only when empty. The explicit remove request carries that token; the daemon revalidates containment, repository identity, Git registration, HEAD, and cleanliness before a non-forced `git worktree remove`. The Git branch is preserved.

Creation and deletion cross a filesystem/SQLite boundary that cannot be one atomic transaction. If creation succeeds and persistence fails, the checkout is preserved and audited for later reconciliation. If Git removal succeeds and the database update fails, the discrepancy is audited. Automatic recursive rollback is prohibited because it could destroy repository-controlled files created during checkout.

## Consequences

- Managed checkouts can run the Phase 2 daemon-owned shell session immediately.
- Dirty, untracked, or ignored files prevent removal.
- Root and externally located checkouts cannot be removed through this path.
- External-worktree discovery and discrepancy reconciliation remain follow-up hardening.
- The milestone remains an implementation candidate until the complete Windows and Unix matrix passes.
