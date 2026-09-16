# Git and worktree safety

The daemon is the only Git mutation authority. It invokes the user's `git` executable with structured arguments and bounded output; it never uses a shell or implements Git itself.

Registration resolves the repository root, canonicalizes identity, reads branches/remotes without network access, rejects duplicates, and models the root checkout as a stable worktree.

Managed worktrees use an access-controlled root and opaque project/worktree IDs. The default is `<data-directory>/worktrees/<project-id>/<worktree-id>`; an absolute non-root override may be configured. User-provided branch names are validated with Git and additionally reject control characters, traversal, and leading-option ambiguity. The base ref is resolved to a commit before `git worktree add`. The destination must not exist. After creation, the daemon canonicalizes the result, verifies direct-child containment, checkout-root identity, common Git-directory identity, and HEAD, then records it transactionally.

Removal always refreshes Git's worktree list, branch/HEAD identity, and porcelain status, including tracked, untracked, and ignored files. Any content or branch-identity change refuses removal. Confirmation is bound to the refreshed state. Only non-forced `git worktree remove` is allowed, and the branch is preserved. SylvOps never uses recursive deletion as a fallback.

Unix symlinks and Windows reparse points/junctions are rejected along managed paths. Windows containment comparisons account for drive and case behavior. Identity is revalidated immediately before destructive operations.

Read-only status, diff, branches, commits, file finding, and grep have time and byte caps. Binary and oversized files receive markers rather than terminal rendering.

## Current implementation boundary

Phase 3 implements repository registration, managed creation, porcelain status, state-bound confirmation, guarded non-forced removal, and sessions in managed checkouts. Git commands have five-second deadlines and 64 KiB stdout/stderr limits. Mutations set `core.hooksPath` to an empty application-owned directory and are serialized per project.

Startup now marks missing registered worktrees unavailable and discovers unexpected Git worktrees without modifying them; explicit import remains planned. A bounded read-only unified diff is available, while history presentation and pruning remain planned. SylvOps preserves a successfully created checkout if its database record cannot be written and audits the path; it never recursively deletes that checkout as rollback.
