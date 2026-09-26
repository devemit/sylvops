# Releasing

SylvOps uses explicitly promoted semantic `0.x` application releases. A green `main` commit is releasable, but publication remains a deliberate versioned promotion.

Windows releases contain two x86_64 assets: the primary signed per-user NSIS installer and the portable ZIP fallback. The installer puts application files in `%LOCALAPPDATA%\Programs\SylvOps`, creates a Start Menu entry, registers uninstall and the current-user `sylvops.exe` App Paths entry, and requires no administrator privileges. It never installs into `%LOCALAPPDATA%\SylvOps` or `%APPDATA%\SylvOps`; those locations hold user data and roaming configuration and survive normal uninstall. Repositories, worktrees, and branches are outside the installation and are never removed.

The packaging toolchain is locked to cargo-packager 0.11.8 in `packaging/cargo-packager.version`. `Packager.toml` owns the stable `com.devemit.sylvops` application ID, publisher, version, icon, current-user mode, downgrade policy, and vendored NSIS template. The executable embeds matching product, publisher, version, and icon resources. The NSIS template comes from the pinned cargo-packager source and moves application files under `LocalAppData\Programs` so they cannot collide with runtime data.

For local installer testing on Windows, install the exact packager and use a code-signing certificate already available in the current-user certificate store:

```powershell
cargo install cargo-packager --version 0.11.8 --locked
./scripts/package-windows-installer.ps1 -CertificateThumbprint <SHA1> -TimestampUrl <RFC3161 URL> -RequireTrustedSignature
./scripts/test-windows-installer.ps1 -InstallerPath dist/sylvops-windows-x86_64-setup.exe -CertificateThumbprint <SHA1> -RequireTrustedSignature -RequireTimestamp
```

The smoke test installs silently as the current user, verifies Authenticode and native registration, exercises the installed CLI, starts the desktop and daemon, uninstalls, and proves that SylvOps data, configuration, a repository, worktree, and branch remain. CI runs this same path with an isolated one-day self-signed test certificate; tagged releases require the trusted publisher identity.

The protected `release` environment must define `WINDOWS_SIGNING_CERTIFICATE_BASE64` as a base64-encoded PFX, `WINDOWS_SIGNING_CERTIFICATE_PASSWORD` as its password, and `WINDOWS_SIGNING_TIMESTAMP_URL` as an RFC 3161 timestamp service URL. The workflow writes the PFX only under the ephemeral runner directory, imports it into the current-user certificate store, deletes the PFX immediately, signs the executable before packaging, signs the installer, requires both signatures to validate as trusted, and removes the imported certificate after the job. Secrets and certificate bytes must never be committed or printed.

For local portable testing, `scripts/package-windows.ps1` builds the unsigned native MSVC x64 ZIP. Its launcher delegates to `sylvops open` and never starts a provider implicitly. `scripts/install.ps1` and `scripts/install.sh` retain the portable, checksum-verified installation path without silently editing `PATH`.

Tags must be ordinary semantic `0.x` versions and exactly match the workspace version. `.github/workflows/release.yml` first runs the complete CI workflow, builds and validates Windows x86_64, Linux x86_64, macOS x86_64, and macOS arm64 archives, creates one `SHA256SUMS`, attaches build-provenance attestations, and stages the exact bytes as a workflow artifact. The `publish` job uses the protected `release` environment, revalidates the staged bytes, and publishes a normal GitHub Release. The next expected tag is `v0.1.0`.

Portable builds remain a fallback. Signed OS-integrated packages for macOS and Linux, migration compatibility tests, process/resource soak tests, redacted diagnostics review, and an explicit security review remain planned controls and must not be advertised as shipped.
