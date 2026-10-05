# Claude Code provider

Claude Code follows the same adapter boundary as Codex: bounded discovery and health probes, structured launch arguments, a reviewed environment, and managed hook configuration that preserves effective user, project, and organization settings. SylvOps launches the verified native executable in the selected Worktree and never selects a permission mode.

The daemon adds one atomic, application-owned `--settings` layer for its lifetime. The layer contains no credentials and does not edit Claude user or Project files. It uses direct observational HTTP hooks where Claude supports them. Because Claude Code does not run HTTP handlers for `SessionStart`, that event uses the fixed SylvOps hook relay in documented executable-and-argument form; the relay forwards the unchanged bounded payload to the same authenticated loopback receiver. Per-Session credentials and identity headers come only from the child environment and are invalidated on every failure or terminal path.

Model and effort values are validated by the Claude adapter and passed as separate arguments. An optional initial prompt is a transient final argument and is excluded from persisted arguments, logs, audit events, and SQLite.

A managed Claude Session expects an authenticated `SessionStart` within 15 seconds. If the process is still live without it, the Session becomes needs feedback with bounded recovery guidance while its Terminal remains attached and usable. A later valid event captures the verified Claude session ID, clears the warning, and restores running state.

No implementation may infer permission or input requests by scraping terminal text. Default tests use the fake agent and require no authenticated account.
