# Testing

Run formatting, linting, and the full workspace suite:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets
```

Tests cover frame parsing/version rejection, configuration preservation, SQLite migration/snapshots, restart reconciliation, authentication rejection, concurrent clients, clean shutdown, bounded scrollback, PTY input/output/replay, and process-tree cleanup. Worktree coverage includes branch/ref validation, managed containment, dirty-removal refusal including ignored files, branch preservation, and a two-worktree/two-session scenario. Provider/status coverage includes structured Codex and Claude arguments, selector validation, credential filtering, observational hook configuration, verified conversation-identity transitions, guarded resume, rate limiting, duplicate subagent idempotence, terminal-state protection, and attention ordering. Platform process, path, Git, transport, and TUI behavior must pass on Windows, Linux, and macOS before release promotion.

Suites use temporary Git repositories, the general fake agent, and fake Codex and Claude executables that support bounded probes, hook JSON, interactive input, completion, and resume. The fake-Claude acceptance path additionally covers concurrent resume refusal, restart recovery, and provider-owned descendant cleanup. Provider end-to-end tests and the full quality gates run in native hosted Windows, Linux, and macOS CI. Future GitHub tests use fake `gh` binaries. No default test may depend on network access or authenticated provider accounts. All waits need explicit deadlines and cleanup must run after failure.

The real Claude Code smoke is explicit and opt-in. It requires a pre-existing supported Claude Code login and a profile whose normal permission behavior produces both `AskUserQuestion` and `PermissionRequest` attention. It creates two concurrent Sessions, verifies both attention types, completion, guarded resume, prompt privacy, and process cleanup inside a five-minute deadline. It never prints terminal transcripts, credentials, or environment values.

On PowerShell:

```text
$env:SYLVOPS_RUN_REAL_CLAUDE_SMOKE = '1'
cargo test -p sylvops-test-support --test real_claude_smoke -- --ignored --exact real_claude_account_smoke_is_bounded_and_redacted --nocapture
```

On POSIX shells:

```text
SYLVOPS_RUN_REAL_CLAUDE_SMOKE=1 cargo test -p sylvops-test-support --test real_claude_smoke -- --ignored --exact real_claude_account_smoke_is_bounded_and_redacted --nocapture
```

The reusable `Real Claude Code smoke` workflow runs the same test only on a self-hosted runner carrying the `claude-code-smoke` label and protected by the `real-claude-smoke` environment. It can be dispatched manually, and release promotion calls it for the exact commit being promoted before packaging can begin. The runner must already be authenticated; the workflow does not accept, copy, or inject provider credentials.
