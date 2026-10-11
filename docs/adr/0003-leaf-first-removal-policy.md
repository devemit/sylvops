---
status: accepted
---

# Use explicit leaf-first removal

SylvOps removes supervision state from the leaves upward: a Session must be inactive before `Delete Session`, a Project must have no Sessions or active managed non-root Worktrees before `Remove Project`, and a Workspace must have no Projects before `Delete Workspace`. Parent operations do not cascade because requiring users to clear each child keeps process termination, retained history, and repository ownership visible at the point where each decision is made.

Every removal uses bounded authenticated inspection followed by explicit confirmation and execution with state-bound authorization. The daemon revalidates runtime truth or descendant membership immediately before mutation; confirmations cannot authorize a changed target. Successful attempts retain only a content-free audit record containing the timestamp, action, entity kind, opaque identifier, and outcome. Removal has no undo: safety comes from leaf-first eligibility, target-specific confirmation, revalidation, and preserving content outside the named boundary.

`Delete Session`, `Remove Project`, and `Delete Workspace` remove SylvOps-owned metadata only. `Remove Worktree` is the sole operation that removes a working directory: it remains limited to a clean, active, managed non-root Worktree with no active Sessions, uses structured non-forced Git execution after canonical revalidation, and preserves the Git branch. This decision supersedes earlier no-deletion statements only for these explicit leaf-first operations; it does not introduce automatic pruning, cascading deletion, root-checkout or branch deletion, or broader retention of terminal or provider content.
