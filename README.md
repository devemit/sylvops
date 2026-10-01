# SylvOps

SylvOps is a local-first mission control for supervising interactive coding-agent sessions in Git worktrees. It ships a native desktop client and retains a keyboard-first terminal client.

## Prerequisites

Install Git yourself and make sure `git --version` works before starting SylvOps. A Rust toolchain is not required for a packaged release.

Codex is optional and separately installed and authenticated. Install the Codex CLI and complete its login using OpenAI's instructions before choosing the Codex provider. SylvOps does not install Git or Codex, run their login commands, copy credentials, or store provider secrets. After SylvOps is running, use `sylvops doctor` and `sylvops provider probe codex` to distinguish a missing executable from a login problem.

Download release files only from the [SylvOps GitHub Releases](https://github.com/devemit/sylvops/releases) page. Keep the release's `SHA256SUMS` beside the downloaded package and verify the named file before opening it. Every asset also has GitHub build-provenance attestations.

## Free unsigned previews

Unsigned previews are testing builds, not official signed application releases. They contain a Windows x86_64 portable ZIP and Linux x86_64 portable archive, AppImage, and Debian package. They do not contain macOS packages or a native Windows installer, and they do not receive signed in-app updates.

To install `v0.1.0-preview.1` on Windows from PowerShell, download the matching installer script first so it can be inspected before execution:

```powershell
$tag = 'v0.1.0-preview.1'; $script = Join-Path $env:TEMP 'install-sylvops.ps1'; Invoke-WebRequest "https://raw.githubusercontent.com/devemit/sylvops/$tag/scripts/install.ps1" -OutFile $script; & $script -Version $tag.TrimStart('v')
```

Launch the portable installation explicitly:

```powershell
& "$env:LOCALAPPDATA\SylvOps\bin\sylvops.exe" up C:\path\to\repository
```

Windows may identify the executable as coming from an unknown publisher. Do not bypass operating-system security warnings. The preview ZIP includes `UNSIGNED-PREVIEW.txt`; verify `SHA256SUMS`, inspect the downloaded script, or build from source when trust cannot be established.

On Linux x86_64:

```sh
tag=v0.1.0-preview.1; curl --fail --location --proto '=https' --tlsv1.2 "https://raw.githubusercontent.com/devemit/sylvops/$tag/scripts/install.sh" --output /tmp/install-sylvops.sh; SYLVOPS_VERSION="${tag#v}" sh /tmp/install-sylvops.sh
```

The manual **Unsigned Preview** GitHub Actions workflow publishes these files as a non-latest GitHub testing release after the complete CI gate passes. The signed `v0.1.0` workflow remains separate and still fails closed unless its protected publisher credentials are configured.

## Install on Windows

Windows packages support x86_64 Windows 10 version 1809 or newer and Windows 11.

1. Download `sylvops-windows-x86_64-setup.exe` and `SHA256SUMS` from the same release.
2. In PowerShell, verify the checksum and Authenticode signature:

   ```powershell
   $expected = (Select-String -Path .\SHA256SUMS -Pattern 'sylvops-windows-x86_64-setup.exe$').Line.Split()[0]
   $actual = (Get-FileHash .\sylvops-windows-x86_64-setup.exe -Algorithm SHA256).Hash.ToLowerInvariant()
   if ($actual -ne $expected) { throw 'SylvOps installer checksum mismatch' }
   (Get-AuthenticodeSignature .\sylvops-windows-x86_64-setup.exe).Status
   ```

   The signature status must be `Valid`. The installer and application executable are signed and timestamped; do not run a missing, invalid, or mismatched signature.
3. Run the installer. It is per-user, needs no administrator privileges, installs under `%LOCALAPPDATA%\Programs\SylvOps`, and adds **SylvOps** to the Start Menu.
4. Launch **SylvOps** from the Start Menu. The first launch starts the local daemon and opens the native desktop client.

The installer registers `sylvops.exe` through the current user's Windows App Paths. PowerShell can invoke a CLI command without editing `PATH`:

```powershell
Start-Process sylvops -ArgumentList 'doctor'
```

If a shell does not resolve App Paths, use `%LOCALAPPDATA%\Programs\SylvOps\sylvops.exe` explicitly.
The installer also places this guide at `%LOCALAPPDATA%\Programs\SylvOps\README.md`.

## Install on macOS

1. Run `uname -m`. Download `sylvops-macos-x86_64.dmg` for `x86_64` (Intel) or `sylvops-macos-aarch64.dmg` for `arm64` (Apple silicon), plus `SHA256SUMS` from the same release.
2. Verify the selected file. This Apple-silicon example uses the same commands with the Intel filename when appropriate:

   ```sh
   grep 'sylvops-macos-aarch64.dmg$' SHA256SUMS | shasum -a 256 --check -
   codesign --verify --verbose=2 sylvops-macos-aarch64.dmg
   xcrun stapler validate sylvops-macos-aarch64.dmg
   spctl --assess --type open --context context:primary-signature --verbose=4 sylvops-macos-aarch64.dmg
   ```

   The DMG and application are Developer ID signed, the application uses the hardened runtime, and the notarization ticket is stapled. Stop if checksum, signature, notarization, or Gatekeeper assessment fails.
3. Open the DMG and drag **SylvOps** to `/Applications` (or another Applications location you control).
4. Launch **SylvOps** from Finder. Gatekeeper should accept the normal first launch without an unsigned-application workaround.

Advanced CLI users can run `/Applications/SylvOps.app/Contents/MacOS/sylvops` directly.
The application bundle carries this guide at `/Applications/SylvOps.app/Contents/Resources/README.md`.

## Install on Linux

Linux packages support x86_64 graphical Linux systems. Choose one format:

- Debian or Ubuntu: download `sylvops-linux-x86_64.deb` and `SHA256SUMS`, then run:

  ```sh
  grep 'sylvops-linux-x86_64.deb$' SHA256SUMS | sha256sum --check -
  sudo apt install ./sylvops-linux-x86_64.deb
  ```

  Launch **SylvOps** from the desktop application menu or run `sylvops`. The Debian package installs `/usr/bin/sylvops`, the icon, desktop entry, and AppStream metadata.
  It installs this guide at `/usr/lib/sylvops/README.md`.

- No-administrator AppImage: download `sylvops-linux-x86_64.AppImage` and `SHA256SUMS`, then run:

  ```sh
  grep 'sylvops-linux-x86_64.AppImage$' SHA256SUMS | sha256sum --check -
  chmod +x sylvops-linux-x86_64.AppImage
  ./sylvops-linux-x86_64.AppImage
  ```

  The AppImage is a portable executable: it does not install desktop integration or edit `PATH`. It carries this guide at `usr/lib/sylvops/README.md` inside its package filesystem.

Linux packages do not currently carry a platform package signature. The published checksum and GitHub provenance attestations verify the release bytes. Installed Debian files can later be checked with `sudo dpkg --verify sylvops`.

## Portable fallback

Portable archives are an advanced distribution and recovery path:

- Windows x86_64: `sylvops-windows-x86_64.zip`
- macOS Intel: `sylvops-macos-x86_64.tar.gz`
- macOS Apple silicon: `sylvops-macos-aarch64.tar.gz`
- Linux x86_64: `sylvops-linux-x86_64.tar.gz`

On Windows, verify, extract, and launch without relying on `PATH`:

```powershell
$archive = 'sylvops-windows-x86_64.zip'
$expected = (Select-String -Path .\SHA256SUMS -Pattern "$archive$").Line.Split()[0]
$actual = (Get-FileHash ".\$archive" -Algorithm SHA256).Hash.ToLowerInvariant()
if ($actual -ne $expected) { throw 'SylvOps archive checksum mismatch' }
Expand-Archive -LiteralPath ".\$archive" -DestinationPath .\sylvops-portable
.\sylvops-portable\sylvops.exe up C:\path\to\repository
```

The Windows ZIP also includes `Start-SylvOps.ps1`, `Start SylvOps.cmd`, `QUICKSTART.txt`, and this `README.md`.

On macOS Apple silicon, use the following commands; substitute `sylvops-macos-x86_64.tar.gz` on an Intel Mac:

```sh
grep 'sylvops-macos-aarch64.tar.gz$' SHA256SUMS | shasum -a 256 --check -
mkdir -p sylvops-portable
tar -xzf sylvops-macos-aarch64.tar.gz -C sylvops-portable
./sylvops-portable/sylvops up /path/to/repository
```

On Linux x86_64:

```sh
grep 'sylvops-linux-x86_64.tar.gz$' SHA256SUMS | sha256sum --check -
mkdir -p sylvops-portable
tar -xzf sylvops-linux-x86_64.tar.gz -C sylvops-portable
./sylvops-portable/sylvops up /path/to/repository
```

The macOS portable archives are unsigned. Portable archives do not register an application, add desktop integration, or edit `PATH`; invoke the extracted executable as shown.

This starts the daemon, creates or reuses the `Local` workspace, registers the repository idempotently, and opens the native desktop app. It does not start a shell or agent. `sylvops open .` is a compatibility alias; `sylvops tui` opens the keyboard-first terminal client.

## First run

The desktop supports first launch without a repository and guides the complete `Workspace -> Project/root Worktree -> Session` hierarchy:

1. Create a **Workspace**, a local group of repositories to supervise together.
2. Choose an existing Git repository. SylvOps registers it as a **Project** and records its current checkout as the root **Worktree**; it does not copy or move the repository.
3. Choose the root Worktree (or later create a managed Worktree), select **Codex**, and choose **Start Codex**. If discovery reports that Codex is missing or logged out, install or authenticate Codex outside SylvOps, choose **Retry discovery**, and retry. **Shell** remains available without Codex.
4. Choose **Open terminal** to attach. Closing or detaching the desktop does not stop the daemon-owned session.

The first Codex session is created only after the visible action in step 3; SylvOps never launches an agent just because the application opened.

## Updates and rollback

In **Settings -> Application updates**, choose **Check now**, then **Download verified upgrade**, then **Install update**. The desktop shows the target version, byte length, and bounded release notes. Periodic checks are on by default at a bounded 24-hour interval and can be disabled with **Periodic checks: Off**. Checks read signed metadata; downloads and installation always require visible user actions.

The equivalent CLI flow is:

```text
sylvops update check
sylvops update download
sylvops update status
sylvops update install
```

Installation defers while sessions are active. To override that protection, confirm every session named by the blocked response by repeating `--confirm-active-session <SESSION_ID>` on `sylvops update install`. The named sessions and their complete process trees will stop.

Before replacement, SylvOps retains the previous package and a compatible database snapshot. It then checks the installed version, IPC protocol, database startup, executable/package identity, and desktop relaunch. Windows also checks Authenticode identity, macOS checks the bundle and Developer ID team, and Linux checks the active AppImage or Debian-owned files and desktop integration. If health fails or the helper is interrupted, SylvOps performs one rollback and relaunches the previous version. Run `sylvops update status` to see whether the current state is installed, rolled back, or failed; there is no separate manual rollback command.

## Uninstall and remove user data

Normal uninstall removes application files and OS integration but preserves configuration, session data, repositories, worktrees, and branches:

- Windows: open **Installed apps**, uninstall **SylvOps**, or run `%LOCALAPPDATA%\Programs\SylvOps\uninstall.exe`.
- macOS: remove `/Applications/SylvOps.app` (or the Applications location chosen during installation).
- Debian or Ubuntu: run `sudo apt remove sylvops`.
- AppImage or portable archive: remove the downloaded file or extracted application directory.

SylvOps-owned data uses these paths:

- Windows data and runtime: `%LOCALAPPDATA%\SylvOps` and `%LOCALAPPDATA%\SylvOps\run`; configuration: `%APPDATA%\SylvOps`.
- macOS and Linux data: `$XDG_DATA_HOME/sylvops`, or `~/.local/share/sylvops` when `XDG_DATA_HOME` is unset.
- macOS and Linux configuration: `$XDG_CONFIG_HOME/sylvops`, or `~/.config/sylvops` when `XDG_CONFIG_HOME` is unset.
- macOS and Linux runtime: `$XDG_RUNTIME_DIR/sylvops` when set, otherwise the data directory's `run` subdirectory.

To remove that data deliberately, first stop every session, then use **Settings -> Safety -> Remove SylvOps user data** or:

```text
sylvops data remove --confirm "DELETE SYLVOPS USER DATA"
```

The exact phrase is required. The daemon refuses removal while sessions are active, shuts down before deletion, revalidates only SylvOps-owned paths, and preserves registered or structurally discovered repositories, worktrees, and Git branches. Partial removal can be retried with the same action.

## Troubleshooting

- **Provider discovery:** run `sylvops provider probe codex`. A missing executable and a login-required result are distinct. Install or sign in to Codex outside SylvOps; SylvOps never performs either action.
- **Daemon startup:** launch the installed application or run `sylvops daemon start`, then `sylvops daemon status`. If startup still fails, run `sylvops doctor` from the same installation.
- **Mixed versions:** compare `sylvops --version` with the daemon version printed by `sylvops daemon status` or `sylvops doctor`. If they differ, run `sylvops daemon stop`, then reopen the intended installed application. For a portable copy, invoke that copy explicitly so an older executable on `PATH` is not selected.
- **Package verification:** never open a file that fails `SHA256SUMS`. Windows must also report a valid Authenticode signature, macOS must pass `codesign`, `stapler`, and Gatekeeper checks, and an installed Debian package can be checked with `sudo dpkg --verify sylvops`.
- **Upgrade failure:** run `sylvops update status`. A failed health check should report that the previous version was restored; the same failed target is not retried in a loop.
- **Diagnostics:** `sylvops doctor` checks local state, Git, daemon version/protocol/database health, provider availability, and PTY lifecycle. Its process output is bounded and redacted: it does not print credentials or environment values. Share only the version, platform, categorical status, and redacted logs.

## Build from source

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
sylvops update install --confirm-active-session <SESSION_ID>
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
