# SylvOps

SylvOps is a local-first mission control for supervising interactive coding-agent sessions in Git worktrees. It ships a native desktop client and retains a keyboard-first terminal client.

## Quick start

SylvOps is currently a public-beta candidate, not a stable release. Once a beta archive is published, extract it, put the binary on `PATH`, and run this inside a Git repository:

```text
sylvops up .
```

This starts the local daemon, creates or reuses the `Local` workspace, registers the repository idempotently, and opens the native desktop app. It does not launch an agent automatically. `sylvops open .` is a compatibility alias, while `sylvops tui` opens the terminal interface.

To build from source, install stable Rust 1.88 or newer, Git, and the platform C toolchain:

```text
git clone https://github.com/devemit/sylvops.git
cd sylvops
cargo build --release -p sylvops-cli
```

The executable is written to `target/release/sylvops` (`sylvops.exe` on Windows):

```text
./target/release/sylvops up .
```

On Windows, use `target\release\sylvops.exe` from PowerShell. Maintainers can build the unsigned portable Windows ZIP with `powershell -File scripts/package-windows.ps1`; generated packages are intentionally excluded from Git.

The repository now contains a **cross-platform beta candidate**. It includes:

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
- a native, mouse-first desktop client with workspace tabs, guided repository/worktree/session management, responsive resizable navigation, a dominant terminal workspace, session tabs, Changes/Details views, persisted appearance and layout, and daemon-backed terminal attachment;
- a hierarchical Ratatui mission-control client with an explorer, Terminal/Changes/Details tabs, guided forms, keyboard and mouse controls, attention selection, safe embedded terminal rendering, bounded read-only diffs, and restored navigation context;
- CLI workspace, project, worktree, provider, session, desktop, and TUI commands.

Claude, GitHub integration, commit/push/PR actions, remote execution, notifications, file finding, and Git grep remain post-MVP. SylvOps never copies or stores provider credentials.

The beta branch contains a Win32 ConPTY launcher that supplies both the pseudoconsole and kill-on-close Job Object through `STARTUPINFOEX` at process creation. The prior beta baseline passed the complete hosted Windows, Linux, and macOS quality gates; the redesigned UX must pass those gates before `v0.1.0-beta.1` is published.

### TUI essentials

- The explorer shows the active workspace hierarchy: project → worktree → session.
- `Tab` switches between Explorer and Main; arrows navigate and collapse/expand.
- `1`, `2`, and `3` open Terminal, Changes, and Details.
- `Enter` explicitly attaches a selected session; `Ctrl+]` detaches without stopping it.
- `n`, `r`, and `d` create, rename, and stop/remove in context. `/` searches commands and entities.
- Mouse selection, tabs, actions, forms, confirmations, and wheel navigation are supported. Terminal mouse-protocol forwarding is intentionally deferred.

### Desktop essentials

- `sylvops` or `sylvops up [PATH]` opens the native window; closing it leaves daemon-owned sessions running.
- The top bar switches workspaces. Projects, worktrees, and sessions stay visible beside the large Terminal/Changes/Details area.
- Select a session, click **Attach**, and interact normally. `Ctrl+]` detaches without stopping it.
- The footer always shows the version, workspace, branch, current view, daemon connectivity, and key hints.
- Settings provide System, Light, Dark, Nord, Tokyo Night, and Catppuccin themes plus comfortable/compact density, terminal font sizing, and layout reset. The terminal TUI remains available as a keyboard-first alternative.

## Current commands

```text
sylvops
sylvops up .
sylvops open .
sylvops open . --provider codex --prompt "..."
sylvops doctor
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

The standard Windows target requires Visual C++ Build Tools and the Windows SDK. See [the PTY architecture](docs/architecture/pty.md) for the atomic ConPTY/Job Object boundary.
