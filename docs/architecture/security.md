# Security model

## Untrusted inputs

Repository contents and paths, IPC clients, hook payloads, provider output, Git output, GitHub content, provider configuration, and discovered executables are untrusted.

## Required controls

- Canonicalize and revalidate filesystem identity before privileged or destructive operations.
- Keep managed worktrees below an access-controlled root using opaque IDs rather than user text.
- Reject symlink and Windows junction escapes.
- Invoke Git, providers, `gh`, browsers, and repository commands using structured arguments, never shell interpolation.
- Bound frames, queues, PTY buffers, hook bodies, subprocess output, and diagnostic retention.
- Apply timeouts and cancellation to external operations.
- Start providers from a reviewed environment allowlist and never log environment values.
- Never persist prompts, provider transcripts, API keys, cookies, OAuth tokens, or copied CLI credentials.
- Strip URL user-info, query strings, and fragments from Git remote metadata before persistence.
- Audit security-relevant mutations and failures using redacted structured fields.
- Require visible user action for repository-defined commands and every destructive operation.

Managed worktree creation disables Git hooks for the `git worktree add` invocation, uses an opaque destination, and resolves the requested base to a commit before mutation. Git checkout filters and the working-tree materialization itself are still repository-controlled behavior; creation therefore remains an explicit user action. Removal requires `--confirm`, a fresh clean status including ignored files, and a state-bound daemon token. It never uses `--force` or recursive filesystem deletion.

## Process and identity rules

Socket permissions or pipe ACLs are necessary but not sufficient; peer identity is checked where the OS permits. Persisted PIDs are informational only and are never trusted after restart. Windows child trees are placed in Job Objects; Unix children use dedicated process groups.

## Terminal output

ANSI/VT output is data, not commands. Escape handling uses a maintained parser, and the Phase 2 CLI redraws parser-generated cells instead of forwarding raw PTY bytes. OSC, DCS, title, and clipboard sequences are therefore not emitted directly to the host terminal.
