---
title: Lock the beta acceptance contract
label: wayfinder:grilling
state: closed
assignee: null
blocked_by: []
---

> **Historical release-planning record:** this ticket belongs to the retired beta-phase plan and is not an active roadmap item or release gate.

## Question

Which exact existing desktop, TUI, CLI, daemon, worktree, Shell, Codex, recovery, packaging, and cross-platform behaviors are release-blocking for `v0.1.0-beta.1`, and which observed imperfections are explicitly acceptable beta limitations?

## Resolution

`v0.1.0-beta.1` is accepted only when every release-blocking clause below has passing evidence for the exact candidate commit. A failure on any platform to which a clause applies blocks the release. A behavior may not be waived as "beta quality" unless it is listed under **Acceptable beta limitations**.

This contract fixes the scope of acceptance. The separate [fresh-machine release proof](release-proof.md) ticket owns the executable runbook and the evidence record, and [beta cut](beta-cut.md) owns the final ship/delay decision. Neither is resolved here.

### Release-blocking behavior

| ID | Area | Pass condition for the beta |
|---|---|---|
| B1 | Packaging and release | The workspace and tag are exactly `0.1.0-beta.1`/`v0.1.0-beta.1`. The gated release workflow builds locked Windows x86_64, Linux x86_64, macOS x86_64, and macOS arm64 archives only after all CI jobs pass; it publishes one checksum manifest and provenance attestations. Each archive installs or extracts on a clean supported host without Rust or a compiler, contains the documented executable and supporting files, and its bundled instructions describe the UI it actually opens. |
| B2 | CLI and desktop entry | `sylvops`, `sylvops up [PATH]`, and `sylvops open [PATH]` start or reuse one daemon and open the native desktop. Supplying a repository idempotently selects or registers it; omitting provider flags never starts a session. An empty snapshot presents the ordered Workspace → Project/root Worktree → Session flow, Shell is ready by default, attachment remains explicit, and closing/reopening the window leaves daemon-owned sessions running. |
| B3 | TUI and non-interactive CLI | `sylvops tui` remains a usable keyboard-first client for the same authoritative state. It can navigate the hierarchy, create the supported entities through typed IPC, attach/detach, render Terminal/Changes/Details, confirm stop/removal, and exit without stopping sessions. The documented workspace, project, worktree, provider, session, snapshot, doctor, and daemon commands either complete successfully or return a bounded actionable error. |
| B4 | Daemon, IPC, and persistence | One daemon owns SQLite, Git mutations, PTYs, and provider processes. Control IPC is local-only, authenticates the first frame, exact-matches protocol 1.7, bounds frames and client queues, and rejects malformed or unauthorized input. Migrations are embedded, checksum-validated, and transactional. Restart never adopts a persisted PID: active-looking records become `disconnected` idempotently while terminal records and metadata remain intact. |
| B5 | Repository and worktree safety | An existing Git repository can be registered and its root checkout selected. Managed worktree creation validates branch/base, uses structured Git arguments, stays under the private managed root, and does not run Git hooks. Removal is explicit and state-bound, refuses live sessions and tracked, untracked, or ignored content, revalidates identity immediately before a non-forced removal, and preserves the branch. Missing worktrees become unavailable; unexpected external worktrees are observed but not imported or mutated. |
| B6 | Shell PTY lifecycle | A Shell session starts in the selected checkout, accepts input and resize, emits bounded replayable output, survives client detach/reconnect while the daemon remains alive, and has a single controller with read-only observers. Stop and daemon shutdown terminate and reap the complete process tree. Windows creation applies ConPTY and the kill-on-close Job Object atomically; Unix verifies a dedicated process group. |
| B7 | Codex provider | Discovery selects only a canonical native Codex binary from the bounded supported layouts and never executes package-manager or shell shims. Version/login probes are bounded. An unavailable or unauthenticated selection gives install/login guidance, a daemon-backed **Check again**, and a Shell fallback. With an already authenticated installation, launch uses structured arguments and a reviewed environment; observational hooks drive deterministic attention, capture a verified external ID, and resume creates a new session record. No credential is copied, stored, or printed. |
| B8 | Recovery, history, and review | Reconnect repairs bounded client output loss from daemon parser state. After daemon restart, unverifiable live sessions are visibly `disconnected` and terminal output is explicitly unavailable rather than implied durable. Finished, failed, terminated, and disconnected records expose their retained outcome metadata; only eligible provider states with a verified external ID can resume. A bounded read-only Git diff is available and reports truncation. |
| B9 | Diagnostics and trust boundaries | `sylvops doctor` checks the state directory, Git, authenticated daemon health, provider health, and a real PTY spawn/output/reap path with bounded redacted output. Repository paths/content, Git/provider output, hook payloads, executable discovery, configuration, and IPC clients remain untrusted; failures do not expose credentials, execute repository-defined commands implicitly, force-remove worktrees, or leave an uncontained child tree. |
| B10 | Cross-platform gate | Formatting, Clippy with warnings denied, and the complete workspace test suite pass on the same candidate commit on native hosted Windows, Linux, and macOS. Native IPC, PTY, path, and process-tree coverage must execute on the applicable operating system; compilation or source review from another OS is not a substitute. |

The repository evidence that makes these clauses enforceable includes `.github/workflows/ci.yml`, `.github/workflows/release.yml`, the platform PTY/IPC implementations, and the integration suites in `crates/sylvops-daemon/tests` and `crates/sylvops-test-support/tests`. In particular, the suites exercise authenticated concurrent clients, restart reconciliation, Shell detach/replay, process-tree cleanup, bounded scrollback, managed-worktree refusal/removal, fake-Codex hooks/attention/resume, and provider discovery fixtures. Desktop and TUI unit tests cover bounded state restoration, hierarchy/form behavior, safe inactive-session actions, first-run/provider recovery copy, embedded terminal handling, responsive rendering, and explicit detach.

