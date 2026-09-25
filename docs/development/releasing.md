# Releasing

SylvOps is a beta implementation candidate, not a stable release.

For local Windows testing, `scripts/package-windows.ps1` builds an unsigned native MSVC x64 ZIP. Its launcher delegates to `sylvops open` and never starts a provider implicitly. `scripts/install.ps1` and `scripts/install.sh` install a pinned GitHub release to a user-local directory after checksum verification without silently editing `PATH`.

Tags must exactly match the workspace version. `.github/workflows/release.yml` first runs the complete CI workflow, builds and validates Windows x86_64, Linux x86_64, macOS x86_64, and macOS arm64 archives, creates one `SHA256SUMS`, attaches build-provenance attestations, and stages the exact candidate as a workflow artifact. The `publish` job uses the protected `beta-release` environment and must have required reviewers with no release-operator bypass. It revalidates and publishes the same staged bytes only after the complete [fresh-machine release proof](../wayfinding/mvp-beta/tickets/release-proof.md) passes and beta cut approves publication. The first expected tag is `v0.1.0-beta.1`.

A stable release additionally requires signed/notarized packages, migration compatibility tests, process/resource soak tests, redacted diagnostics review, and an explicit security review. Planned controls must remain absent or visibly disabled.
