# ADR-0004: Bounded raw scrollback plus terminal state

- Status: accepted for Phase 0
- Date: 2026-09-15

## Decision

Assign a monotonic sequence to each raw PTY output chunk and retain chunks in a byte-bounded ring. Never allow a client queue to grow without limit. Duplicate sequences are ignored; eviction produces an explicit output-gap response.

The production reattach protocol also needs parser-derived terminal state, because an evicted raw stream may begin in the middle of UTF-8 or an ANSI escape sequence. Use an established VT parser rather than implementing one from scratch.

## Consequences

Phase 0 proves bounded replay mechanics. Alternate-screen and parser snapshot behavior remains a Phase 2 acceptance item. Scrollback is memory-only initially and is lost when the daemon exits.
