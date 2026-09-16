# PTY and process ownership

## Session actor

One daemon task owns each live session. It owns the PTY master/resize handle, process-tree controller, scrollback, sequence counter, terminal parser, and attachment lease. A bounded blocking-I/O worker exclusively owns the PTY writer so a child that stops reading cannot pin an async runtime thread. Other code communicates through bounded messages.

PTY output is retained as raw-byte chunks with monotonic sequence numbers, bounded by both configured bytes and a hard item count so tiny reads cannot create excessive queue overhead. Clients decode UTF-8 incrementally and use an established VT parser. If the requested sequence was evicted, the daemon reports an explicit gap and provides a parser-derived screen snapshot or clean redraw boundary.

Only one client is the interactive controller. Other attached clients are observers. Input and resize from observers are rejected. Detaching or losing the controller connection releases the lease; observers must explicitly reattach to claim it.

## Termination

Termination is idempotent: request graceful exit, wait a bounded interval, terminate the entire process tree, reap the root process, and persist exactly one terminal state.

- Unix: the PTY child must lead a dedicated process group/session; termination targets that group.
- Windows: the child is assigned to a Job Object with `KILL_ON_JOB_CLOSE`; production code should create it suspended, assign it, then resume it.

## Phase 2 boundary

The production plain-shell path uses `portable-pty`, a dedicated Unix process group, or a Windows kill-on-close Job Object. Windows assignment still occurs immediately after spawn and therefore has a containment race. Provider and release hardening are gated on a suspended-spawn wrapper or equivalent proof.

Alternate-screen replay and eviction in the middle of an ANSI sequence require explicit tests. Replaying arbitrary truncated bytes alone is insufficient.
