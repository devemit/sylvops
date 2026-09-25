# SylvOps beta closure map

Label: `wayfinder:map`

## Destination

Reach a defensible `v0.1.0-beta.1`: a developer on Windows, Linux, or macOS can install SylvOps, open an existing Git repository, create or select a worktree, run Shell or Codex, detach and recover, inspect changes, and stop safely without building from source or diagnosing hidden environment assumptions.

## Notes

- The user explicitly authorized implementation while working this map; resolved decisions may be implemented in place.
- Treat `docs/product/mvp.md`, `AGENTS.md`, and the architecture decisions as standing constraints.
- Keep the daemon authoritative, retain bounded resource behavior, and never launch provider or Git commands through a shell.
- Resolve at most one decision ticket per working session.

## Decisions so far

<!-- Closed tickets are indexed here with a one-line gist and a link to their resolution. -->

- [Safe Codex discovery](tickets/safe-codex-discovery.md): resolve only canonical native binaries, including official npm payloads and desktop caches, without executing package-manager or shell shims.
- [Inactive-session history](tickets/inactive-session-history.md): retain outcome metadata indefinitely, keep terminal output memory-only, and resume eligible provider sessions into new records.
- [First-run and provider recovery](tickets/first-run-recovery.md): guide an empty desktop through Workspace → Project/root Worktree → Session, keep Shell as the fallback, and re-probe Codex after explicit install or login repair.
- [Beta acceptance contract](tickets/beta-acceptance-contract.md): block release on the exact cross-platform package, client, daemon, worktree, Shell, Codex, recovery, safety, and evidence clauses; permit only the named beta limitations.
- [Fresh-machine release proof](tickets/release-proof.md): stage exactly four validated, checksummed, attested archives behind a protected publication gate, then require immutable native clean-host evidence for every contract scenario and record all missing or failed checks.

## Still to prove and decide

- The release-proof process is resolved, but no staged-candidate, clean-host, or opt-in real-Codex evidence has been collected. Every such row remains `NOT_RUN` and release-blocking.
- The beta-cut ticket must apply the contract to a complete evidence bundle and make the final ship-or-delay decision before the protected publication gate is approved.
- Real-user desktop testing may expose a small final polish set; only beta-blocking failures graduate into tickets.

## Out of scope

- Claude, GitHub issue/PR views, commit/push/merge actions, remote execution, notifications, file finding, Git grep, auto-update, signing, and notarization remain post-beta.
- Persisted terminal scrollback remains outside this beta; only metadata survives a daemon restart.
