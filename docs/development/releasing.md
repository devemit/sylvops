# Releasing

SylvOps uses explicitly promoted semantic `0.x` application releases. A green `main` commit is releasable, but publication remains a deliberate versioned promotion.

For local Windows testing, `scripts/package-windows.ps1` builds an unsigned native MSVC x64 ZIP. Its launcher delegates to `sylvops open` and never starts a provider implicitly. `scripts/install.ps1` and `scripts/install.sh` install a pinned GitHub release to a user-local directory after checksum verification without silently editing `PATH`.

Tags must be ordinary semantic `0.x` versions and exactly match the workspace version. `.github/workflows/release.yml` first runs the complete CI workflow, builds and validates Windows x86_64, Linux x86_64, macOS x86_64, and macOS arm64 archives, creates one `SHA256SUMS`, attaches build-provenance attestations, and stages the exact bytes as a workflow artifact. The `publish` job uses the protected `release` environment, revalidates the staged bytes, and publishes a normal GitHub Release. The next expected tag is `v0.1.0`.

Current releases retain unsigned portable builds as a fallback. Signed OS-integrated packages, migration compatibility tests, process/resource soak tests, redacted diagnostics review, and an explicit security review remain planned controls and must not be advertised as shipped.
