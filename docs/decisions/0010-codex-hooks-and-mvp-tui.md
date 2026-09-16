# ADR-0010: Codex hooks and the replaceable MVP TUI

Status: implementation candidate; acceptance pending cross-platform validation.

## Decision

Codex and plain shells use one daemon-owned provider-adapter contract. Clients choose a known provider kind and optional model, effort, and prompt; they never submit executable paths. Codex is launched with structured arguments and a reviewed environment.

SylvOps adds observational hooks through a uniquely named `$CODEX_HOME/<name>.config.toml` layer selected with `--profile`. It does not modify the base user configuration or repository files. A fixed application-owned relay posts bounded JSON to an authenticated IPv4-loopback receiver. Hook output never approves, denies, rewrites, or steers provider behavior.

The Ratatui interface is an IPC client only. It displays the hierarchy, attention state, and bounded daemon-produced Git diff, then hands interactive attachment to the existing safe terminal client. Closing it does not own or stop sessions.

## Consequences

- Existing Codex authentication and user/project configuration remain owned by Codex and the user.
- Hook state is deterministic, bounded, and separate from terminal-text scraping.
- Scrollback remains memory-only and external session IDs are the only provider resume metadata persisted.
- The TUI can be replaced without changing daemon process ownership.
- This does not close the post-spawn Windows Job Object race; Windows MVP acceptance remains blocked until suspended spawn or equivalent containment is proven.
