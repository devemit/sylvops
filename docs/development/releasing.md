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

macOS releases contain a primary DMG and a portable archive for each supported architecture: `sylvops-macos-x86_64.dmg` for Intel and `sylvops-macos-aarch64.dmg` for Apple silicon. `packaging/macos/Packager.toml` carries the shared `com.devemit.sylvops` identity, version, name, icon, and minimum macOS version into the application bundle. `scripts/package-macos.sh` rejects mismatched target/architecture pairs, builds the native executable, creates the app, applies the hardened runtime and Developer ID signature, creates and signs the DMG, submits the DMG to Apple's notary service, and staples the accepted ticket.

For local package testing on the matching native Mac, install cargo-packager 0.11.8 and use a Developer ID Application identity already available to `codesign` plus an App Store Connect API key:

```sh
cargo install cargo-packager --version 0.11.8 --locked
./scripts/package-macos.sh --target aarch64-apple-darwin --architecture aarch64 --signing-identity 'Developer ID Application: Example (TEAMID)' --notary-key /path/to/AuthKey_KEYID.p8 --notary-key-id KEYID --notary-issuer-id ISSUER --require-developer-id
./scripts/test-macos-package.sh --dmg dist/sylvops-macos-aarch64.dmg --architecture aarch64 --expected-version 0.1.0 --require-developer-id --require-notarization
```

The native smoke test verifies the DMG signature and stapled ticket, asks Gatekeeper to assess it, mounts and copies the app, checks bundle metadata and executable architecture, launches it through Finder's LaunchServices path, verifies the daemon, removes the app, and proves that state, a repository, worktree content, and its branch survive. CI runs the same packaging path with ad-hoc signing on both native architectures; tagged releases require Developer ID signing and notarization.

The protected `release` environment must define `MACOS_SIGNING_CERTIFICATE_BASE64` as a base64-encoded Developer ID Application P12 and `MACOS_SIGNING_CERTIFICATE_PASSWORD` as its password. It must also define the variables `MACOS_SIGNING_IDENTITY`, `MACOS_NOTARY_KEY_ID`, and `MACOS_NOTARY_ISSUER_ID`, plus `MACOS_NOTARY_KEY_BASE64` as the base64-encoded App Store Connect API private key. The workflow imports these only into an ephemeral keychain and runner files, validates the requested identity, and removes them after packaging. Secrets, private keys, passwords, and certificate bytes must never be committed or printed.

Linux x86_64 releases contain `sylvops-linux-x86_64.AppImage`, `sylvops-linux-x86_64.deb`, and the portable archive. `packaging/linux/Packager.toml` carries the shared application identity, version, icon, desktop entry, and AppStream metadata into both native formats. The AppImage remains a portable executable and makes no installation changes; the Debian package installs the desktop integration and CLI through the platform package manager.

For local package testing on native x86_64 Linux, install cargo-packager 0.11.8 plus Xvfb and the normal graphical runtime libraries, then run:

```sh
cargo install cargo-packager --version 0.11.8 --locked
./scripts/package-linux.sh
./scripts/test-linux-packages.sh --appimage dist/sylvops-linux-x86_64.AppImage --deb dist/sylvops-linux-x86_64.deb --expected-version 0.1.0
```

The native smoke test inspects both packages, launches the AppImage and installed Debian application under a virtual display, verifies the daemon and desktop integration, removes the Debian package, and proves that state, a repository, worktree content, and its branch survive. When a previous release exists it also verifies candidate installation, rollback to the previous package, and candidate reapplication. CI runs this same path on a clean native Ubuntu x86_64 host.

Every platform also publishes a target-specific `sylvops-update-*.json` envelope. `UPDATE_SIGNING_PRIVATE_KEY_BASE64` is a protected 32-byte Ed25519 seed and `UPDATE_SIGNING_PUBLIC_KEY_BASE64` is its 32-byte public key; tagged binaries embed only the public key. The staging job writes the private seed to an ephemeral file, signs the exact version/target/URL/length/SHA-256/notes/timestamp metadata, verifies each envelope against the staged asset, deletes the seed, and only then builds `SHA256SUMS` and attestations. Rotate the key by generating a new offline pair, updating both protected secrets together, and promoting a bridge release signed by the old key with the new public key embedded. If the private key may be exposed, stop promotion, remove the secret, publish no unsigned workaround, and ship a manually verified recovery release through the existing platform-signing channels.

For local portable testing, `scripts/package-windows.ps1` builds the unsigned native MSVC x64 ZIP. Its launcher delegates to `sylvops open` and never starts a provider implicitly. `scripts/install.ps1` and `scripts/install.sh` retain the portable, checksum-verified installation path without silently editing `PATH`.

Tags must be ordinary semantic `0.x` versions and exactly match the workspace version. `.github/workflows/release.yml` first runs the complete CI workflow, including the signed staged-handoff test that forces the detached Linux helper to restore the installed executable, restart the old daemon, and report rollback. It builds and validates the Windows x86_64 installer and portable ZIP, Linux x86_64 AppImage, Debian package, and archive, and macOS x86_64 and arm64 notarized DMGs plus portable archives. When a prior published release exists, every native job downloads its matching package, upgrades it to the candidate, restores the previous package as a rollback probe, reapplies the candidate, and uploads its result for the promotion job to verify. It then verifies all five signed update envelopes against those exact native packages, emits `RELEASE-EVIDENCE.json` for the commit, four native package jobs, staged payloads, coordinator and detached-helper rollback tests, and native-package N-1 coverage, creates one `SHA256SUMS`, attaches build-provenance attestations, and stages the bytes as one indivisible workflow artifact. The initial `v0.1.0` release records N-1 package coverage as not applicable because no previous package exists; later versions require four passed results. The `publish` job uses the protected `release` environment, revalidates the full asset set, and publishes a normal GitHub Release. Missing, partial, mismatched, or unsigned assets fail promotion. The next expected tag is `v0.1.0`.

Portable builds remain a fallback. Store distribution, migration compatibility beyond the supported upgrade window, process/resource soak expansion, and additional signing services remain planned controls and must not be advertised as shipped.
