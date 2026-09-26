# ADR-0011: Atomic Windows ConPTY and Job Object launch

> **Historical context:** release-phase terminology below records when this decision was made. Current delivery uses ordinary semantic `0.x` application releases.

## Status

Accepted for the cross-platform beta candidate; hosted Windows acceptance remains required.

## Context

The previous Windows path spawned through `portable-pty` and then called `AssignProcessToJobObject`. A child or descendant could run before containment, and assignment can fail when the process has already entered an incompatible job. The public `portable-pty` interface does not expose additional `STARTUPINFOEX` process attributes.

## Decision

SylvOps owns a small Windows-only ConPTY launch backend implemented from the documented Win32 contract. It creates the pseudoconsole and kill-on-close Job Object first, then supplies `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` and `PROC_THREAD_ATTRIBUTE_JOB_LIST` together to `CreateProcessW`. Unix continues using `portable-pty` and verified process groups.

The Windows boundary:

- constructs a mutable UTF-16 command line from a structured executable and argument vector;
- constructs a sorted Unicode environment block and rejects NULs, equals signs in names, and case-insensitive duplicate names;
- transfers every kernel handle into an RAII owner immediately;
- returns no session unless process creation atomically applied both attributes;
- terminates descendants through the Job Object and waits on the root process handle.

## Consequences

The unsafe Win32 surface is isolated in one module and requires Windows-specific unit and integration tests. The minimum supported Windows version is Windows 10 1809. Any future PTY-library replacement must preserve atomic containment rather than restoring post-spawn assignment.
