use iced::keyboard::{self, Key, key::Named};
use sylvops_core::domain::AttachmentRole;

pub(crate) struct TerminalState {
    pub role: AttachmentRole,
    pub parser: vt100::Parser,
    pub last_sequence: u64,
    pub attached: bool,
    pub columns: u16,
    pub rows: u16,
    scroll_fraction: f32,
}

impl TerminalState {
    pub(crate) fn new(
        role: AttachmentRole,
        rows: u16,
        columns: u16,
        scrollback_rows: usize,
    ) -> Self {
        Self {
            role,
            parser: vt100::Parser::new(rows, columns, scrollback_rows),
            last_sequence: 0,
            attached: true,
            columns,
            rows,
            scroll_fraction: 0.0,
        }
    }

    pub(crate) fn scroll_lines(&mut self, lines: f32) {
        if !lines.is_finite() || lines == 0.0 {
            return;
        }
        let total = self.scroll_fraction + lines;
        let scroll_up = total.is_sign_positive();
        let mut remainder = total.abs().min(1_000.0);
        let mut whole_lines = 0_usize;
        while remainder >= 1.0 {
            remainder -= 1.0;
            whole_lines += 1;
        }
        self.scroll_fraction = remainder.copysign(total);
        if whole_lines == 0 {
            return;
        }

        let current = self.parser.screen().scrollback();
        let next = if scroll_up {
            current.saturating_add(whole_lines)
        } else {
            current.saturating_sub(whole_lines)
        };
        self.parser.screen_mut().set_scrollback(next);
    }

    pub(crate) fn scroll_page(&mut self, pages: isize) {
        let page_rows = usize::from(self.rows.saturating_sub(1).max(1));
        let current = self.parser.screen().scrollback();
        let next = if pages.is_positive() {
            current.saturating_add(page_rows.saturating_mul(pages.unsigned_abs()))
        } else {
            current.saturating_sub(page_rows.saturating_mul(pages.unsigned_abs()))
        };
        self.parser.screen_mut().set_scrollback(next);
        self.scroll_fraction = 0.0;
    }

    pub(crate) fn scroll_to_oldest(&mut self) {
        self.parser.screen_mut().set_scrollback(usize::MAX);
        self.scroll_fraction = 0.0;
    }

    pub(crate) fn prepare_for_input(&mut self) {
        self.parser.screen_mut().set_scrollback(0);
        self.scroll_fraction = 0.0;
    }

    pub(crate) fn is_scrolled_back(&self) -> bool {
        self.scrollback_rows() > 0
    }

    pub(crate) fn scrollback_rows(&self) -> usize {
        self.parser.screen().scrollback()
    }
}

pub(crate) fn encode_key(
    key: &Key,
    modifiers: keyboard::Modifiers,
    produced_text: Option<&str>,
) -> Option<Vec<u8>> {
    let mut bytes = match key.as_ref() {
        Key::Named(Named::Enter) => vec![b'\r'],
        Key::Named(Named::Space) => vec![b' '],
        Key::Named(Named::Tab) if modifiers.shift() => b"\x1b[Z".to_vec(),
        Key::Named(Named::Tab) => vec![b'\t'],
        Key::Named(Named::Backspace) => vec![0x7f],
        Key::Named(Named::Escape) => vec![0x1b],
        Key::Named(Named::ArrowUp) => b"\x1b[A".to_vec(),
        Key::Named(Named::ArrowDown) => b"\x1b[B".to_vec(),
        Key::Named(Named::ArrowRight) => b"\x1b[C".to_vec(),
        Key::Named(Named::ArrowLeft) => b"\x1b[D".to_vec(),
        Key::Named(Named::Home) => b"\x1b[H".to_vec(),
        Key::Named(Named::End) => b"\x1b[F".to_vec(),
        Key::Named(Named::Delete) => b"\x1b[3~".to_vec(),
        Key::Named(Named::Insert) => b"\x1b[2~".to_vec(),
        Key::Named(Named::PageUp) => b"\x1b[5~".to_vec(),
        Key::Named(Named::PageDown) => b"\x1b[6~".to_vec(),
        Key::Character(character) if modifiers.control() => {
            let character = character.chars().next()?;
            if !character.is_ascii() {
                return None;
            }
            let byte = u8::try_from(u32::from(character.to_ascii_uppercase())).ok()?;
            vec![byte & 0x1f]
        }
        Key::Character(character) => produced_text.unwrap_or(character).as_bytes().to_vec(),
        _ => return None,
    };
    if modifiers.alt() && bytes.first() != Some(&0x1b) {
        bytes.insert(0, 0x1b);
    }
    Some(bytes)
}

