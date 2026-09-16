# Releasing

SylvOps is a beta implementation candidate, not a stable release.

For local Windows testing, `scripts/package-windows.ps1` builds an unsigned native MSVC x64 ZIP. Its launcher delegates to `sylvops open` and never starts a provider implicitly. `scripts/install.ps1` and `scripts/install.sh` install a pinned GitHub release to a user-local directory after checksum verification without silently editing `PATH`.

Tags must exactly match the workspace version. `.github/workflows/release.yml` first runs the complete CI workflow, builds Windows x86_64, Linux x86_64, macOS x86_64, and macOS arm64 archives, creates `SHA256SUMS`, attaches build-provenance attestations, and publishes a GitHub prerelease. The first expected tag is `v0.1.0-beta.1`.

A stable release additionally requires signed/notarized packages, migration compatibility tests, process/resource soak tests, redacted diagnostics review, and an explicit security review. Planned controls must remain absent or visibly disabled.
