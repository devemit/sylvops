# Platform assumptions

Windows and Unix share domain, protocol, actor, and safety semantics but not OS resource ownership.

| Concern | Unix | Windows |
|---|---|---|
| IPC | Unix-domain socket, mode 0600, peer UID | Named pipe, current-user DACL, client SID |
| PTY | POSIX PTY | ConPTY |
| Process tree | new session/process group and group signals | Job Object with kill-on-close |
| Detachment | detached session and redirected standard handles | detached/no-window flags and redirected handles |
| Filesystem | symlinks, usually case-sensitive | drive roots, case folding, junctions/reparse points |
| Executables | native binaries and documented shebang behavior | executable and command-shim resolution |

Windows, Linux, and macOS are release-blocking platforms. The release workflow builds Windows x86_64 MSVC, Linux x86_64 glibc, macOS x86_64, and macOS arm64 artifacts only after the complete hosted test matrix passes. Cross-compilation is useful static evidence but never substitutes for native PTY, IPC, path, and process-tree tests.
