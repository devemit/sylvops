const BUDGET: usize = 64 * 1024;

#[test]
fn shrinking_and_widening_preserve_sentence_end() {
    let mut parser = vt100::Parser::new(8, 80, 0);
    parser.process(b"Sentence begins here and continues to the very end MARKER");
    parser.screen_mut().set_size_reflow(8, 44, BUDGET);
    assert!(parser.screen().contents().contains("MARKER"));
    parser.screen_mut().set_size_reflow(8, 80, BUDGET);
    assert_eq!(
        parser.screen().contents(),
        "Sentence begins here and continues to the very end MARKER"
    );
}

#[test]
fn reflow_preserves_hard_breaks_spaces_styles_and_cursor() {
    let mut parser = vt100::Parser::new(10, 20, 0);
    parser.process(b"\x1b[31mfirst  line\r\nsecond line\x1b7");
    for width in [6, 30, 4, 20] {
        parser.screen_mut().set_size_reflow(10, width, BUDGET);
        assert_eq!(parser.screen().contents(), "first  line\nsecond line");
        assert_eq!(
            parser.screen().cell(0, 0).unwrap().fgcolor(),
            vt100::Color::Idx(1)
        );
    }
    assert_eq!(parser.screen().cursor_position(), (1, 11));
    parser.process(b"\x1b[H\x1b8!");
    assert!(parser.screen().contents().ends_with("second line!"));
}

#[test]
fn reflow_preserves_wide_and_combining_characters() {
    let mut parser = vt100::Parser::new(12, 20, 0);
    parser.process("ab界e\u{301}界 marker".as_bytes());
    for width in [4, 1, 8, 20] {
        parser.screen_mut().set_size_reflow(12, width, BUDGET);
    }
    assert_eq!(parser.screen().contents(), "ab界e\u{301}界 marker");
}

#[test]
fn overflow_is_retained_and_history_is_bounded() {
    let mut parser = vt100::Parser::new(4, 80, 0);
    parser.process(b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789");
    parser.screen_mut().set_size_reflow(4, 4, BUDGET);
    parser.screen_mut().set_scrollback(usize::MAX);
    assert!(parser.screen().contents().starts_with("abcdefghijklmnop"));
    parser.screen_mut().set_scrollback(0);
    parser.screen_mut().set_size_reflow(4, 80, BUDGET);
    assert!(parser.screen().contents().ends_with("0123456789"));
    parser.screen_mut().set_size_reflow(4, 1, 64);
    parser.screen_mut().set_scrollback(usize::MAX);
    assert!(parser.screen().scrollback() <= 1);
}

#[test]
fn alternate_screen_keeps_grid_resize_behavior() {
    let mut parser = vt100::Parser::new(8, 80, 0);
    parser.process(b"normal marker\x1b[?1049hfull screen marker");
    parser.screen_mut().set_size_reflow(8, 6, BUDGET);
    assert!(parser.screen().alternate_screen());
    assert!(!parser.screen().contents().contains("marker"));
    parser.process(b"\x1b[?1049l");
    assert_eq!(parser.screen().contents(), "normal marker");
}

#[test]
fn wide_glyph_padding_does_not_become_an_extra_space() {
    let expected = "ab界cd界ef marker";
    let mut parser = vt100::Parser::new(30, 80, 0);
    parser.process(expected.as_bytes());
    for width in [3, 5, 7, 1, 2, 4, 80] {
        parser.screen_mut().set_size_reflow(30, width, BUDGET);
    }
    assert_eq!(parser.screen().contents(), expected);
}

#[test]
fn repeated_resizes_preserve_wrap_pending_cursor_and_future_output() {
    let mut parser = vt100::Parser::new(20, 10, 0);
    parser.process(b"0123456789");
    for width in [5, 10, 2, 10] {
        parser.screen_mut().set_size_reflow(20, width, BUDGET);
    }
    parser.process(b"tail");
    assert_eq!(parser.screen().contents(), "0123456789tail");
}

#[test]
fn all_ascii_widths_round_trip_without_losing_or_inserting_text() {
    let expected = "A long sentence with spaces, punctuation, and a verylongwordENDING.";
    let mut parser = vt100::Parser::new(100, 80, 0);
    parser.process(expected.as_bytes());
    for width in 1..=80 {
        parser.screen_mut().set_size_reflow(100, width, BUDGET);
        assert_eq!(parser.screen().contents(), expected, "width {width}");
    }
}

#[test]
fn ordered_snapshots_preserve_split_utf8_and_escape_sequences() {
    let mut parser = vt100::Parser::new(8, 80, 0);
    let glyph = "λ".as_bytes();
    parser.process(&glyph[..1]);
    parser.screen_mut().set_size_reflow(8, 44, BUDGET);
    let snapshot = parser.screen().state_formatted();
    parser.process_snapshot(&snapshot, true);
    parser.process(&glyph[1..]);
    assert_eq!(parser.screen().contents(), "λ");
    parser.process(b"\x1b[31");
    let snapshot = parser.screen().state_formatted();
    parser.process_snapshot(&snapshot, true);
    parser.process(b"mred");
    assert_eq!(
        parser.screen().cell(0, 1).unwrap().fgcolor(),
        vt100::Color::Idx(1)
    );
    assert_eq!(parser.screen().contents(), "λred");
}

#[test]
fn history_never_exceeds_ten_thousand_rows() {
    let mut parser = vt100::Parser::new(2, 1, 0);
    parser.screen_mut().set_size_reflow(2, 1, 64 * 1024 * 1024);
    parser.process("x\r\n".repeat(10_010).as_bytes());
    parser.screen_mut().set_scrollback(usize::MAX);
    assert_eq!(parser.screen().scrollback(), 10_000);
}
