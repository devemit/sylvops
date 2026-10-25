# SylvOps vt100 patch

Based on the unmodified crates.io vt100 0.16.2 source, under the accompanying MIT
license. The workspace patches this exact version locally.

`Screen::set_size_reflow(rows, columns, scrollback_bytes)` opts into normal-screen
cell reflow. `set_size` retains upstream behavior. Alternate screens use upstream
grid resizing. Reflow preserves cell attributes, explicit line breaks, soft wraps,
and cursor offsets. History is bounded by both the supplied cell-storage budget
and 10,000 rows; the visible screen remains bounded by application protocol limits.
History and terminal state are never persisted. At a one-column width a wide
glyph occupies one cell until enough width is available again.

Application regression coverage lives in sylvops-tui/tests/reflow.rs. Keep this
patch isolated when updating upstream; do not replace it with replay of raw
terminal bytes, which may contain cursor-addressed application output.

The companion Parser::process_snapshot applies out-of-band snapshots with a
separate decoder, preserving split UTF-8/escape sequences for ordered resize
checkpoints and resetting pending decoder state after a gap.
