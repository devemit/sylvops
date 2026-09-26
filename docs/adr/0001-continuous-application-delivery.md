---
status: accepted
---

# Ship SylvOps as a continuously upgraded application

SylvOps ships through explicitly promoted semantic `0.x` application releases from a continuously releasable `main`, rather than through beta or stable phases. GitHub Releases is the distribution and upgrade source; Windows, Linux, macOS x86_64, and macOS arm64 remain supported, with platform work delivered in that order when it cannot land atomically. The primary distribution is an OS-integrated per-user application, upgrades are signed and user-approved, active sessions are never stopped without explicit confirmation, uninstall preserves user data by default, and portable builds remain an advanced fallback. This trades a larger packaging and signing surface for the normal installation, upgrade, rollback, and removal experience expected of a desktop application.
