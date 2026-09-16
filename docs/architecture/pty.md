# PTY and process ownership

## Session actor

One daemon task owns each live session. It owns the PTY master/resize handle, process-tree controller, scrollback, sequence counter, terminal parser, and attachment lease. A bounded blocking-I/O worker exclusively owns the PTY writer so a child that stops reading cannot pin an async runtime thread. Other code communicates through bounded messages.

PTY output is retained as raw-byte chunks with monotonic sequence numbers, bounded by both configured bytes and a hard item count so tiny reads cannot create excessive queue overhead. Clients decode UTF-8 incrementally and use an established VT parser. If the requested sequence was evicted, the daemon reports an explicit gap and provides a parser-derived screen snapshot or clean redraw boundary.

Only one client is the interactive controller. Other attached clients are observers. Input and resize from observers are rejected. Detaching or losing the controller connection releases the lease; observers must explicitly reattach to claim it.

## Termination

Termination is idempotent: request graceful exit, wait a bounded interval, terminate the entire process tree, reap the root process, and persist exactly one terminal state.

- Unix: the PTY child must lead a dedicated process group/session; termination targets that group.
- Windows: SylvOps creates ConPTY pipes and a kill-on-close Job Object before process creation. `CreateProcessW` receives both `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` and `PROC_THREAD_ATTRIBUTE_JOB_LIST` in one `STARTUPINFOEX` list, so no uncontained post-spawn interval exists.

## Phase 2 boundary

Unix uses `portable-pty` and requires the child to lead a dedicated process group. Windows uses the application-owned documented ConPTY boundary because `portable-pty` does not expose process-creation attributes. Handles are RAII-owned, arguments and Unicode environment blocks are constructed without a shell, and a partial failure cannot return a running uncontained process.

Alternate-screen replay and eviction in the middle of an ANSI sequence require explicit tests. Replaying arbitrary truncated bytes alone is insufficient.
