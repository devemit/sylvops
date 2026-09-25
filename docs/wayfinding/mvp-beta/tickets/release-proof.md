---
title: Define fresh-machine release proof
label: wayfinder:research
state: closed
assignee: null
blocked_by:
  - Lock the beta acceptance contract
  - Make Codex discovery work for supported local installations
  - Choose the first-run and provider-recovery experience
---

## Question

Which automated and manual checks on clean Windows, Linux, and macOS hosts are sufficient evidence that the archives, checksums, launchers, daemon lifecycle, Git workflow, Shell workflow, fake Codex workflow, and opt-in real Codex smoke test are ready to publish?

## Resolution

The process below is the only admissible fresh-machine proof for `v0.1.0-beta.1`. It implements every B1–B10 evidence requirement from the [beta acceptance contract](beta-acceptance-contract.md); it does not waive or reinterpret any clause. Closing this research ticket means the proof process is defined and executable, not that the beta has passed it.

The release remains blocked until the exact candidate has a complete passing evidence bundle and [beta cut](beta-cut.md) records the ship decision. No source inspection, cross-compilation, local developer build, previous commit, or result from another platform substitutes for a required native result.

### Evidence record

Create one immutable evidence directory named with the full 40-character candidate commit. Keep an `index.md` plus plain-text or Markdown transcripts and screenshots beneath it. The index has one row per check or attempt with these fields:

| Field | Required value |
|---|---|
| Check | One stable check ID from this ticket. Reruns add an attempt suffix; they do not erase a failure. |
| Candidate commit | Full 40-character commit tested. Every row must match the candidate. |
| Target | `windows`, `linux`, `macos`, or `release-set`. Record the exact OS/version or runner image in the supporting evidence. |
| Architecture | `x86_64`, `arm64`, or `multi-target` only for the release-set staging row. Record the observed native architecture, never an assumed one. |
| Artifact | Exact archive filename, or `N/A` only when no artifact is exercised. |
| SHA-256 | Lowercase 64-character archive digest, or `N/A` only when the artifact is `N/A`. |
| Date | UTC ISO-8601 date and time. |
| Result | `PASS`, `FAIL`, `BLOCKED`, or `NOT_RUN`. Blank and “expected to pass” are invalid. |
| Evidence | Workflow run/job URL or relative transcript/screenshot path, plus the exact failure/blocker when not passing. |

Transcripts must start with the same metadata and the host OS/build and native architecture. They may contain the commands, bounded command output, and UI observations needed for the check, but never credentials, tokens, raw environment dumps, or provider prompts/transcripts beyond the deliberately harmless smoke prompt. Record GitHub-hosted runner names and architecture from the job itself. Preserve failed evidence and add a new attempt after a fix.

### Release workflow gate

Before a candidate tag is pushed, configure the GitHub `beta-release` environment with required reviewers and no bypass for the release operator. Absence of that protection is `REL-GATE: FAIL`; do not push the tag. The repository cannot enforce the reviewer setting in YAML, so the environment settings page and reviewer list are part of the transcript.

For a candidate tag, `.github/workflows/release.yml` does the following in order:

1. Runs the reusable native Windows, Linux, and macOS CI matrix.
2. Verifies that the tag is exactly `v0.1.0-beta.1` and the workspace is exactly `0.1.0-beta.1`.
3. Builds the four locked native archives only after CI succeeds.
4. Validates each archive's exact file manifest and bundled launch instructions, then creates a provenance attestation for that archive.
5. Downloads those exact four workflow artifacts, creates one `SHA256SUMS`, revalidates the complete set, and uploads `release-candidate-<commit>`.
6. Waits at the protected `beta-release` environment before downloading and revalidating that same staged artifact for publication.

While publication is waiting, download the staged candidate without modifying or recompressing it. From a checkout of the same commit, run:

```text
pwsh ./scripts/validate-release.ps1 -DistDirectory <staged-directory> -ExpectedTag v0.1.0-beta.1
```

Verify each staged archive's provenance from a connected audit host and save the output:

```text
gh attestation verify <archive> -R devemit/sylvops
```

