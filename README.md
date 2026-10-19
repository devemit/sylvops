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

The current public version is [`0.1.0-preview.10`](https://github.com/devemit/sylvops/releases/tag/v0.1.0-preview.10). Choose the appropriate installation path:

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
