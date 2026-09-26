---
status: accepted
---

# Use each platform's native installation scope

This supersedes ADR-0001 while retaining its continuously releasable application model, GitHub Releases source, user-approved signed upgrades, active-session protection, data-preserving removal, and portable fallbacks. Windows remains a per-user installation, macOS uses the user-selected Applications location, and Linux uses a package-manager-managed system installation for the Debian package because standard desktop and AppStream integration live in system paths; the Linux AppImage remains the no-installation, no-administrator fallback. This trades a universal per-user scope for the normal installation and removal behavior users expect on each operating system.