/// Produces a plain-text view of the maintained terminal screen and adds a
/// visible cursor marker. Provider escape sequences stay inside the VT parser.
pub(crate) fn display_contents(terminal: &TerminalState, focused: bool) -> String {
    let screen = terminal.parser.screen();
    let (cursor_row, cursor_column) = screen.cursor_position();
    let show_cursor = terminal.attached
        && terminal.role == AttachmentRole::Controller
        && !terminal.is_scrolled_back()
        && !screen.hide_cursor();
    let cursor_marker = if focused { '█' } else { '▯' };

    let mut last_row = cursor_row;
    for row in 0..terminal.rows {
        if (0..terminal.columns).any(|column| {
            screen
                .cell(row, column)
                .is_some_and(vt100::Cell::has_contents)
        }) {
            last_row = row;
        }
    }

    let mut contents = String::new();
    for row in 0..=last_row.min(terminal.rows.saturating_sub(1)) {
        let mut line = String::new();
        for column in 0..terminal.columns {
            if show_cursor && row == cursor_row && column == cursor_column {
                line.push(cursor_marker);
                continue;
            }
            let Some(cell) = screen.cell(row, column) else {
                line.push(' ');
                continue;
            };
            if cell.is_wide_continuation() {
                continue;
            }
            if cell.has_contents() {
                line.push_str(cell.contents());
            } else {
                line.push(' ');
            }
        }
        while line.ends_with(' ') {
            line.pop();
        }
        contents.push_str(&line);
        if row < last_row {
            contents.push('\n');
        }
    }
    contents
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detach_is_reserved_outside_encoder_and_navigation_is_encoded() {
        assert_eq!(
            encode_key(
                &Key::Named(Named::ArrowUp),
                keyboard::Modifiers::empty(),
                None,
            ),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            encode_key(&Key::Character("c".into()), keyboard::Modifiers::CTRL, None,),
            Some(vec![0x03])
        );
        assert_eq!(
            encode_key(
                &Key::Named(Named::Space),
                keyboard::Modifiers::empty(),
                Some(" "),
            ),
            Some(vec![b' '])
        );
    }

    #[test]
    fn display_adds_a_visible_focus_cursor_without_raw_escape_sequences() {
        let mut terminal = TerminalState::new(AttachmentRole::Controller, 4, 12, 100);
        terminal.parser.process(b"ready");

        assert_eq!(display_contents(&terminal, true), "ready█");
        assert_eq!(display_contents(&terminal, false), "ready▯");
    }

    #[test]
    fn history_scrolls_without_losing_the_live_input_row() {
        let mut terminal = TerminalState::new(AttachmentRole::Controller, 3, 12, 100);
        terminal
            .parser
            .process(b"one\r\ntwo\r\nthree\r\nfour\r\nfive");

        assert!(display_contents(&terminal, true).contains("five"));
        terminal.scroll_lines(2.0);
        assert!(terminal.is_scrolled_back());
        assert!(display_contents(&terminal, true).contains("two"));
        assert!(!display_contents(&terminal, true).contains('█'));

        terminal.prepare_for_input();
        assert!(!terminal.is_scrolled_back());
        let latest = display_contents(&terminal, true);
        assert!(latest.contains("five"));
        assert!(latest.contains('█'));
    }

    #[test]
    fn incoming_output_preserves_the_history_view_until_input_resumes() {
        let mut terminal = TerminalState::new(AttachmentRole::Controller, 3, 12, 100);
        terminal
            .parser
            .process(b"one\r\ntwo\r\nthree\r\nfour\r\nfive");
        terminal.scroll_lines(2.0);
        let before = display_contents(&terminal, true);

        terminal.parser.process(b"\r\nsix");

        assert_eq!(display_contents(&terminal, true), before);
        assert!(terminal.is_scrolled_back());
        terminal.prepare_for_input();
        assert!(display_contents(&terminal, true).contains("six"));
    }
}