The attestation result must identify the expected repository, release workflow, candidate commit, and archive digest. GitHub documents this verification in [Using artifact attestations](https://docs.github.com/en/actions/how-tos/secure-your-work/use-artifact-attestations/use-artifact-attestations). `ART-*` fails if the archive is absent, the checksum differs, package validation fails, the attestation is missing/invalid, or its subject commit/workflow differs.

Only beta-cut approval may release the environment gate. Any missing or failed record leaves the gate unapproved and the release unpublished.

### Required automated and artifact rows

| Check ID | Required evidence |
|---|---|
| `REL-GATE` | Protected `beta-release` environment and pending deployment for this run. |
| `CI-WIN-X64` | `windows-latest`: all three required Cargo commands pass at the candidate commit. |
| `CI-LINUX-X64` | `ubuntu-latest`: all three required Cargo commands pass at the candidate commit. |
| `CI-MACOS-NATIVE` | `macos-latest`: all three required Cargo commands pass; record the runner's actual native architecture. |
| `REL-STAGE` | Tag/workspace match; all packaging jobs depend on successful CI; exactly four staged archives and one `SHA256SUMS`; publication is still waiting. |
| `ART-WIN-X64` | `sylvops-windows-x86_64.zip`: checksum, exact contents, corrected desktop/TUI instructions, and provenance pass. |
| `ART-LINUX-X64` | `sylvops-linux-x86_64.tar.gz`: checksum, exact contents, executable, bundled instructions, and provenance pass. |
| `ART-MAC-X64` | `sylvops-macos-x86_64.tar.gz`: checksum, exact contents, native executable, bundled instructions, and provenance pass. |
| `ART-MAC-ARM64` | `sylvops-macos-aarch64.tar.gz`: checksum, exact contents, native executable, bundled instructions, and provenance pass. |

The CI transcripts must include `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, and `cargo test --workspace --all-targets`. The full suite must visibly execute the fake-Codex hook, attention, external-ID, and resume coverage. The release validator is additional evidence, not a replacement for any command.

### Clean-host rules

A clean host is a fresh supported OS installation or fresh VM snapshot with Git and the normal graphical/runtime prerequisites, but no Rust toolchain, compiler, source checkout, prior SylvOps state, or previously extracted SylvOps archive. Record installed prerequisites and demonstrate that `rustc`/`cargo` are absent. The real-Codex check is the one exception: it uses a separate clean profile or snapshot with an already installed and authenticated Codex binary. SylvOps must not install Codex or perform login.

Use only the staged archive bytes. Copy the matching `SHA256SUMS` with the archive and verify the single named digest natively before extraction. Record the calculated digest in every clean-host row. Do not use an artifact from a different workflow attempt even when the filename is the same.

Minimum hosts:

- Windows x86_64, Windows 10 1809 or newer;
- Linux x86_64 under a supported graphical desktop session;
- macOS x86_64;
- macOS arm64.

The unavailable/unauthenticated provider case and the pre-authenticated real-Codex case may use separate clean snapshots of the same target and architecture. Record both host identities. Do not rename, hide, or tamper with a real Codex installation to manufacture the unavailable case.

### Manual scenarios

Each scenario produces its own evidence row. A UI check records the observed labels/actions and at least one screenshot; a command check records exit status and bounded output.

#### `HOST-INSTALL`

1. Record OS version/build, native architecture, absence of Rust/compiler, candidate commit, artifact name, and verified checksum.
2. Extract without installing a toolchain. Confirm only the locked package files are present.
3. Run `sylvops --version` (or `sylvops.exe --version`) and confirm `0.1.0-beta.1`.
4. On Windows, use `Start SylvOps.cmd` and direct `sylvops.exe up <repo>`. The launcher must open the native desktop described by `QUICKSTART.txt`.
5. On Linux/macOS, run the extracted executable directly, then `sylvops up <repo>` under the graphical session.
6. If the unsigned-build warning appears, record the exact warning and the documented narrow override. On macOS use Privacy & Security → Open Anyway after checksum verification; never disable Gatekeeper globally.

#### `DESKTOP-ENTRY`

1. With a fresh state directory, run `sylvops` without a path. Confirm one daemon starts and the empty native desktop presents Workspace → Project/root Worktree → Session in that order, with Shell ready by default and attachment explicit.
2. Register a disposable Git repository through the desktop. Run `sylvops up <repo>` and `sylvops open <repo>` twice. Confirm the same project/root checkout is selected without duplicate entities or an implicit session.
3. Close and reopen the desktop while a Shell is active. Confirm the daemon and Shell continue.

#### `CLI-TUI`

1. Save `daemon status`, `snapshot`, `provider list`, and `provider probe codex` output. Exercise workspace add, project add, worktree create/status, session create/attach/detach/stop, worktree remove with confirmation, and daemon start/status/stop. A deliberate bad ID/path must return a bounded actionable error.
2. Run `sylvops tui`. Navigate project → worktree → session, create supported entities with the forms, attach/detach, and render Terminal/Changes/Details. Confirm stop/removal prompts and quit with `q`; the Shell must continue.
3. Use the current keys: Tab/Shift+Tab changes focus, arrows or j/k navigate, Left/Right or h/l collapse/expand, 1/2/3 selects views, Enter performs the explicit primary action, `n` creates, `d` stops/removes, `g` loads the bounded diff, and Ctrl+] detaches.

#### `SHELL-LIFECYCLE`

1. Create a Shell in the selected root checkout and prove its working directory.
2. Attach, send input, resize several times, and generate more output while detached. Reattach from desktop, TUI, and CLI and observe replay/reconnect without duplicated or missing current screen state.
3. Attach two clients. Confirm exactly one controller accepts input and the observer is read-only.
4. Start a uniquely identifiable descendant that appends a timestamp to a temporary heartbeat file. Stop the session and verify the heartbeat stops changing and both parent and descendant disappear. Repeat with graceful daemon shutdown. Record native process inspection, not only the UI state.

#### `WORKTREE-SAFETY`

1. Select the repository root checkout and create a managed worktree from an explicit base/branch. Confirm it is beneath the private managed root and its branch/base are correct.
2. While a session is live, confirm removal is refused.
3. Independently introduce tracked, untracked, and ignored content; refresh status and confirm each dirty state blocks removal.
4. Change state after obtaining removal confirmation and confirm the stale confirmation is refused.
5. Return the worktree to an exactly clean state, remove it non-forcibly, and confirm the path is gone but the branch remains.
6. Make a registered path missing and add an external Git worktree. Restart/reconcile: the missing checkout becomes unavailable; the external checkout is observed read-only and is neither imported nor mutated.

#### `RECOVERY-REVIEW`

1. With a Shell producing bounded output, close both desktop and TUI. Confirm the process remains alive; reopen each client and recover attachment through the daemon.
2. Load a bounded read-only diff and force the size limit; record the truncation notice.
3. In a disposable host snapshot, terminate the live daemon process without using `sylvops daemon stop`, restart it, and confirm the old active-looking record becomes `disconnected`, retains outcome metadata, and explicitly says terminal output is unavailable. Confirm repeated restart is idempotent and SylvOps did not adopt the persisted PID. Clean up any OS process left by the deliberately abrupt test before reusing the host.
4. A failed authentication, protocol-version, stale-state removal, or descendant-cleanup observation is an immediate failure, not a limitation.

#### `DOCTOR-PROVIDER`

1. Run `sylvops doctor` against the active daemon. It must pass state-directory, Git, authenticated daemon/protocol, provider-health, and real PTY spawn/output/reap checks with bounded redacted output.
2. On a clean profile where Codex is absent or genuinely logged out, confirm desktop and TUI show actionable install/login guidance, daemon-backed **Check again**, and Shell fallback. Re-probe after the external condition is repaired.
3. Confirm no credential, raw environment value, or provider secret appears in the transcript, diagnostics, state directory, or UI.

#### `REAL-CODEX`

This is opt-in, never part of the default automated suite, and runs only with pre-existing authentication. Record Codex version/path health without recording credentials.

1. `provider probe codex` must report available and authenticated using a canonical native binary.
2. Start Codex through SylvOps in the disposable repository with a harmless prompt. Confirm structured launch in the selected checkout and interactive input/output.
3. Observe an authenticated hook-driven attention transition and a verified external session ID in the SylvOps snapshot. Do not copy raw provider configuration or environment values.
4. Put the session into a resume-eligible inactive state, resume it, and confirm a new SylvOps session record refers to the verified external ID while the prior record remains intact.
5. Stop the resumed session and confirm complete process-tree cleanup.

### Required native coverage rows

Use these exact IDs in the evidence index:

| Target | Required rows |
|---|---|
| Windows x86_64 | `WIN-HOST-INSTALL`, `WIN-DESKTOP-ENTRY`, `WIN-CLI-TUI`, `WIN-SHELL-LIFECYCLE`, `WIN-WORKTREE-SAFETY`, `WIN-RECOVERY-REVIEW`, `WIN-DOCTOR-PROVIDER`, `WIN-REAL-CODEX` |
| Linux x86_64 | `LINUX-HOST-INSTALL`, `LINUX-DESKTOP-ENTRY`, `LINUX-CLI-TUI`, `LINUX-SHELL-LIFECYCLE`, `LINUX-WORKTREE-SAFETY`, `LINUX-RECOVERY-REVIEW`, `LINUX-DOCTOR-PROVIDER`, `LINUX-REAL-CODEX` |
| macOS x86_64 | `MAC-X64-HOST-INSTALL`, `MAC-X64-DESKTOP-ENTRY`, `MAC-X64-SHELL-LIFECYCLE`, `MAC-X64-DOCTOR` |
| macOS arm64 | `MAC-ARM64-HOST-INSTALL`, `MAC-ARM64-DESKTOP-ENTRY`, `MAC-ARM64-SHELL-LIFECYCLE`, `MAC-ARM64-DOCTOR` |
| One native macOS architecture, identified in every row | `MAC-CLI-TUI`, `MAC-WORKTREE-SAFETY`, `MAC-RECOVERY-REVIEW`, `MAC-DOCTOR-PROVIDER`, `MAC-REAL-CODEX` |

The macOS extended rows must all use the same candidate but may use separate clean snapshots of one architecture for provider-unavailable and real-Codex checks. Results from Rosetta do not count as arm64 native proof, and an arm64 result does not count as x86_64 proof.

### Contract traceability

| Contract | Evidence |
|---|---|
| B1 | `REL-*`, `ART-*`, all `HOST-INSTALL` rows |
| B2 | all `DESKTOP-ENTRY` rows and Shell-default observations |
| B3 | Windows/Linux and extended macOS `CLI-TUI` rows |
| B4 | native CI plus every `RECOVERY-REVIEW` row |
| B5 | Windows/Linux and extended macOS `WORKTREE-SAFETY` rows |
| B6 | every `SHELL-LIFECYCLE` row |
| B7 | every `DOCTOR-PROVIDER` and `REAL-CODEX` row |
| B8 | every `RECOVERY-REVIEW` row |
| B9 | every `DOCTOR-PROVIDER`, cleanup, and refusal observation |
| B10 | `CI-WIN-X64`, `CI-LINUX-X64`, and `CI-MACOS-NATIVE` |

### Evidence status at resolution

No candidate release evidence was collected or claimed in this ticket. The repository was inspected and the process/validation was implemented, but the following remain release-blocking:

- `REL-GATE`, all native CI rows, `REL-STAGE`, and all four `ART-*` rows are `NOT_RUN` for a staged candidate.
- Every Windows, Linux, macOS x86_64, and macOS arm64 clean-host row is `NOT_RUN`.
- Every opt-in real-Codex row is `NOT_RUN`.
- No release was published and no clean-host result is inferred from source, local compilation, or earlier hosted runs.

The Windows packaging contradiction is repaired: `QUICKSTART.txt` now describes the native desktop opened by `Start-SylvOps.ps1` and retains an accurate `sylvops tui` fallback. `scripts/validate-release.ps1` and CI/package workflow calls make the version, target set, archive manifests, checksum set, bundled instructions, provenance step, and publication gate resistant to accidental omission.
