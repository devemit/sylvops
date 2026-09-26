---
title: Make Codex discovery work for supported local installations
label: wayfinder:research
state: closed
assignee: null
blocked_by: []
---

> **Historical release-planning record:** this ticket belongs to the retired beta-phase plan and is not an active roadmap item or release gate.

## Question

Which native-executable discovery rules should SylvOps support for official Codex installations on each beta platform—especially Windows npm and Codex desktop installs—so that ordinary users are detected without ever executing `.cmd`, PowerShell, or other shell shims, and what bounded fixtures prove those rules safely?

## Resolution

SylvOps resolves Codex in this order and uses the first candidate that canonicalizes to a regular, executable native binary for the current platform:

1. An absolute `CODEX_CLI_PATH` value.
2. A native `codex` executable in the first 128 absolute `PATH` directories.
3. The platform-native payload inside an official `@openai/codex` npm installation adjacent to a `PATH` entry. On Unix, a canonical `codex` symlink may identify the package root. On Windows, the fixed npm package layout is inspected directly; `codex.cmd`, `codex.ps1`, and the extensionless Node shim are neither parsed nor executed.
4. The official standalone install's `~/.codex/packages/standalone/current` location.
5. Platform desktop locations:
   - Windows: the standalone launcher under `%LOCALAPPDATA%\Programs\OpenAI\Codex\bin` and at most 64 direct or 16-hex-versioned entries under the unpackaged and MSIX LocalCache Codex `bin` directories.
   - macOS: the bundled native CLI under system or per-user `ChatGPT.app` and legacy `Codex.app` application bundles.
   - Linux: no desktop-private binary is assumed; the supported CLI standalone and npm installations are covered by the earlier rules.

The supported beta targets are Windows, Linux, and macOS on x64 and ARM64. Candidate validation checks the PE, ELF, or Mach-O/fat-Mach-O file signature, respectively, before and after canonicalization. Unix candidates must also have an execute bit. A candidate of the wrong format, a script, a directory, a broken link, a relative override, or an unreadable file is skipped. Discovery never searches recursively, invokes a shell, executes a shim, reads shim contents, or trusts a filename alone.

The official OpenAI documentation currently offers standalone, npm, and Homebrew CLI installation paths and native desktop apps on the beta platforms. The standalone installer is the default CLI path documented for macOS and Linux; Windows desktop is distributed as a native app. See the [Codex CLI install guide](https://learn.chatgpt.com/docs/codex/cli), [Windows desktop guide](https://learn.chatgpt.com/docs/windows/windows-app), and [Linux desktop guide](https://learn.chatgpt.com/docs/linux/linux-app).

## Acceptance evidence

- A Windows npm fixture places inert `.cmd` and PowerShell shims beside the package and proves that the nested `codex.exe` payload is selected.
- A table-driven fixture covers the current official npm package/triple layout for Windows, Linux, and macOS on x64 and ARM64.
- A Windows desktop-cache fixture proves that only bounded, well-formed version directories participate and that the newest usable native payload wins.
- Negative fixtures reject a script with an executable-looking name and a native binary for the wrong platform.
- A precedence fixture proves that a valid explicit native path wins without broadening fallback discovery.
