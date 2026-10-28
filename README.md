# SylvOps

SylvOps is a local-first mission control for supervising interactive coding-agent sessions in Git worktrees. It combines a native desktop app, a keyboard-first terminal client, and a daemon that keeps sessions running when a client disconnects.

[Documentation](https://devemit.github.io/sylvops-docs/) · [Getting started](https://devemit.github.io/sylvops-docs/getting-started) · [Installation](https://devemit.github.io/sylvops-docs/installation) · [CLI reference](https://devemit.github.io/sylvops-docs/cli-reference) · [Releases](https://github.com/devemit/sylvops/releases)

> [!WARNING]
> Public builds are free unsigned testing previews. New previews provide Windows and Linux x86_64 packages plus macOS Intel and Apple silicon DMGs with ad-hoc signatures, without Apple notarization. Older previews omit macOS. Read the [installation guide](https://devemit.github.io/sylvops-docs/installation) and check the selected release's assets before downloading.

## What SylvOps does

- Runs Shell, Codex, and Claude Code sessions in existing repositories or isolated Git worktrees.
- Keeps the daemon authoritative for session processes, terminal attachment, state, and recovery.
- Lets you detach from a terminal or close the desktop without stopping the session.
- Provides a native desktop client and a keyboard-first TUI over the same authenticated local IPC boundary.
- Can associate a project with a validated GitHub remote to browse Issues and Pull Requests and start isolated work from an Issue.
- Keeps credentials outside SylvOps. Provider CLIs and GitHub CLI are installed and authenticated separately.
- Reviews local changes by file in Desktop and TUI, with separate staged, unstaged, and untracked groups, numbered diffs, search, syntax colors, and pausable refresh. Desktop also offers side-by-side diffs. See [reviewing changes](docs/product/changes.md).

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
- [macOS Intel or Apple silicon preview](https://devemit.github.io/sylvops-docs/installation#macos-preview)
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

Install and authenticate GitHub CLI outside SylvOps, then select a project to browse Issues and Pull Requests in the desktop or TUI. SylvOps uses your saved repository choice, otherwise a validated `origin` remote or the sole unambiguous GitHub repository among the remotes. Choose a remote in Settings when the target is ambiguous or you want to override that default. **Start Work** still requires an explicit saved repository association and can create an isolated worktree and Codex session while retaining the Issue as its work source.

See [GitHub Issues and Pull Requests](https://devemit.github.io/sylvops-docs/github) for setup and security boundaries.

## Clients and session lifecycle

Run `sylvops` or `sylvops up [PATH]` for the native desktop client. Run `sylvops tui` for the keyboard-first terminal client. Both connect to the same local daemon and show the same workspace, project, checkout, provider, and session state.

The daemon owns every mutable PTY session. Clients attach and detach; they do not adopt agent processes or mutate Git directly. Terminal scrollback and IPC messages are bounded, and unknown protocol, hook, and approval types fail closed.

### Attention, notifications, and Codex hook review

Pending attention uses amber for input requests and recoverable failures, green for successful completion awaiting review, and red for terminal failure in TUI Cards and List views. Desktop navigator rows and tabs use written indicators without attention background tints. Opening an input card successfully, or choosing **Mark reviewed** / Ctrl+Shift+R, clears its unread emphasis while retaining a neutral **Waiting for input** label until the Provider resolves it. Selection and foreground visibility alone do not acknowledge attention. Input popups and sound wait one second and are cancelled by review, resolution, replacement, or foreground display; other Sessions can still notify. Explicit **Stop session** clears prior attention and records a stopped process without a new completion/failure alert.

Managed Codex Sessions now observe actual questions and approval prompts through a private structured connection while retaining the native terminal. A prompt shows **Waiting for input** and stops the Working animation; answer or approve in the terminal to continue. Asynchronous question messages pause their turn until your reply. Automatic permission checks stay neutral. Opening or reviewing a card never answers it. This requires a native Codex CLI with remote-terminal authentication and stdio app-server support; Provider health explains when an update is needed.

After replacing a preview, restart daemon and clients together and create or Resume a Codex Session. Existing Sessions do not acquire a different runtime. If Codex asks to review existing SylvOps hooks, open `/hooks`, review the handlers, and press `t` to trust them normally. SylvOps never copies approvals to changed hook commands or alters the Provider's permission mode.

The Settings notification test checks presentation only. Session alerts and Conversation identity require accepted Provider events; managed Codex obtains these through its structured connection. Refresh Provider health when monitoring is unavailable. Resume requires a verified Conversation identity; old Session records without one are not repaired automatically.

## Install on Windows

For this unsigned preview, download `sylvops-windows-x86_64.zip` and `SHA256SUMS` from the same preview release. Verify the ZIP before extracting it; the portable instructions below include the commands. The preview includes `UNSIGNED-PREVIEW.txt`, does not create a Start Menu entry or uninstall registration, and does not receive signed in-app updates. Do not bypass operating-system security warnings.

The source also supports a future official signed `sylvops-windows-x86_64-setup.exe`, but that installer is not part of this preview. For a published signed installer, require both the matching checksum and a `Valid` Authenticode signature before opening it. That per-user installer uses `%LOCALAPPDATA%\Programs\SylvOps`, registers uninstall and App Paths, and adds a Start Menu entry. These signed-installation instructions do not apply to an unsigned ZIP.

With that signed App Paths registration, PowerShell can run `Start-Process sylvops -ArgumentList 'doctor'`. For the current unsigned preview, invoke the installed or extracted executable by its explicit path instead.

## Install on macOS

macOS previews support macOS 13 or newer. In **Apple menu -> About This Mac**, an Apple M-series chip means Apple silicon; an Intel processor means Intel. Download `sylvops-macos-aarch64.dmg` for Apple silicon or `sylvops-macos-x86_64.dmg` for Intel, plus `SHA256SUMS` from the same preview release. Older previews have no Mac assets.

Verify the exact downloaded DMG before opening it. For Apple silicon:

```sh
awk '$2 == "sylvops-macos-aarch64.dmg" { print }' SHA256SUMS > macos-checksum.txt
test -s macos-checksum.txt && shasum -a 256 --check macos-checksum.txt
```

For Intel, replace `sylvops-macos-aarch64.dmg` with `sylvops-macos-x86_64.dmg`. Stop if the checksum is missing or verification fails. Checksums and GitHub provenance identify release bytes; they do not establish Apple publisher trust.

Open the verified DMG, drag **SylvOps** into **Applications** (or an Applications folder you control), eject the disk image, then open the installed app. These free previews have ad-hoc signatures, not Developer ID signatures, and are not notarized by Apple. No paid Apple account is needed to run them.

If macOS blocks the trusted app because its developer cannot be verified or Apple cannot check it, first try opening the installed app, then choose **System Settings -> Privacy & Security -> Open Anyway**, authenticate if requested, and confirm **Open**. This creates an exception for this app. Follow [Apple's instructions](https://support.apple.com/en-us/102445). Managed Macs may prohibit this exception. Do not disable Gatekeeper, remove quarantine attributes, or override alerts that the app is damaged or contains malware. Build from source when the exception is unavailable or trust cannot be established.

The desktop starts from Applications without editing `PATH`. Optional CLI and TUI commands use the installed bundle's executable:

```sh
/Applications/SylvOps.app/Contents/MacOS/sylvops doctor
/Applications/SylvOps.app/Contents/MacOS/sylvops tui
```

Adjust the path if you chose another Applications folder. The bundled guide is at `SylvOps.app/Contents/Resources/README.md`. Future official signed releases require Developer ID signatures, stapled notarization tickets, and Gatekeeper verification; those requirements do not describe unsigned previews.

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

Protocol is 1.36: restart daemon and clients together after replacement, then start or Resume Sessions. Codex questions and real approval prompts establish Waiting for input; confirmed answers resolve it. Input notifications retain their one-second delay. Reviewing input removes unread emphasis while preserving the unresolved waiting condition.

Preview.15 adds local terminal text selection and copying: left-drag to select, then use Copy in the terminal border or Ctrl+Shift+C. Ctrl+C remains the provider interrupt. Selection stays still while output continues, and copying keeps Unicode, indentation, and line breaks.

It also retains preview.14’s actual terminal pane sizing, bounded normal-screen reflow, and snapshot recovery that preserves full-screen and mouse-input modes.

It includes PowerShell-compatible Codex hook commands, private-runtime launch and resume when supported, pending-attention indicators, provider-specific callback guidance, and preservation of bounded Codex model preferences in managed hook profiles. It retains preview.12's startup, terminal theme, modified-key, multiline-draft, and Unicode-column fixes. Real-account completion and resume verification is opt-in and requires user review of the corrected hook source; it is not established by the Settings notification test.

While attached, Shift+Enter adds a draft newline when supported by the host terminal, and multiline paste respects the provider's requested paste mode. Explicit program colors remain intact. Provider shortcuts retain their native meaning: in Codex, Tab queues a follow-up for the next turn and Enter steers the current turn. Escape is not rebound to queue. `Ctrl+]` detaches the TUI without stopping the Session.

## Updates and rollback

Unsigned previews must be replaced manually with a verified newer preview. They do not receive signed in-app updates. Before replacing application files, finish or explicitly stop active Sessions, stop the daemon, and launch the new executable explicitly so an older copy on `PATH` is not selected.

On macOS, stop the daemon using `/Applications/SylvOps.app/Contents/MacOS/sylvops daemon stop`, close desktop/TUI clients, replace the app from the verified newer DMG, and launch the installed app again. The new preview may need its own app-specific first-launch exception. User data remains separate from the app bundle.

The source supports signed-update controls for future official signed releases. In **Settings -> Updates**, those controls are **Check again**, **Download verified upgrade**, and **Install update**. **Periodic checks: Off** disables scheduled checks. The corresponding commands include `sylvops update check` and `sylvops update status`. These controls fail closed without signed official-release metadata and are not an update path for this preview.

For a signed installation, replacement defers while Sessions are active unless every named Session is explicitly confirmed. The detached helper retains the old package and a compatible database snapshot, checks the replacement's health and identity, and attempts one rollback if health fails or the helper is interrupted. There is no separate manual rollback command.

## Uninstall and remove user data

Normal uninstall preserves configuration, session data, repositories, worktrees, and branches:

- Windows preview: stop SylvOps, then remove the extracted application files or `%LOCALAPPDATA%\SylvOps\bin\sylvops.exe`. Do not delete the parent data directory as an uninstall shortcut.
- Signed Windows installation, when available: use **Installed apps** or `%LOCALAPPDATA%\Programs\SylvOps\uninstall.exe`.
- Debian or Ubuntu: run `sudo apt remove sylvops`.
- AppImage or portable archive: remove the downloaded file or extracted application directory.
- macOS preview: stop Sessions and the daemon, close clients, then remove `SylvOps.app` from its Applications location. Preserve the separate configuration and data directories.

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
- Stop if a download fails `SHA256SUMS`. For a verified macOS preview, use only the app-specific exception described above for unidentified-developer or unnotarized-app alerts. Never ignore malware or damaged-app alerts. Build from source if trust cannot be established; do not bypass Windows security warnings.
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
