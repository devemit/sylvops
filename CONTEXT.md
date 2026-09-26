# SylvOps

SylvOps is a local application for supervising coding-agent sessions that run in real Git worktrees. This glossary names the product concepts that must remain stable across the daemon, clients, packaging, and documentation.

## Language

**Application release**:
A deliberately promoted, versioned SylvOps build that users can install or upgrade to.
_Avoid_: Release-phase labels, release candidate, build artifact

**Installation**:
An OS-integrated, per-user copy of SylvOps with a normal launch entry and an uninstall path.
_Avoid_: Extracted archive, copied binary, setup script

**Upgrade**:
A user-approved replacement of an installation that preserves user data and never silently terminates active sessions.
_Avoid_: Reinstall, silent update, overwrite

**Portable build**:
A standalone executable archive that runs without OS application registration and remains an advanced fallback distribution.
_Avoid_: Installer, installed app

**User data**:
SylvOps-owned configuration, preferences, logs, and session metadata stored outside an installation. Repositories and worktrees are user content, never SylvOps user data.
_Avoid_: Application files, installation data
