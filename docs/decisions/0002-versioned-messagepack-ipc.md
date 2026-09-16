# ADR-0002: Versioned length-prefixed MessagePack IPC

- Status: accepted for Phase 0
- Date: 2026-09-15

## Decision

Use a fixed, validated binary header and a typed MessagePack body over local Unix sockets or Windows named pipes. Negotiate protocol versions before other operations. Bound every frame to 1 MiB and PTY chunks to 64 KiB.

## Rationale

Length-prefixing supports incremental stream reads and early size rejection. A fixed header makes version, class, operation, identity, and correlation available before body decoding. MessagePack keeps shared Rust request/event types compact without designing an undocumented custom serialization format.

## Consequences

Schema evolution needs explicit compatibility rules and protocol tests. Local transport security remains platform-specific. Phase 1 must add a current-user Windows pipe DACL and peer validation before the listener is production-ready.
