# Process model

## Roles

The `sylvops` executable has one authoritative service role and two replaceable client roles:

- `sylvops daemon run` owns SQLite, PTYs, child processes, Git mutations, hooks, and IPC subscriptions.
- `sylvops desktop` is the internal native-window entry point. Users launch it through `sylvops` or `sylvops up [PATH]`; it owns presentation and input routing only.
- `sylvops tui` connects to that daemon and owns only presentation and input routing.

Non-interactive CLI commands are short-lived daemon clients. Closing any client must leave the daemon and sessions running.

```text
Desktop clients ─┐
TUI clients ─────┼─ local IPC ─► daemon ─┬─ database actor
CLI clients ─────┘                       ├─ Git coordinators
                                        ├─ session actors ─► PTYs/process trees
                                        └─ loopback hook receiver
```

## Concurrency

Each session actor exclusively owns its PTY handles, child controller, attachment lease, output sequence, and scrollback. Blocking PTY reads occur on dedicated threads and feed bounded actor channels. Client delivery queues are bounded independently so a slow client cannot block PTY draining.

Database access is serialized through a dedicated actor. Git mutations are serialized by short-lived per-project operation locks; session creation and removal additionally coordinate through a per-worktree lock. The lock maps themselves are never held across an async operation. No database transaction or global lock may span an asynchronous wait or subprocess execution.

## Restart behavior

The daemon never adopts a process based only on a persisted PID. Phase 1 transactionally marks persisted active-looking sessions `disconnected` and clears their PIDs on restart. Preserving and securely re-adopting a live PTY process across a daemon crash is outside the current scope.

## Platform split

Unix uses a Unix-domain socket and a dedicated process group/session for each PTY. Windows uses a named pipe and a Job Object. Detachment, peer authentication, executable resolution, and path identity require separate platform implementations behind shared traits.
