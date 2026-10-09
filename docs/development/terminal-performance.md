# Desktop terminal performance

The desktop terminal keeps the daemon-owned VT parser authoritative. Presentation may cache only
derived visible-row cells and display runs; input, raw output, scrollback, and terminal state are
never cached as client authority.

## Responsiveness contract

- A deterministic fake-clock harness covers every phase offset across the former 16 ms polling
  interval. Echo arrives one millisecond after input and must be visible within an additional eight
  milliseconds.
- The same harness sends 200 keys, requires the exact ordered byte stream, and confirms that no text
  appears before daemon-owned echo. A focused bridge test drives those bytes through the real
  batching, flush, and acknowledgement path.
- Terminal events use a bounded 4,096-item queue and are processed in bounded 512-event drains.
  Critical events are drained first, so sustained terminal output cannot starve connection or
  operation feedback. Overflow continues to trigger authoritative resynchronization.

Run the deterministic checks with:

```text
cargo test -p sylvops-desktop wake_driven_echo_meets_every_phase_offset_and_preserves_a_200_key_burst
cargo test -p sylvops-desktop two_hundred_keys_use_the_ordered_batch_and_consume_successful_acknowledgement
cargo test -p sylvops-desktop heavy_terminal_output_stays_bounded_and_cannot_starve_critical_events
cargo test -p sylvops-test-support fake_provider_preserves_two_hundred_tagged_inputs_and_reports_echo_latency -- --nocapture
```

## Styled-screen benchmark

The ignored release-mode benchmark updates and presents a 200-row by 300-column terminal containing
48,000 alternating ANSI-styled cells. It includes VT update, dirty-row detection, display-run
assembly, and Iced span construction. Timing is measurement evidence rather than a portable
correctness assertion; the deterministic harness remains the correctness gate. One invocation runs
both the uncached reference reconstruction and the cached path against identical updates.

Run it with:

```text
cargo test --release -p sylvops-desktop large_styled_terminal_frame_benchmark -- --ignored --nocapture
```

Windows results recorded on 2026-10-10 from 40 measured frames after five warmups:

| Implementation | p50 | p95 | maximum |
| --- | ---: | ---: | ---: |
| Wake/input changes with full reconstruction | 8.994 ms | 9.685 ms | 9.969 ms |
| Derived dirty-row display cache | 6.399 ms | 7.037 ms | 7.358 ms |

The production daemon Session actor plus fake-provider PTY smoke also preserved all 200 tagged
inputs in order. Its separate end-to-end measurement reported p95 15.111 ms and maximum 17.899 ms;
operating-system scheduling and PTY/provider time make that diagnostic measurement distinct from
the deterministic eight-millisecond application-added correctness budget.

Full reconstruction narrowly missed the accepted eight-millisecond budget, so measured presentation
caching was required. The cache compares visible VT cells exactly and rebuilds only changed rows.
It rebuilds cursor rows when position, visibility, focus, or shape changes; clears derived rows for
resize, scrollback, and selection changes; detects style and wide-cell changes from the cells; and
rebuilds across alternate-screen transitions. Palette mapping remains downstream and is applied on
every presentation pass. Cached and uncached output are regression-compared across those states.
