# ADR 0015: Native desktop state and management

## Decision

Protocol 1.7 adds bounded `GetDesktopState` and `SaveDesktopState` operations. The daemon stores one independent `desktop/navigation.v1` record in the existing `ui_state` table. Writes do not advance the entity revision and do not emit lifecycle events.

The desktop remains a replaceable authenticated IPC client. Its synchronous state updates render local interaction immediately, while bounded bridge effects request authoritative workspace, repository, worktree, session, diff, attachment, and removal operations from the daemon.

The persisted record contains only navigation IDs, up to 16 open session tabs, appearance choices, validated window dimensions, panel ratios, and the compact navigator selection. Attachment state is deliberately excluded so reopening the desktop never takes terminal control automatically.

Repository selection uses a native folder dialog with an editable path fallback. The selected path is still untrusted: only `EnsureProject` in the daemon canonicalizes it and decides whether it is a valid Git repository.

## Consequences

- TUI and desktop preferences cannot overwrite each other.
- Corrupt, oversized, stale, or out-of-range state safely falls back to defaults.
- Desktop close waits briefly for a state acknowledgement but never stops daemon-owned sessions.
- Exact protocol matching means 1.6 clients and 1.7 daemons must be upgraded together.
