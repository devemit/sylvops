# SylvOps

SylvOps is a local-first mission control for supervising interactive coding-agent sessions in Git worktrees. It combines a native desktop app, a keyboard-first terminal client, and a daemon that keeps sessions running when a client disconnects.

[Documentation](https://devemit.github.io/sylvops-docs/) · [Getting started](https://devemit.github.io/sylvops-docs/getting-started) · [Installation](https://devemit.github.io/sylvops-docs/installation) · [CLI reference](https://devemit.github.io/sylvops-docs/cli-reference) · [Releases](https://github.com/devemit/sylvops/releases)

> [!WARNING]
> The current public build is an unsigned testing preview. Windows and Linux x86_64 packages are available; macOS packages are not included. Read the [installation guide](https://devemit.github.io/sylvops-docs/installation) before downloading.

## What SylvOps does

- Runs Shell, Codex, and Claude Code sessions in existing repositories or isolated Git worktrees.
- Keeps the daemon authoritative for session processes, terminal attachment, state, and recovery.
- Lets you detach from a terminal or close the desktop without stopping the session.
- Provides a native desktop client and a keyboard-first TUI over the same authenticated local IPC boundary.
- Can associate a project with a validated GitHub remote to browse Issues and Pull Requests and start isolated work from an Issue.
- Keeps credentials outside SylvOps. Provider CLIs and GitHub CLI are installed and authenticated separately.

## Getting started

### 1. Install Git

Install Git using your operating system's normal method, then confirm it is available:

```sh
git --version
```

SylvOps uses your installed `git` executable. Registering a repository does not copy or move it, and does not perform a network operation.

### 2. Install SylvOps

Choose a published unsigned testing preview from [GitHub Releases](https://github.com/devemit/sylvops/releases). The public installation guide identifies the current preview and its exact assets:

- [Windows x86_64 preview](https://devemit.github.io/sylvops-docs/installation#windows-preview)
- [Linux Debian or Ubuntu](https://devemit.github.io/sylvops-docs/installation#debian-or-ubuntu)
- [Linux AppImage](https://devemit.github.io/sylvops-docs/installation#appimage)
- [Portable Linux archive](https://devemit.github.io/sylvops-docs/installation#portable-linux-archive)

Release assets, checksums, and provenance attestations are published on [GitHub Releases](https://github.com/devemit/sylvops/releases).

### 3. Optionally install an agent CLI

The built-in Shell provider works without another agent. To use Codex or Claude Code, install and authenticate the CLI outside SylvOps.

For Codex, follow the [official Codex CLI guide](https://learn.chatgpt.com/docs/codex/cli):

```sh
codex login
codex login status
```

For Claude Code, install an official native CLI version 2.1.145 or newer using the [official Claude Code setup guide](https://docs.anthropic.com/en/docs/claude-code/getting-started):

```sh
claude --version
claude auth login
claude auth status
```

SylvOps does not install or update provider CLIs, perform login, copy credentials, store provider secrets, or choose a provider's permission mode.

### 4. Launch SylvOps with a repository

Give SylvOps the path to an existing Git repository:

```text
sylvops up /path/to/repository
```

On Windows preview installations, invoke the executable explicitly if its directory is not on `PATH`:

```powershell
& "$env:LOCALAPPDATA\SylvOps\bin\sylvops.exe" up C:\path\to\repository
```

`up` starts or reconnects to the local daemon, creates or reuses the **Local** workspace, registers the repository, and opens the native desktop client. It does not automatically start an agent.

### 5. Create or choose a workspace

A **Workspace** groups repositories you want to supervise together. Use **New workspace** in the desktop switcher, or keep **Local** if you only need one group.

### 6. Register a repository

Choose **Add repository** and select an existing Git repository. SylvOps records the repository and its current checkout without copying or moving it. The CLI and internal APIs call a registered repository a **Project**.

### 7. Select or create a checkout

Every registered repository has a root checkout. Work there directly or choose **New checkout** to create an isolated branch and worktree.

When creating a checkout, you can base it on the latest default branch, the current checkout, or a custom local ref. Fetching the latest default branch does not switch or rewrite the root checkout. The application calls this a **Checkout**; Git calls it a **worktree**.

### 8. Start a session

Select a checkout, choose **New session**, and select a provider:

- **Shell** starts a normal interactive terminal.
- **Codex** starts an installed and authenticated Codex CLI.
- **Claude Code** starts a supported, installed, and authenticated Claude Code CLI.

You can also assign a display name. Advanced provider model, effort, and initial-prompt options are available through the CLI.

### 9. Open the embedded terminal

Select the session and choose **Open terminal**. The daemon owns the process, so **Leave terminal** and closing the desktop only detach the client. Reopen SylvOps later to attach again. Choose **Stop** only when you want to terminate the session and its complete process tree.

Inactive Codex and Claude Code conversations can resume through a guarded successor session when the provider conversation is verified as resumable.

### 10. Optionally connect GitHub

Install and authenticate GitHub CLI outside SylvOps, select a project, and associate it with one of the repository's validated GitHub remotes. The desktop and TUI can then browse Issues and Pull Requests. **Start Work** on an Issue can create an isolated worktree and Codex session while retaining the Issue as its work source.

See [GitHub Issues and Pull Requests](https://devemit.github.io/sylvops-docs/github) for setup and security boundaries.

## Clients and session lifecycle

Run `sylvops` or `sylvops up [PATH]` for the native desktop client. Run `sylvops tui` for the keyboard-first terminal client. Both connect to the same local daemon and show the same workspace, project, checkout, provider, and session state.

The daemon owns every mutable PTY session. Clients attach and detach; they do not adopt agent processes or mutate Git directly. Terminal scrollback and IPC messages are bounded, and unknown protocol, hook, and approval types fail closed.

### Attention, notifications, and Codex hook review

Pending attention uses amber for input requests and recoverable failures, green for successful completion awaiting review, and red for terminal failure. The TUI Cards and List views and desktop Session rows retain this signal while selected. A visibly open foreground Session shows attention without an external notification or sound; visibility alone does not acknowledge it. Explicit **Stop session** clears prior attention and records a stopped process, rather than successful completion or a new failure alert.

After replacing a preview, restart the daemon and create a new Codex Session. In that Session, open `/hooks`, review the SylvOps handlers, and press `t` on each handler until all eight show **Active**. A changed hook command has a new source hash and needs fresh review; SylvOps never copies approvals to a changed command. Preview.13 corrects Windows PowerShell hook invocation and uses a private Codex runtime when the installed CLI supports it. Existing sessions do not acquire the new launch configuration.

The Settings notification test checks presentation only. Session alerts and Conversation identity capture require working Provider callbacks. If the missing-callback warning remains, inspect hook execution errors and refresh Provider health before starting another Session. Resume requires a verified Conversation identity; the preview does not repair old Session records that never captured one.

## Install on Windows

For this unsigned preview, download `sylvops-windows-x86_64.zip` and `SHA256SUMS` from the same preview release. Verify the ZIP before extracting it; the portable instructions below include the commands. The preview includes `UNSIGNED-PREVIEW.txt`, does not create a Start Menu entry or uninstall registration, and does not receive signed in-app updates. Do not bypass operating-system security warnings.

The source also supports a future official signed `sylvops-windows-x86_64-setup.exe`, but that installer is not part of this preview. For a published signed installer, require both the matching checksum and a `Valid` Authenticode signature before opening it. That per-user installer uses `%LOCALAPPDATA%\Programs\SylvOps`, registers uninstall and App Paths, and adds a Start Menu entry. These signed-installation instructions do not apply to an unsigned ZIP.

With that signed App Paths registration, PowerShell can run `Start-Process sylvops -ArgumentList 'doctor'`. For the current unsigned preview, invoke the installed or extracted executable by its explicit path instead.

## Install on macOS

This unsigned preview has no macOS downloads. Build from source on macOS or wait for a published signed application release.

The official packaging path produces `sylvops-macos-x86_64.dmg` for Intel and `sylvops-macos-aarch64.dmg` for Apple silicon. Only use those files when actually published with matching `SHA256SUMS`, Developer ID signatures, and stapled notarization tickets. Verify with `codesign`, `xcrun stapler validate`, and Gatekeeper before copying SylvOps to `/Applications` or another Applications location you control. The bundled guide is at `SylvOps.app/Contents/Resources/README.md`.

## Install on Linux

For Debian or Ubuntu, download `sylvops-linux-x86_64.deb` and its `SHA256SUMS`, then run:

```sh
grep 'sylvops-linux-x86_64.deb$' SHA256SUMS | sha256sum --check -
sudo apt install ./sylvops-linux-x86_64.deb
```

Launch SylvOps from the application menu or run `sylvops`. The package installs the CLI, desktop integration, and this guide at `/usr/lib/sylvops/README.md`.

For a no-installation alternative, download `sylvops-linux-x86_64.AppImage` and its checksum file:

```sh
grep 'sylvops-linux-x86_64.AppImage$' SHA256SUMS | sha256sum --check -
chmod +x sylvops-linux-x86_64.AppImage
./sylvops-linux-x86_64.AppImage
```

The AppImage does not add desktop integration or edit `PATH`. Linux packages have no platform package signature; verify the exact checksum and GitHub build-provenance attestation.

## Portable fallback

Portable archives do not register an application or edit `PATH`. Download the archive and `SHA256SUMS` from the same release, verify, then extract to a directory you control.

On Windows:

```powershell
$archive = 'sylvops-windows-x86_64.zip'
$expected = (Select-String -Path .\SHA256SUMS -Pattern "$archive$").Line.Split()[0]
$actual = (Get-FileHash ".\$archive" -Algorithm SHA256).Hash.ToLowerInvariant()
if ($actual -ne $expected) { throw 'SylvOps archive checksum mismatch' }
Expand-Archive -LiteralPath ".\$archive" -DestinationPath .\sylvops-portable
.\sylvops-portable\sylvops.exe up C:\path\to\repository
```

On Linux:

```sh
grep 'sylvops-linux-x86_64.tar.gz$' SHA256SUMS | sha256sum --check -
mkdir -p sylvops-portable
tar -xzf sylvops-linux-x86_64.tar.gz -C sylvops-portable
./sylvops-portable/sylvops up /path/to/repository
```

macOS archives are not included in this preview. If a later release publishes `sylvops-macos-aarch64.tar.gz` or `sylvops-macos-x86_64.tar.gz`, verify the matching checksum before extracting. The Apple-silicon extraction command is `tar -xzf sylvops-macos-aarch64.tar.gz -C sylvops-portable`; then invoke `./sylvops-portable/sylvops up /path/to/repository`.

## First run

The setup order is `Workspace -> Project/root Worktree -> Session`. Follow the getting-started steps above, or run `sylvops up .` in an existing repository. This opens the native desktop and does not start a provider automatically. Choose **Start shell** or **Start Codex** explicitly, then **Open terminal**. **Leave terminal** only detaches; **Stop session** terminates its process tree. **Delete checkout** is a separate, guarded action that requires a clean managed checkout.

Preview.15 adds local terminal text selection and copying: left-drag to select, then use Copy in the terminal border or Ctrl+Shift+C. Ctrl+C remains the provider interrupt. Selection stays still while output continues, and copying keeps Unicode, indentation, and line breaks.

It also retains preview.14’s actual terminal pane sizing, bounded normal-screen reflow, and snapshot recovery that preserves full-screen and mouse-input modes.

It includes PowerShell-compatible Codex hook commands, private-runtime launch and resume when supported, pending-attention colors in desktop Session rows, provider-specific callback guidance, and preservation of bounded Codex model preferences in managed hook profiles. It retains preview.12's startup, terminal theme, modified-key, multiline-draft, and Unicode-column fixes. Real-account completion and resume verification is opt-in and requires user review of the corrected hook source; it is not established by the Settings notification test.

While attached, Shift+Enter adds a draft newline when supported by the host terminal, and multiline paste respects the provider's requested paste mode. Explicit program colors remain intact. Provider shortcuts retain their native meaning: in Codex, Tab queues a follow-up for the next turn and Enter steers the current turn. Escape is not rebound to queue. `Ctrl+]` detaches the TUI without stopping the Session.

## Updates and rollback

Unsigned previews must be replaced manually with a verified newer preview. They do not receive signed in-app updates. Before replacing application files, finish or explicitly stop active Sessions, stop the daemon, and launch the new executable explicitly so an older copy on `PATH` is not selected.

The source supports signed-update controls for future official signed releases. In **Settings -> Updates**, those controls are **Check again**, **Download verified upgrade**, and **Install update**. **Periodic checks: Off** disables scheduled checks. The corresponding commands include `sylvops update check` and `sylvops update status`. These controls fail closed without signed official-release metadata and are not an update path for this preview.

For a signed installation, replacement defers while Sessions are active unless every named Session is explicitly confirmed. The detached helper retains the old package and a compatible database snapshot, checks the replacement's health and identity, and attempts one rollback if health fails or the helper is interrupted. There is no separate manual rollback command.

## Uninstall and remove user data

Normal uninstall preserves configuration, session data, repositories, worktrees, and branches:

- Windows preview: stop SylvOps, then remove the extracted application files or `%LOCALAPPDATA%\SylvOps\bin\sylvops.exe`. Do not delete the parent data directory as an uninstall shortcut.
- Signed Windows installation, when available: use **Installed apps** or `%LOCALAPPDATA%\Programs\SylvOps\uninstall.exe`.
- Debian or Ubuntu: run `sudo apt remove sylvops`.
- AppImage or portable archive: remove the downloaded file or extracted application directory.
- macOS application, when available: remove the application from its Applications location.

Windows data and runtime live under `%LOCALAPPDATA%\SylvOps`; configuration lives under `%APPDATA%\SylvOps`. On Unix, data uses `$XDG_DATA_HOME/sylvops` or `~/.local/share/sylvops`; configuration uses `$XDG_CONFIG_HOME/sylvops` or `~/.config/sylvops`.

To remove SylvOps-owned data deliberately, stop every Session, then choose **Settings -> Advanced -> Safety and data -> Remove SylvOps user data**, or run:

```text
sylvops data remove --confirm "DELETE SYLVOPS USER DATA"
```

The exact phrase is required. The daemon refuses removal while Sessions are active and preserves repositories, Worktrees, and Git branches. Partial removal can be retried.

## Troubleshooting

- Run `sylvops doctor` and `sylvops daemon status` from the intended installation to check daemon version, protocol, database, Git, and provider health.
- Use `sylvops provider probe codex` or `sylvops provider probe claude` to distinguish missing, outdated, or unauthenticated provider CLIs. Install and authenticate them outside SylvOps.
- If client and daemon versions differ, stop the daemon and reopen the intended executable. Closing a client alone leaves the daemon and Sessions running.
- Stop if a download fails `SHA256SUMS`. Do not override a platform security warning; build from source or wait for a signed package if trust cannot be established.
- Rebuild or install the new preview and restart the client to receive the terminal fixes. An existing client process does not change when source is pushed.
- Share only versions, platform details, categorical status, and redacted diagnostics. Never share credentials or environment values.

## Build from source

Install stable Rust 1.88 or newer, Git, and your platform's C toolchain:

```text
git clone https://github.com/devemit/sylvops.git
cd sylvops
cargo build --release -p sylvops-cli
```

The executable is written to `target/release/sylvops` (`target\release\sylvops.exe` on Windows). Launch it against the current repository with:

```sh
./target/release/sylvops up .
```

On Windows, run:

```powershell
.\target\release\sylvops.exe up .
```

Platform packaging and release procedures are documented in [docs/development/releasing.md](docs/development/releasing.md).

## Development

Before submitting a change, run the workspace quality gates:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets
```

Read the [contributing guide](https://devemit.github.io/sylvops-docs/contributing), [MVP definition](docs/product/mvp.md), and [process architecture](docs/architecture/process-model.md) for project expectations and design context.

## License

SylvOps is released under the [MIT License](LICENSE).
