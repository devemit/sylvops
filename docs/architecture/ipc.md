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

The first client request is `Hello`. It authenticates with a high-entropy token stored in the private runtime directory and negotiates an exact protocol version before other requests are accepted. Authentication comparisons are constant-time. Protocol 1.5 includes managed-worktree controls, provider listing/probing, provider-aware session creation, Codex resume, normalized provider events, bounded Git diff requests, workspace switching, and metadata-only entity renames. Opcode 10 carries correlated controls and opcode 11 carries unsolicited daemon events. Snapshots and entity events carry database-actor revisions; clients detecting a revision gap request a new snapshot.

`RemoveWorktree` includes a confirmation token derived from the checkout identity, branch, HEAD, and a clean porcelain-status result. The daemon refreshes all of that state and compares the token immediately before invoking non-forced removal. Tokens are authorization context for one observed state, not credentials.

Each connection has dedicated reader and writer ownership tasks. A frame read is never cancellation-interrupted by lifecycle or terminal events, and responses are correlated independently of event delivery.

## Backpressure

Each connection has a 256-item, 4 MiB send queue and a ten-second writer deadline. Lifecycle changes use bounded waiting; lag on the shared lifecycle broadcast produces `StateResynchronizationRequired` with the latest revision so the client can request a snapshot. Terminal output uses non-blocking enqueue and may be dropped only for the affected slow client, which then receives `ResynchronizationRequired` with parser-derived terminal state.

An attach boundary newer than the daemon's latest output sequence is treated as an explicit terminal resynchronization request. This lets a client recover with a parser snapshot if its own bounded local event queue reports loss.

## Current implementation boundary

The frame codec, authenticated listener, multiplexed client, correlated responses, event delivery, session replay, and slow-client resynchronization are implemented. Unix additionally rejects replacement of a live or foreign-owned socket and applies mode `0600`. Windows rejects remote clients and creates every pipe instance with a protected owner-only DACL in addition to the runtime token. Connected-client SID verification remains defense-in-depth release hardening.
