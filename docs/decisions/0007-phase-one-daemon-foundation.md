# ADR-0007: Phase 1 daemon foundation

- Status: accepted
- Date: 2026-09-16

## Decision

The Phase 1 control plane is one authoritative daemon, a dedicated SQLite owner thread, and one bounded Tokio task per authenticated local client. Clients must read a high-entropy per-start token from the private runtime directory and send it in their first `Hello` frame. A protocol-major mismatch, newer unsupported minor version, invalid token, malformed frame, or non-`Hello` first request fails closed.

The CLI starts the same executable in its `daemon run` role using structured arguments, redirects diagnostics to a local log, and waits for an authenticated health response. `status`, `snapshot`, and `stop` are short-lived clients. Closing them does not stop the daemon.

SQLite migrations are embedded and checksum-validated. Configuration is TOML rather than database state, retains unknown top-level fields, accepts a recursive machine-local overlay, and is created with atomic replacement. On restart, active-looking session rows become `disconnected`; persisted PIDs are never trusted.

## Concurrency and cleanup

No SQLite connection leaves its owner thread. Database commands use a bounded channel, and no transaction crosses an asynchronous wait. Listener failure or explicit shutdown broadcasts cancellation to clients, joins connection tasks, stops the database actor, and removes the authentication token only if it still contains this daemon's token.

## Platform split

- Unix uses a mode-`0600` Unix-domain socket below a mode-`0700` runtime directory. A stale socket is removed only after file-type and owner validation and a failed live connection probe.
- Windows uses a deterministic local named-pipe name derived from the runtime directory, rejects remote clients, applies a protected owner-only DACL to every pipe instance, and requires the per-start token. Detached process flags replace Unix process-group detachment. Connected-client SID validation remains defense-in-depth release hardening.
- SQLite is bundled on Unix and Windows to avoid depending on a machine-global SQLite ABI. Windows compilation therefore requires a supported C toolchain in addition to Rust and the Windows SDK.

## Verification

Source formatting and workspace-wide Clippy/type checking pass; the latter used rusqlite's system-link mode solely to bypass compilation of bundled SQLite C code on this host. Core unit tests pass. The Phase 1 suite includes migration, newer-schema rejection, reconciliation, configuration, authentication, concurrent-client, snapshot, and clean-shutdown coverage. On this development host, daemon runtime execution remains gated by the absent Windows C/MSVC SDK toolchain required by the checked-in bundled SQLite mode; CI must run the full suite on Windows and Unix before Phase 1 is considered release-verified.

## Consequences

This establishes the durable control plane without prematurely exposing session or Git mutations. Phase 2 can add production PTY actors and streaming events behind the authenticated connection while keeping the CLI/TUI replaceable.
