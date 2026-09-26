# Installation and upgrades

## Outcome

A new user can install SylvOps as a normal desktop application, open it from the operating system, upgrade from inside the app, and uninstall it without a Rust toolchain or manual `PATH` editing. Maintainers can explicitly promote any green `main` commit as a semantic `0.x` release through GitHub Releases.

This supersedes the one-time release-cut plan. Its safety checks are repeatable release checks rather than a product phase.

## Settled product decisions

- `main` remains continuously releasable; publishing an application release is an explicit promotion.
- GitHub Releases is the source for installers, portable builds, release notes, and upgrade metadata.
- Versions use ordinary `0.x` semantic versions without release-phase suffixes.
- Windows x86_64, Linux x86_64, macOS x86_64, and macOS arm64 remain supported. Packaging work proceeds Windows, then macOS, then Linux when it cannot land together.
- Installation is per-user and does not require administrator access.
- Windows ships a signed installer with normal shortcuts and uninstall registration.
- macOS ships a Developer ID-signed, notarized application bundle in a DMG.
- Linux ships an AppImage and a Debian package with application metadata and a desktop entry.
- Portable archives remain available for advanced and recovery use, but are not the primary onboarding path.
- The app notifies users about an available upgrade, shows the version and release notes, and requires confirmation before installation.
- An upgrade never silently stops active sessions. It waits until they finish or requires a second explicit confirmation that names the sessions that will stop.
- One previous working version is retained for rollback until the upgraded app and daemon pass a health check.
- Uninstall removes application files and OS integration but preserves user data by default. Removing user data is a separate explicit option and never removes repositories or worktrees.
- Initial Codex prompts are not persisted. Eligible Codex sessions can be resumed from the desktop, TUI, and CLI.

## Packaging basis

The platform shape follows current vendor guidance and the capabilities of the Rust desktop ecosystem:

