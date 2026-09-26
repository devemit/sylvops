# SylvOps

SylvOps is a local-first mission control for supervising interactive coding-agent sessions in Git worktrees. It ships a native desktop client and retains a keyboard-first terminal client.

## Quick start

SylvOps is distributed through explicitly promoted semantic `0.x` application releases on GitHub. On Windows x86_64, download `sylvops-windows-x86_64-setup.exe`, verify it against `SHA256SUMS`, and run the signed per-user installer. It installs under `%LOCALAPPDATA%\Programs\SylvOps` without administrator privileges and adds **SylvOps** to the Start Menu. Launching that entry starts the local daemon and native desktop client.

The installer also registers `sylvops.exe` through the current user's Windows App Paths. PowerShell users can invoke an installed CLI command without editing `PATH`, for example:

```powershell
Start-Process sylvops -ArgumentList 'doctor'
```

The explicit path `%LOCALAPPDATA%\Programs\SylvOps\sylvops.exe` remains available for shells that do not resolve App Paths. Uninstalling SylvOps removes the application and its Windows integration but preserves configuration, session data, repositories, worktrees, and branches.

On macOS, download `sylvops-macos-x86_64.dmg` for an Intel Mac or `sylvops-macos-aarch64.dmg` for Apple silicon, verify it against `SHA256SUMS`, open the DMG, and drag **SylvOps** to `/Applications`. The application and disk image are Developer ID signed, the application uses the hardened runtime, and the notarized DMG carries a stapled ticket, so Finder launch is accepted by Gatekeeper without a security workaround. Removing `/Applications/SylvOps.app` removes only the application; configuration, session data, repositories, worktrees, and branches remain in place. Advanced CLI users can invoke `/Applications/SylvOps.app/Contents/MacOS/sylvops` directly.

On Linux x86_64, download either `sylvops-linux-x86_64.deb` for normal Debian-family installation or `sylvops-linux-x86_64.AppImage` as a portable fallback, then verify it against `SHA256SUMS`. Install the Debian package with `sudo apt install ./sylvops-linux-x86_64.deb`; it provides the shared SylvOps name, icon, desktop entry, AppStream metadata, and `sylvops` CLI. Remove it with `sudo apt remove sylvops`. Package removal deletes application integration while preserving SylvOps state, repositories, worktrees, and branches. The AppImage requires `chmod +x sylvops-linux-x86_64.AppImage` before launch and does not install files or desktop integration.

The desktop Settings panel can check signed release metadata, show the target version, size, and notes, download a verified package, and install only after a visible confirmation. Periodic checks default to a bounded 24-hour interval and can be disabled. Installation defers while agent sessions are active; the override confirmation names every session whose process tree will stop. The same controls are available as `sylvops update check`, `download`, `status`, `cancel`, and `install --override-active-sessions`.

Git is a separate prerequisite and must already be installed and discoverable by the operating system. Codex is optional and separately discovered: install the Codex CLI and complete its login using OpenAI's own instructions before selecting the Codex provider. SylvOps does not install either tool, run login commands, copy credentials, or store provider secrets. Use `sylvops doctor` and `sylvops provider probe codex` to distinguish a missing executable from an authentication problem.

If an installed upgrade fails its bounded version, protocol, database, executable, or platform-signature health check, SylvOps restores the previous package once and relaunches that version. Run `sylvops update status` to confirm the rollback, then `sylvops doctor` for redacted diagnostics before retrying. Persistent failures should be reported with the version, platform, status, and redacted daemon log; never attach credentials or unredacted environment output. A portable archive remains available as a recovery path if normal package repair is unavailable.

Normal uninstall never removes user data. To deliberately remove configuration, preferences, logs, SQLite session metadata, and staged upgrades, first uninstall or stop SylvOps, then run `sylvops data remove --confirm "DELETE SYLVOPS USER DATA"`. The command revalidates application-owned paths immediately before deletion and refuses to remove registered repositories or worktrees. On Windows data lives below `%LOCALAPPDATA%\SylvOps` and `%APPDATA%\SylvOps`; on macOS and Linux it follows the platform data/config directories (`~/Library` or XDG defaults). Run `sylvops doctor` for redacted installation, daemon, Git, provider, and PTY diagnostics.

Portable archives remain available on every platform as an advanced and recovery fallback. After extracting one and placing the binary on `PATH`, run this inside a Git repository:

```text
sylvops up .
```

This starts the local daemon, creates or reuses the `Local` workspace, registers the repository idempotently, and opens the native desktop app. It does not launch an agent automatically. `sylvops open .` is a compatibility alias, while `sylvops tui` opens the terminal interface.

Every release asset is covered by the release's `SHA256SUMS` and build-provenance attestations. The Windows installer and executable are Authenticode signed and timestamped, and the macOS DMGs provide the signed and notarized normal installation path. Portable archives and the Linux AppImage do not edit `PATH`; macOS portable archives remain an unsigned advanced fallback. Linux desktop launch requires a graphical session and the system libraries normally provided by a supported desktop installation.

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

On Windows, use `target\release\sylvops.exe` from PowerShell. Maintainers can build the unsigned portable Windows ZIP with `powershell -File scripts/package-windows.ps1`; native Windows, macOS, and Linux package workflows are documented in [docs/development/releasing.md](docs/development/releasing.md). Generated packages are intentionally excluded from Git.

The repository now contains a **cross-platform application-release baseline**. It includes:

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

The Windows implementation contains a Win32 ConPTY launcher that supplies both the pseudoconsole and kill-on-close Job Object through `STARTUPINFOEX` at process creation. The prior cross-platform baseline passed the complete hosted Windows, Linux, and macOS quality gates; the current tree must pass those gates before `v0.1.0` is promoted.

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
- Select a session, click **Open terminal**, and interact normally. The wheel, history scrollbar, or Shift+PageUp/Shift+PageDown reads terminal history; **Latest**, Shift+End, clicking to type, or typing returns to the live input row. **Leave terminal** or `Ctrl+]` disconnects the desktop without stopping the session.
- The footer always shows the version, workspace, branch, current view, daemon connectivity, and key hints.
- Settings use compact dropdowns for ten built-in themes, comfortable/compact density, four terminal typefaces, 10–22 px text sizing, and Block/Line terminal cursors. New interactive sessions ask only for a provider and optional display name; advanced Codex model, effort, and initial-prompt options remain available from the CLI. The terminal TUI remains available as a keyboard-first alternative.

## Current commands

```text
sylvops
sylvops up .
sylvops open .
sylvops open . --provider codex --prompt "..."
sylvops doctor
sylvops update check
sylvops update download
sylvops update status
sylvops update cancel
sylvops update install
sylvops update install --override-active-sessions
sylvops data remove --confirm "DELETE SYLVOPS USER DATA"
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
