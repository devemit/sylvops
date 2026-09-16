# SylvOps

SylvOps is a local-first terminal mission control for supervising interactive coding-agent sessions in Git worktrees.

## Build and run

SylvOps is currently an MVP implementation candidate, not a stable release. Install stable Rust 1.85 or newer and Git, then build it from the repository:

```text
git clone https://github.com/devemit/sylvops.git
cd sylvops
cargo build --release -p sylvops-cli
```

The executable is written to `target/release/sylvops` (`sylvops.exe` on Windows). Start the daemon and open the TUI:

```text
./target/release/sylvops daemon start
./target/release/sylvops tui
```

On Windows, use `target\release\sylvops.exe` from PowerShell. Maintainers can build the unsigned portable Windows ZIP with `powershell -File scripts/package-windows.ps1`; generated packages are intentionally excluded from Git.

The repository now contains an **MVP implementation candidate**. It includes:

- versioned, length-prefixed MessagePack framing;
- authenticated local Unix-socket and Windows named-pipe control transport;
- an authoritative daemon with concurrent client handling;
- an embedded, checksum-validated SQLite migration and dedicated database actor;
- versioned TOML configuration with machine-local overrides and atomic writes;
- startup reconciliation that marks unverifiable active sessions disconnected;
- read-only registration of Git repositories and their root checkouts;
- daemon-owned plain-shell PTYs with bounded sequenced scrollback;
- controller/observer attachment, detach, resize, input, reconnect, and parser snapshots;
- explicit process-tree termination and persisted exit state;
- guarded managed-worktree creation, status, and removal;
- startup reconciliation for missing and externally discovered worktrees;
- per-project Git mutation serialization and per-worktree session/removal coordination;
- a provider adapter registry for plain shells and Codex;
- bounded Codex discovery, version, and authentication probes;
- application-owned Codex hook profiles plus an authenticated loopback hook relay;
- deterministic attention states, external Codex session-ID capture, and guarded resume;
- a four-panel Ratatui client with hierarchical navigation, attention selection, attachment handoff, and bounded read-only diff preview;
- CLI workspace, project, worktree, provider, session, and TUI commands.

Claude, GitHub integration, commit/push/PR actions, remote execution, notifications, file finding, and Git grep remain post-MVP. SylvOps never copies or stores provider credentials.

The Linux Docker validation gate passes formatting, warnings-as-errors Clippy, and the full workspace test suite, including the fake-Codex hook/resume end-to-end scenario. Windows-target strict Clippy also passes with the GNU toolchain. This candidate is not release-verified until native Windows tests and hosted CI pass: the current host is missing the MSVC linker and Windows SDK, and the post-spawn Windows Job Object assignment race still blocks a race-free Windows MVP claim.

## Current commands

```text
sylvops daemon start
sylvops daemon status
sylvops workspace add my-workspace
sylvops project add --workspace <workspace-id> <repository-path>
sylvops worktree create --project <project-id> --branch feature/example --base HEAD
sylvops worktree status <worktree-id>
sylvops provider list
sylvops provider probe codex
sylvops session create --worktree <worktree-id> --provider shell
sylvops session create --worktree <worktree-id> --provider codex --model <model> --effort high --prompt "..."
sylvops session attach <session-id>
sylvops session resume <session-id>
sylvops session stop <session-id>
sylvops worktree remove <worktree-id> --confirm
sylvops tui
sylvops snapshot
sylvops daemon stop
```

## Developer commands

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets
```

See [the MVP definition](docs/product/mvp.md) and [process architecture](docs/architecture/process-model.md).

The standard Windows MSVC target requires the Visual C++ toolchain and Windows SDK; bundled SQLite also requires a supported C compiler. The current Windows Job Object assignment has a documented post-spawn race and is not claimed as release-hardened. See [ADR-0010](docs/decisions/0010-codex-hooks-and-mvp-tui.md).
