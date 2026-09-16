# Releasing

SylvOps is not release-ready. The Codex-first MVP exists as an implementation candidate, not an accepted release.

For local testing, `scripts/package-windows.ps1` builds an unsigned portable x64 Windows ZIP through the checked-in MinGW Docker image. The package includes a one-click launcher that performs first-run repository and shell-session setup. This development artifact is not a substitute for signed release packaging.

A release requires passing Windows, Linux, and supported macOS CI; fake-provider end-to-end tests; race-free Windows process-tree containment; migration compatibility tests; process and resource soak tests; redacted diagnostics review; packaging verification; and an explicit security review. Planned controls must remain absent or visibly disabled.
