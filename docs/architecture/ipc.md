# IPC protocol

## Transport

- Unix: a mode-`0600` Unix-domain socket below the user runtime directory, with peer credential validation where available.
- Windows: a named pipe with a DACL restricted to the current user SID and client-token validation where practical.

Control IPC never listens on TCP. The provider hook HTTP receiver is a separate loopback-only service.

## Frame

Every frame is length-prefixed and contains a fixed header followed by MessagePack:

```text
u32 frame length
u32 magic (CSTL)
u16 protocol major
u16 protocol minor
u8  class (request/response/event)
u16 opcode
u8  flags
16B message UUID
16B correlation UUID
u32 payload length
N   MessagePack payload
```

The maximum complete frame is 1 MiB and an individual PTY output payload is at most 64 KiB. Unsupported major versions, unknown classes/opcodes, inconsistent lengths, trailing bytes, and oversized frames fail closed.

The first client request is `Hello`. It authenticates with a high-entropy token stored in the private runtime directory and negotiates an exact protocol version before other requests are accepted. Authentication comparisons are constant-time. Protocol 1.15 includes managed-worktree controls, provider listing/probing, provider-aware session creation, guarded Codex and Claude resume, provider runtime and authentication capabilities, normalized lifecycle events with Provider/Session/Worktree identity, bounded recoverable failure categories, remaining background-work state, authenticated conversation-identity transitions, bounded Git diff requests, workspace switching, metadata-only entity renames, bounded client-state persistence including the TUI content view, Cards/List Session presentation, appearance, and density, signed application-upgrade controls and progress, exact active-session upgrade confirmations, guarded user-data-removal preparation, inspect-then-execute deletion of inactive Sessions, inspect-then-execute removal of cleared Projects, and inspect-then-execute deletion of empty Workspaces with state-bound authorization and deterministic navigation fallback. The provider runtime seam accepts only bounded lifecycle payloads, carries a redacted loopback endpoint credential bound to one Provider/Session/Worktree, and returns application-owned overlay paths for revalidated cleanup. Opcode 10 carries correlated controls and opcode 11 carries unsolicited daemon events. Snapshots and entity events carry database-actor revisions; clients detecting a revision gap request a new snapshot. UI-state writes deliberately do not advance that revision or emit lifecycle events.

The daemon serializes update discovery, download, install preparation, detached-helper outcome recording, and user-data removal through one application-mutation coordinator. A handoff file keeps that coordinator reserved across daemon replacement; a replacement daemon refuses competing mutations until the authenticated helper reports installation or rollback and the reservation is removed. Data removal places a root-identity-bound reservation in every owned state directory and launches a detached daemon-owned lifecycle helper with a one-time handoff token before shutdown. Desktop and CLI clients only observe completion. An OS-released exclusive lease in a high-entropy owner-only coordination directory prevents helper/recovery overlap without trusting persisted PIDs, and daemon startup fails closed while any reservation remains.

The sole persisted TUI record is `client_scope = 'tui'`, `key = 'navigation.v1'`. It contains only selected project/worktree/session IDs, the selected main tab, `theme`, and `density`. Theme is one of `terminal_default`, `nord`, `tokyo_night`, `catppuccin`, `gruvbox_dark`, or `solarized_light`; density is `comfortable` or `compact`. Missing fields use safe defaults and unknown appearance values normalize to Terminal Default and Comfortable. Corrupt or stale records are ignored safely. Searches, forms, terminal input or scrollback, attachment state, and credentials are never persisted there. The `tui` and `desktop` scopes are independent, and either preference-only write leaves the entity revision unchanged and emits no lifecycle event.

The desktop uses a separate `client_scope = 'desktop'`, `key = 'navigation.v1'` record. It is limited to selected IDs, the selected main tab, at most 16 open session IDs, theme, density, terminal font size, validated window size, panel ratios, and the compact navigator panel. It never stores attachment state, terminal data, form contents, searches, or credentials.

`InspectSessionDeletion`, `InspectProjectRemoval`, `InspectWorkspaceDeletion`, and Worktree status inspection return either bounded actionable refusal data or opaque authorization for the observed state. Execution revalidates Session lifecycle and runtime ownership, exact Project or Workspace membership, or Worktree identity, branch, HEAD, and clean porcelain status immediately before mutation. `RemoveWorktree` then invokes non-forced Git removal; the other operations mutate metadata only. Tokens are authorization context for one target and observed state, not credentials.

Each connection has dedicated reader and writer ownership tasks. A frame read is never cancellation-interrupted by lifecycle or terminal events, and responses are correlated independently of event delivery.

## Backpressure

Each connection has a 256-item, 4 MiB send queue and a ten-second writer deadline. Lifecycle changes use bounded waiting; lag on the shared lifecycle broadcast produces `StateResynchronizationRequired` with the latest revision so the client can request a snapshot. Terminal output uses non-blocking enqueue and may be dropped only for the affected slow client, which then receives `ResynchronizationRequired` with parser-derived terminal state.

