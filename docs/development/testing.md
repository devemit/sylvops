# Testing

Run formatting, linting, and the full workspace suite:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets
```

Tests cover frame parsing/version rejection, configuration preservation, SQLite migration/snapshots, restart reconciliation, authentication rejection, concurrent clients, clean shutdown, bounded scrollback, PTY input/output/replay, and process-tree cleanup. Worktree coverage includes branch/ref validation, managed containment, dirty-removal refusal including ignored files, branch preservation, and a two-worktree/two-session scenario. Provider/status coverage includes structured Codex and Claude arguments, selector validation, credential filtering, observational hook configuration, verified conversation-identity transitions, guarded resume, rate limiting, duplicate subagent idempotence, terminal-state protection, and attention ordering. Platform process, path, Git, transport, and TUI behavior must pass on Windows, Linux, and macOS before release promotion.

Desktop acceptance combines approved headless Iced pixels with independent semantic assertions for Details text, Session-action bounds, navigator row geometry, fixed action-strip placement, and bottom-row reveal. The same hosted Windows, Linux, and macOS matrix exercises wide, compact, and narrow layouts. Terminal responsiveness uses deterministic fake-clock ordering/losslessness checks as the hard gate; the real daemon/fake-provider PTY smoke reports p95 and maximum latency as diagnostic evidence. See [Desktop terminal performance](terminal-performance.md) for the focused commands and latest recorded measurements.

Suites use temporary Git repositories, the general fake agent, and fake Codex, Claude, and `gh` executables. The fake-Claude acceptance path covers bounded probes, hook JSON, interactive input, completion, concurrent resume refusal, restart recovery, and provider-owned descendant cleanup. The fake-`gh` path covers GitHub.com and enterprise hosts, live authentication/permission/offline transitions and recovery, rate limits, timeouts, cancellation, malformed and excessively nested JSON, oversized output, fields, and pages, control-character sanitization, unavailable items, cursor pagination, cache refresh, and canonical browser targets. Provider and GitHub end-to-end tests and the full quality gates run in native hosted Windows, Linux, and macOS CI. No default test may depend on network access or authenticated provider accounts. All waits need explicit deadlines and cleanup must run after failure.

## GitHub release proof

The GitHub workflow is gated at its public seams:

- `sylvops-test-support --test github_connection` drives the native fake `gh` process through connection, browsing, hostile-output, bounds, cancellation, refresh, and recovery cases.
- `sylvops-test-support --test start_work` drives authenticated IPC from association and Issue loading through duplicate handling, explicit parallel work, partial success, restart, and Retry Session. After daemon shutdown it inspects SQLite and logs: only association and Work Source identity may remain; credentials, remote bodies/comments, generated prompts, Additional instructions, searches, filters, and result data must be absent.
- `sylvops-cli` renderer tests prove status/list/view present the same bounded daemon response types without adding a mutation path.
- `sylvops-desktop` pins headless visual baselines for wide, compact, and narrow GitHub loading, empty, detail, error, association-confirmation, and partial-success states, with semantic assertions alongside pixels.
- `sylvops-tui` reducer/render tests cover every documented GitHub shortcut, nested list/detail/filter/Start Work/recovery state, contextual help, duplicate/parallel behavior, terminal transition, and preservation of existing keys.
- The daemon browser adapter constructs one canonical HTTPS target from typed identity and one structured native launcher command for Windows, macOS, or Linux. GitHub text never supplies an executable, argument list, or alternate target.

Run these focused seams during development, then the complete formatting, Clippy, and workspace test gate above before release promotion. Remote Issue or Pull Request changes are observational and must never stop, delete, or otherwise mutate local Worktrees or Sessions.

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
