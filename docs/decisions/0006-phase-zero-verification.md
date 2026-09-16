# ADR-0006: Phase 0 verification record

- Status: in progress
- Date: 2026-09-15

## Implemented proofs

- A fixed-header, length-prefixed `MessagePack` frame codec with version, class, opcode, message ID, correlation ID, and payload length.
- Early frame-size rejection and typed request/response payloads.
- Unix-socket and Windows named-pipe transport abstractions with a runnable Phase 0 health server/client.
- A daemon-side PTY owner task with structured process launch, input, resize, sequence-numbered output, bounded replay, and explicit gap reporting.
- Windows Job Object and Unix process-group termination controllers.
- A fake agent supporting interactive input, burst output, child-process heartbeat, and explicit exit.
- Integration tests for local transport, detach/replay, large-output eviction, PTY resize/input, and descendant termination.

The owner task waits for both process reaping and PTY reader completion before publishing exit, preventing loss of final output.

## Local verification

On the current Windows host:

- `cargo metadata --no-deps --format-version 1`: passed.
- `cargo fmt --all -- --check`: passed.
- full workspace type checking using Rust 1.98.1 `windows-gnullvm`: passed.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`: passed using the same target.
- native test execution: blocked before test startup because the host has neither MSVC Build Tools/Windows SDK nor a complete LLVM-MinGW SDK. Rust code reached and passed type checking; final executables could not link due to missing Windows system import libraries.

The CI matrix is configured for Windows, Linux, and macOS and will execute the authored runtime tests once connected to a repository runner. Phase 0 is not considered complete until those tests execute successfully, especially the Windows descendant-termination test and the Unix process-group test.

## Remaining acceptance work

- Execute the suite with a complete platform linker/SDK.
- Confirm whether post-spawn Windows Job assignment is acceptable. It is securely performed using the child's process handle, but the create-suspended/assign/resume race remains.
- Enforce a current-user named-pipe DACL and validate the client SID in Phase 1.
- Validate alternate-screen and parser-derived terminal resynchronization in Phase 2; Phase 0 proves bounded raw replay only.
