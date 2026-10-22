# Provider hooks and session status

On Windows, current Codex releases execute hooks through PowerShell rather than the legacy command processor. Managed private-runtime profiles use PowerShell's call operator for the quoted native relay path; the fixture exercises that same shell contract, including paths with spaces and Unicode. Correcting the command creates a new immutable profile and requires normal user review in Codex `/hooks`; SylvOps never transfers approvals to a changed command. Codex-written bounded model preferences are preserved without permitting changes to generated hook definitions.

Managed Codex launches and resumes probe `--help` and select `--no-daemon` when supported. Codex's shared app-server runtime otherwise owns the work outside the Session's process tree and runs hooks with the server's environment, which does not contain the Session's ephemeral hook credentials. Older CLIs that do not advertise this option keep their existing launch arguments. The application-owned profile and its user-reviewed trust remain stable. See the [Codex 0.163.0 hook runtime](https://github.com/openai/codex/blob/rust-v0.163.0/codex-rs/hooks/src/registry.rs) and [TUI server selection](https://github.com/openai/codex/blob/rust-v0.163.0/codex-rs/tui/src/lib.rs).

The daemon hook receiver binds only to IPv4 loopback and selects an ephemeral port. Every hook-capable managed Session receives its own high-entropy ephemeral bearer token, bound to its Provider, Session, and Worktree and injected only into that child environment. Headers, body size, global and per-Session rate, content type, connection concurrency, request duration, and identifier relationships are bounded or validated. Raw hook bodies are not persisted.

Provider payloads normalize to `PromptSubmitted`, `TurnStarted`, `PermissionRequested`, `UserInputRequested`, `SubagentStarted`, `SubagentStopped`, `TurnStopped`, `TurnInterrupted`, `TurnFailed`, and `SessionEnded`. Exact duplicate payloads and events arriving for a closed turn are ignored through bounded in-memory tracking. Unknown or malformed events are audited with a bounded refusal reason without persisting raw payloads.

Deterministic transitions:

- prompt/turn start → working, resolving obsolete attention;
- permission/user input request → awaiting input with pending input attention;
- turn stop with active subagents, background tasks, scheduled work, or exhausted tracking capacity → working without completion;
- turn stop without remaining work → completed with one pending successful-completion event while the Provider process remains live;
- user interruption → interrupted without successful-completion attention while the Provider process remains live;
- recoverable turn failure → recoverable failure with its bounded category and pending attention while the Provider process remains live;
- opening completed or terminal-failure history → acknowledges that exact event identity without changing lifecycle;
- explicitly dismissing a recoverable failure → acknowledges that exact identity without changing Provider activity; the same effective condition stays quiet until recovery or replacement;
- explicit Stop Session → terminated, resolving prior attention without creating completion;
- a clean plain-shell exit → exited with successful-completion attention;
- an unexpected Provider exit → failed with terminal-failure attention unless a trustworthy completion event already established completion;
- unverifiable process after daemon restart → disconnected.

SessionEnd and process exit reuse an existing completion identity instead of creating duplicate attention. A newer effective input, recoverable failure, completion, or terminal failure replaces the obsolete pending event with the next per-Session identity; interruption and resumed work resolve obsolete attention. Failed, terminated, and disconnected states cannot be resurrected by late hooks. Provider failure payloads persist only their allow-listed bounded category, never raw Provider details. Provider configuration is layered with recognizable application-owned settings; base user and repository configuration is not edited. Codex uses its application-owned profile. Claude Code uses a daemon-lifetime `--settings` file with direct observational HTTP handlers plus a structured, shell-free relay command for `SessionStart`, which Claude does not deliver to HTTP handlers.