- [Microsoft's Windows packaging overview](https://learn.microsoft.com/windows/apps/package-and-deploy/packaging/) treats packaging as the boundary for install, update, integration, and clean uninstall, and requires trusted signing for direct distribution.
- [Apple's distribution guidance](https://developer.apple.com/documentation/xcode/distributing-your-app-for-beta-testing-and-releases) requires Developer ID signing and notarization for macOS applications distributed outside the App Store.
- [AppImage's packaging guide](https://docs.appimage.org/packaging-guide/index.html) defines portable Linux desktop packaging, update metadata, signing, and desktop integration.
- [`cargo-packager`](https://github.com/crabnebula-dev/cargo-packager) supports the selected Windows, macOS, and Linux package formats, while its [updater](https://docs.crabnebula.dev/packager/updater/) verifies application-specific signatures before installation.

## Current reality

The repository currently produces portable archives containing one executable. The install scripts download an archive, compare its SHA-256 digest with the release manifest, copy the executable to a user-local directory, and ask the user to modify `PATH`. They do not create application shortcuts, register uninstall, coordinate with a running daemon, retain a rollback version, or provide upgrade discovery.

The runtime already creates user-local state and configuration directories, so installed application files can remain separate from user data. The existing release workflow already builds the four supported targets, validates archive contents, creates checksums and provenance attestations, and publishes through GitHub Releases. These are foundations to evolve, not parallel systems to replace blindly.

## Delivery plan

### 1. Reconcile product truth and stored data

- Keep current product, development, packaging, workflow, and agent documentation centered on application releases. Preserve earlier phase decisions as clearly marked historical records rather than active roadmap items.
- Keep workspace, application, installer-default, and tag versions aligned on the promoted ordinary `0.x` version, beginning with `0.1.0`.
- Preserve the prompt-privacy invariant: initial prompts never enter `session_prompts` or persisted provider argument arrays.
- Retain the additive migration that deletes legacy prompt rows and scrubs prompt-bearing launch metadata; never edit the applied initial migration.
- Keep the persistence test that uses a unique prompt sentinel and asserts that it is absent from every SQLite text field after session creation.
- Add guarded Resume actions to desktop and TUI using the daemon's existing eligibility policy and typed `ResumeSession` request.

### 2. Establish application identity and packaging

- Add stable application identifiers, publisher metadata, icons, descriptions, license metadata, and platform-specific version mapping.
- Adopt one pinned, reproducible Rust packaging toolchain. Start by validating `cargo-packager`, because it produces NSIS/WiX installers, macOS application bundles and DMGs, AppImages, and Debian packages and has a compatible signed updater.
- Produce a per-user Windows installer that installs the executable, creates a Start Menu entry, optionally exposes the CLI, registers uninstall, and leaves the state directory untouched.
- Produce a universal product experience across the two macOS architectures: an application bundle inside a signed and notarized DMG, with the CLI treated as an optional advanced entry point.
- Produce an AppImage and Debian package with an icon, AppStream metadata, and `.desktop` entry on Linux.
- Retain validated portable archives as secondary assets.

### 3. Define the signed upgrade contract

- Publish bounded upgrade metadata per platform and architecture: current version, minimum supported source version, installer URL, byte length, cryptographic digest, updater signature, release notes URL, and publication timestamp.
- Generate a dedicated upgrade-signing keypair. Embed only the public key in the application; keep the private key and password in protected release secrets with a documented rotation and recovery procedure.
- Sign Windows application and installer binaries with an appropriate trusted code-signing identity.
- Sign macOS application code with Developer ID, enable the hardened runtime, notarize the DMG, and staple the notarization ticket.
- Require every release asset and upgrade manifest to agree on version, target, digest, and signature before publication.
- Fail closed on an unknown target, invalid signature, downgrade, replayed manifest, oversized metadata, or incompatible version.

### 4. Implement the upgrade state machine

- Check for upgrades on explicit request and periodically with a bounded interval. Allow users to disable periodic checks.
- Present an unobtrusive availability notice with version, size, and release notes; never begin installation before confirmation.
- Download to a bounded temporary location, verify length, digest, and updater signature, then stage without modifying the active installation.
- Ask the daemon for active-session state. Defer by default when any session is live; the override confirmation lists the affected sessions and explains that their process trees will stop.
- Quiesce clients, stop the daemon cleanly, install through a separate helper or platform installer, and relaunch the new version.
- Run a bounded health handshake covering version, protocol, database migration, and executable identity. Roll back once if that handshake fails, and preserve a redacted diagnostic.
- Serialize install, upgrade, rollback, and uninstall operations so two clients cannot race application replacement.

### 5. Complete first-run setup and documentation

- Make desktop launch without a repository a supported first-run state with a native repository picker and the existing Workspace → Project/root Worktree → Session guidance.
- Publish one short installation guide per operating system, plus upgrade, rollback, uninstall, user-data location, and troubleshooting sections.
- Document Git and Codex as separately discovered dependencies. Do not install tools, copy credentials, or run login commands on the user's behalf.
- Explain how the optional CLI is exposed on each platform without requiring every desktop user to edit `PATH`.
- Replace archive-specific warning workarounds with the expected signed/notarized installation path.

### 6. Make distribution continuously releasable

- Publish an ordinary GitHub Release only from an exact semantic version tag that agrees with the workspace and packaged application version.
- Build installers, application bundles, Linux packages, portable archives, signed upgrade payloads, checksums, attestations, and the upgrade manifest from the same commit.
- Test clean install, launch, upgrade from the previous release, daemon coordination, rollback, uninstall-with-data-preservation, and reinstall on native hosts.
- Continue running formatting, Clippy with warnings denied, and the complete workspace test suite on Windows, Linux, and macOS before promotion.
- Publish only when all platform assets and metadata pass validation. A partial platform release is a failed promotion, not a degraded success.

## Release acceptance

An application release is ready when all of the following are true:

- A clean supported host installs and launches SylvOps through the normal OS application surface without Rust, a compiler, archive extraction, or manual `PATH` changes.
- The installed version and connected daemon version agree.
- A previous installed release discovers the new release, verifies it, upgrades, relaunches, and preserves configuration and session metadata.
- Invalid or tampered upgrade metadata and payloads are rejected without altering the installation.
- Live sessions are never terminated without the named explicit confirmation.
- A failed post-upgrade health check restores the previous working version.
- Normal uninstall removes the app but preserves user data; explicit data removal still never touches repositories or worktrees.
- No initial prompt is present in persisted SQLite content.
- Eligible Codex sessions resume from desktop, TUI, and CLI into new session records.
- All four supported target builds pass their native install and upgrade checks.

## Deferred scope

- Silent background installation without confirmation.
- Microsoft Store, Mac App Store, Flathub, Snap Store, or other store distribution.
- Windows arm64 and Linux arm64 packages.
- System-wide or administrator-managed installation.
- Automatic removal of user data, repositories, worktrees, or branches.
- Feature expansion such as additional providers, GitHub workflows, remote execution, and notifications until installation and upgrades meet this plan's acceptance criteria.
