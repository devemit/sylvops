# Testing

Run formatting, linting, and the full workspace suite:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets
```

Tests cover frame parsing/version rejection, configuration preservation, SQLite migration/snapshots, restart reconciliation, authentication rejection, concurrent clients, clean shutdown, bounded scrollback, PTY input/output/replay, and process-tree cleanup. Worktree coverage includes branch/ref validation, managed containment, dirty-removal refusal including ignored files, branch preservation, and a two-worktree/two-session scenario. Provider/status coverage includes structured Codex arguments, selector validation, credential filtering, observational hook configuration, rate limiting, duplicate subagent idempotence, terminal-state protection, and attention ordering. Platform process, path, Git, transport, and TUI behavior must pass on Windows, Linux, and macOS before release promotion.

Suites use temporary Git repositories, the general fake agent, and a fake Codex executable that supports bounded probes, hook JSON, interactive input, completion, and resume. The fake-Codex MVP end-to-end test and full quality gates pass in Linux Docker, native Windows, and hosted Windows/Linux/macOS CI. Future GitHub tests use fake `gh` binaries. No default test may depend on network access or authenticated provider accounts. All waits need explicit deadlines and cleanup must run after failure.
