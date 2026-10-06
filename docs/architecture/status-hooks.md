# Provider hooks and session status

The daemon hook receiver binds only to IPv4 loopback and selects an ephemeral port. Every hook-capable managed Session receives its own high-entropy ephemeral bearer token, bound to its Provider, Session, and Worktree and injected only into that child environment. Headers, body size, global and per-Session rate, content type, connection concurrency, request duration, and identifier relationships are bounded or validated. Raw hook bodies are not persisted.

Provider payloads normalize to `PromptSubmitted`, `TurnStarted`, `PermissionRequested`, `UserInputRequested`, `SubagentStarted`, `SubagentStopped`, `TurnStopped`, `TurnFailed`, and `SessionEnded`. Exact duplicate payloads and events arriving for a closed turn are ignored through bounded in-memory tracking. Unknown or malformed events are audited with a bounded refusal reason without persisting raw payloads.

Deterministic transitions:

- prompt/turn start → running;
- permission/user input request → needs feedback;
- turn stop with active subagents, background tasks, or scheduled work → running;
- turn stop without subagents → finished unseen;
- recoverable turn failure → needs feedback while the provider process remains live;
- viewing a finished session → finished seen;
- explicit stop → terminated;
- unexpected exit → failed unless a trustworthy completion event already established completion;
- unverifiable process after daemon restart → disconnected.

Failed, terminated, and disconnected states cannot be resurrected by late hooks. Provider configuration is layered with recognizable application-owned settings; base user and repository configuration is not edited. Codex uses its application-owned profile. Claude Code uses a daemon-lifetime `--settings` file with direct observational HTTP handlers plus a structured, shell-free relay command for `SessionStart`, which Claude does not deliver to HTTP handlers.
