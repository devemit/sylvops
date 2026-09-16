# Provider hooks and session status

The daemon hook receiver binds only to IPv4 loopback and selects an ephemeral port. Every daemon start generates one high-entropy ephemeral bearer token that is injected only into managed Codex session environments. Headers, body size, rate, content type, connection concurrency, request duration, and identifier relationships are bounded or validated. Raw hook bodies are not persisted.

Provider payloads normalize to `PromptSubmitted`, `TurnStarted`, `PermissionRequested`, `UserInputRequested`, `SubagentStarted`, `SubagentStopped`, `TurnStopped`, and `SessionEnded`. Exact duplicate payloads and events arriving for a closed turn are ignored through bounded in-memory tracking. Unknown events are audited without mutating state.

Deterministic transitions:

- prompt/turn start → running;
- permission/user input request → needs feedback;
- turn stop with subagents → running;
- turn stop without subagents → finished unseen;
- viewing a finished session → finished seen;
- explicit stop → terminated;
- unexpected exit → failed unless a trustworthy completion event already established completion;
- unverifiable process after daemon restart → disconnected.

Failed, terminated, and disconnected states cannot be resurrected by late hooks. Provider configuration is layered with a recognizable, uniquely named application-owned profile; base user and repository configuration is not edited.