An attach boundary newer than the daemon's latest output sequence is treated as an explicit terminal resynchronization request. This lets a client recover with a parser snapshot if its own bounded local event queue reports loss.

## Current implementation boundary

Protocol 1.22 extends `GitWorktreeState` with `blocking_paths` (at most 10 entries, each a tracked/untracked/ignored kind and at most 512 bytes of escaped relative-path display text) and `blocking_paths_truncated`. Directory paths retain their trailing slash; a single entry may represent an entire directory. These paths are informational only and must never be used for filesystem operations. Clients and daemon must upgrade together under exact protocol matching. Worktree removal still revalidates the complete raw status and refuses all tracked, untracked, or ignored content.

Protocol 1.23 adds typed Worktree base selections to `CreateWorktree` and `PrepareStartWork`: latest remote default branch (the default), or an explicit local ref. Start Work confirmations carry the resolved selection, readable ref, and pinned commit. The daemon refreshes remote bases before creation and rejects a changed ref or commit so clients must refresh and reconfirm. Remote failures never silently fall back to local state; explicit local refs remain offline-capable. No persistence migration is required.

Protocol 1.24 expands each Session with `status`: independent `lifecycle`, Provider `activity`, and an optional latest Attention Event. Lifecycle distinguishes fresh, starting, running, exited, failed, terminated, and disconnected. Activity distinguishes none (including shells and inactive processes), working, awaiting input, completed, interrupted, and recoverable failure. Attention carries one of input requested, successful completion, recoverable failure, or terminal failure, a positive per-Session monotonic identity bounded to `i64::MAX`, a nonnegative UTC millisecond timestamp, and pending, acknowledged, or resolved acknowledgement. Retaining the latest resolved event preserves sequence continuity without an event archive. Unknown values, invalid combinations, and older protocol versions fail closed. Snapshots and existing Session entity responses/events expose the expanded contract; current clients continue presenting the legacy `state` during this expand phase. Provider reducer and client presentation changes are separate work.

Protocol 1.25 adds the normalized `TurnInterrupted` Provider event and makes daemon-derived expanded Session status authoritative for Provider and process outcomes. Completion, input, recoverable failure, terminal failure, and interruption now carry stable replacement and deduplication semantics through existing Session responses and events.

Protocol 1.26 adds state-bound recoverable-failure dismissal. The request carries the exact per-Session Attention Event identity; the daemon accepts it only while that event is still the pending recoverable failure, persists acknowledgement without changing Session lifecycle or Provider activity, and publishes the updated Session to every connected client. Successful-completion and terminal-failure acknowledgement remains daemon-owned and occurs only after terminal history opens successfully. Opening input never acknowledges it; Provider recovery resolves it.

Protocol 1.27 removes the expand-phase legacy Session state from snapshots, deletion inspections, and exit events. Clients consume Session Lifecycle, Provider Activity, and Attention Events independently. A completed Provider turn remains a live, attachable `running` Session and is neither resumable nor deletable; only a terminal lifecycle controls those policies. Terminal attachment no longer acknowledges completion or failure attention, so acknowledgement requires the explicit state-bound attention request. The exact protocol minor is mandatory during the authenticated handshake, and mismatched clients receive both client and daemon versions in the refusal diagnostic.

Protocol 1.28 makes Resume Conversation daemon-authoritative. Snapshots include bounded persisted predecessor/successor links, and `ResumeSession` accepts any Session in a lineage. The daemon follows successors under the Worktree operation lock, returns the existing live leaf, creates exactly one successor from an eligible terminal leaf, or returns `SessionResumeUnavailable` with one typed refusal. Eligibility covers terminal lifecycle, verified Conversation identity, Provider resume capability/health/authentication, active canonical Worktree identity, and the absence of a successor at the transactional insert. Successful responses distinguish attachment from successor creation and identify the originally requested Session; clients derive attempt markers from lineage links without changing the user-selected Session name.

Protocol 1.29 adds daemon-backed Attention Notification preferences and one live delivery claim per newly created eligible Attention Event. Claims contain only the bounded Session display name and Attention Reason, are never reconstructed from persisted attention at startup or reconnect, and are consumed even when the claimant disconnects or presentation fails.

Protocol 1.30 adds per-connection foreground Session reporting. The daemon suppresses a pending delivery claim only when a connected foreground client reports the affected Session, automatically acknowledges visible successful completion and terminal failure, and leaves visible input and recoverable failure pending. Focus loss and connection loss remove only that client's foreground report; changing Session removes suppression from the previous Session. Repeated focus reports, snapshots, renames, and unchanged Attention Events do not create claims. Resolved-then-reentered conditions and materially different recoverable-failure categories receive a new per-Session Attention Event identity and one new live claim opportunity.

The frame codec, authenticated listener, multiplexed client, correlated responses, event delivery, session replay, and slow-client resynchronization are implemented. Unix additionally rejects replacement of a live or foreign-owned socket and applies mode `0600`. Windows rejects remote clients and creates every pipe instance with a protected owner-only DACL in addition to the runtime token. Connected-client SID verification remains defense-in-depth release hardening.