### Required release evidence

Evidence is admissible only when it names the candidate commit, target/architecture, artifact checksum where applicable, result, and any failure. Secrets and raw provider environment values must be absent. A source inspection, cross-compilation result, or successful run from a different commit is not acceptance evidence.

1. **Automated evidence on the candidate commit:** all three commands below pass in every `ci.yml` job on `windows-latest`, `ubuntu-latest`, and `macos-latest`:

   ```text
   cargo fmt --all -- --check
   cargo clippy --workspace --all-targets --all-features -- -D warnings
   cargo test --workspace --all-targets
   ```

   The tag workflow must then pass its `verify` dependency and build all four locked release archives. The published files, `SHA256SUMS`, and attestations must agree with those workflow artifacts.

2. **Windows x86_64 fresh host:** record checksum verification, ZIP extraction, launcher and direct CLI startup without Rust/Visual Studio, native desktop launch, the complete first-run flow, repository reopen idempotence, a Shell attach/detach/reopen/input/resize/stop cycle, descendant cleanup, guarded dirty and clean worktree removal, TUI startup, `doctor`, unavailable/unauthenticated Codex recovery, and one opt-in real-Codex launch/hook/resume smoke using pre-existing authentication. The host must be Windows 10 1809 or newer.

3. **Linux x86_64 fresh host:** record checksum verification, archive or installer use without Rust, native desktop launch under a supported graphical session, the same repository/Shell/worktree/detach/recovery/TUI/doctor scenarios, Unix process-group cleanup, provider recovery, and one opt-in real-Codex launch/hook/resume smoke using pre-existing authentication.

4. **macOS fresh hosts:** both x86_64 and arm64 archives must verify, extract, launch natively, and pass `doctor` without Rust. At least one host of each architecture must exercise repository opening and a Shell attach/detach/stop cycle. One macOS host must additionally exercise managed worktree create/refusal/removal, TUI, restart recovery, provider recovery, and an opt-in real-Codex launch/hook/resume smoke using pre-existing authentication.

5. **Client and recovery observation:** on each operating system, closing both native clients must leave an attached Shell alive; reopening must recover attachment through the daemon; restarting the daemon must instead retain the record as `disconnected` without claiming terminal-output recovery. A failed descendant-cleanup, stale-state removal, authentication, or protocol-version test is always release-blocking.

The release-proof ticket may make these observations reproducible or add stricter checks, but it may not weaken or omit them.

### Acceptable beta limitations

These are non-blocking only when the behavior is explicit in the shipped UI or documentation and does not violate a release-blocking clause:

- Distribution is by portable, unsigned archives. Windows signing and macOS signing/notarization are not beta requirements; any platform warning and the safe user action needed to continue must be documented and demonstrated on the clean host. Installers do not edit `PATH` automatically.
- Terminal scrollback is bounded and memory-only. It survives client detach, not daemon restart. A daemon crash/restart disconnects the old process record rather than attempting unsafe PTY or PID adoption.
- Only one client controls a session at a time; other attachments are read-only. The desktop forwards bounded wheel events when an alternate-screen TUI requests mouse reporting, but terminal click and drag forwarding remain unavailable so native text selection is preserved.
- Shell and Codex are the only enabled providers. SylvOps does not install Codex, perform `codex login`, or manage provider credentials. The TUI retains individual management forms; the desktop is the supported guided first-run path.
- External worktrees are discovered read-only but cannot be imported. Root/external checkouts cannot be removed by SylvOps, managed removal is never forced, and branches are not deleted.
- Raw provider transcripts, prompts, terminal bytes, searches, form contents, and credentials are not persisted. Inactive records have no deletion or automatic-retention UI.
- Git review is a bounded unified diff. Binary/oversized content may be replaced by a marker or truncation notice; history, file finding, and Git grep are absent.
- Renames change SylvOps display metadata only. They do not rename repository directories, worktree paths, or Git branches.
- Windows connected-client SID verification is defense-in-depth beyond the beta boundary; the owner-only named-pipe DACL plus the per-start authentication token remain mandatory.

### Explicitly post-beta

The beta must not add or advertise Claude, GitHub issue/pull-request views, commit/push/merge/PR actions, branch deletion, remote execution, notifications, file finding, Git grep, history/pruning tools, external-worktree import, persisted terminal scrollback, terminal click/drag forwarding, auto-update, signed/native installers, signing, or notarization. Stable-release migration compatibility, soak/resource, diagnostics, and security reviews also remain later gates.

### Audit result at the decision point

The implementation audit was performed from `origin/main` at `7e70584`. Existing source and tests substantiate the automated side of B2–B10, and the user reports the merged Windows/Linux/macOS matrix green. That is not yet sufficient release evidence under this contract.

Two current failures block `v0.1.0-beta.1` until they are resolved and evidenced by the later tickets:

1. **Fresh-machine proof is absent.** No candidate artifact evidence currently demonstrates B1 or the target-specific observations above; [release-proof.md](release-proof.md) remains open for that work.
2. **The Windows archive instructions describe the wrong client.** `packaging/windows/Start-SylvOps.ps1` invokes `sylvops open`, which launches the native desktop, while `packaging/windows/QUICKSTART.txt` says it opens the terminal UI and lists obsolete TUI keys. A package whose bundled first-run instructions contradict its actual entry point fails B1. This audit records the blocker but does not repair packaging under the decision-only ticket.

No post-beta feature is required to clear either blocker.
