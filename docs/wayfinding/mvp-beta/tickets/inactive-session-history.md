---
title: Define inactive-session history semantics
label: wayfinder:grilling
state: closed
assignee: null
blocked_by: []
---

## Question

What must users be able to see and do with finished, failed, terminated, and disconnected sessions before and after a daemon restart, given that terminal scrollback is intentionally memory-only for the beta?

## Resolution

Inactive sessions remain durable records in their original Workspace → Project → Worktree hierarchy; the beta does not automatically prune or hide them. Both native clients show the session name, provider, state, worktree context, start/end timestamps, exit code, failure or recovery reason, and whether provider resume metadata makes resume eligible. Metadata renames remain safe for inactive sessions.

Terminal output is explicitly memory-only. While the same daemon process still owns the completed session actor, a client may replay the actor's bounded terminal state. After a daemon restart, no terminal output is advertised as durable or reconstructed: persisted active-looking sessions become `disconnected`, while already terminal states retain their recorded state and metadata. An unavailable replay is a normal limitation, not evidence that the session record was lost.

`finished_unseen` remains an attention item until viewed, then becomes `finished_seen`. `failed` remains an attention item until the user reviews it. `terminated` records an explicit user stop. `disconnected` records restart recovery where the prior process identity could not safely be trusted. Only finished, failed, or disconnected provider sessions with a verified external session ID are eligible for resume; terminated sessions are deliberately not resumable. Resume creates a new session record rather than mutating or resurrecting the historical record.

The beta does not persist raw terminal bytes, provider transcripts, prompts, or secrets, and it does not offer history deletion or automatic retention policies.

## Acceptance evidence

- Database tests cover terminal-state persistence, seen/unseen transitions, and idempotent restart reconciliation to `disconnected`.
- Daemon tests prove detach/reconnect replay while the daemon remains alive and retain the session record after an explicit stop.
- Desktop and TUI details expose the retained outcome metadata and state that terminal output does not survive daemon restart.
- Shared state-policy tests keep resume eligibility aligned between daemon enforcement and both clients.
