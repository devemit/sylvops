# Provider hooks and session status

On Windows, current Codex releases execute hooks through PowerShell rather than the legacy command processor. Managed private-runtime profiles use PowerShell's call operator for the quoted native relay path; the fixture exercises that same shell contract, including paths with spaces and Unicode. Correcting the command creates a new immutable profile and requires normal user review in Codex `/hooks`; SylvOps never transfers approvals to a changed command. Codex-written bounded model preferences are preserved without permitting changes to generated hook definitions.

Managed Codex launches and resumes require native remote-terminal authentication and stdio app-server support. The Session actor owns both the terminal and a private backend process tree; a credential-protected loopback bridge forwards their protocol without storing transcripts or credentials. Actual command/file/permission approvals, user-input requests, and MCP elicitations create input attention. Confirmed request resolution clears it only after all outstanding requests resolve. A typed asynchronous question interrupts its turn and remains Waiting for input until an explicit reply starts the next turn. This uses native protocol controls, not terminal text or process suspension. The existing application-owned profile and its user-reviewed hook trust remain unchanged.

The daemon hook receiver binds only to IPv4 loopback and selects an ephemeral port. Every hook-capable managed Session receives its own high-entropy ephemeral bearer token, bound to its Provider, Session, and Worktree and injected only into that child environment. Headers, body size, global and per-Session rate, content type, connection concurrency, request duration, and identifier relationships are bounded or validated. Raw hook bodies are not persisted.

Provider payloads normalize to `PromptSubmitted`, `TurnStarted`, `PermissionRequested`, `PermissionCheckStarted`, `UserInputRequested`, `InputRequestOpened`, `InputRequestsResolved`, `ToolStarted`, `ToolCompleted`, `SubagentStarted`, `SubagentStopped`, `TurnStopped`, `TurnInterrupted`, `TurnFailed`, and `SessionEnded`. Exact duplicate payloads and events arriving for a closed turn are ignored through bounded in-memory tracking. Unknown or malformed events are refused without persisting raw payloads. Older observational Codex hooks cannot clear a structured waiting condition.

Deterministic transitions:

- prompt/turn start → working, resolving obsolete attention;
- permission/user input request → awaiting input with pending input attention;
- explicit successful opening/review of input → acknowledged attention while still awaiting input;
- actual Codex request resolution, or acceptance of a reply to a paused asynchronous question → working, resolving input attention;
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
