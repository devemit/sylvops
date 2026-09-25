use iced::keyboard::{self, Key, key::Named};
use sylvops_core::domain::AttachmentRole;
use sylvops_core::protocol::MAX_PTY_CHUNK_SIZE;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct CellPosition {
    row: u16,
    column: u16,
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct TerminalStyle {
    pub foreground: vt100::Color,
    pub background: vt100::Color,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
    pub selected: bool,
    pub cursor: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DisplayRun {
    pub text: String,
    pub style: TerminalStyle,
}

pub(crate) struct TerminalState {
    pub role: AttachmentRole,
    pub parser: vt100::Parser,
    pub last_sequence: u64,
    pub attached: bool,
    pub columns: u16,
    pub rows: u16,
    scroll_fraction: f32,
    selection_anchor: Option<CellPosition>,
    selection_focus: Option<CellPosition>,
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
            selection_anchor: None,
            selection_focus: None,
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
        self.clear_selection();
    }

    pub(crate) fn is_scrolled_back(&self) -> bool {
        self.scrollback_rows() > 0
    }

    pub(crate) fn scrollback_rows(&self) -> usize {
        self.parser.screen().scrollback()
    }

    pub(crate) fn begin_selection(&mut self, row: u16, column: u16) {
        let position = self.clamp_position(row, column);
        self.selection_anchor = Some(position);
        self.selection_focus = Some(position);
    }

    pub(crate) fn update_selection(&mut self, row: u16, column: u16) {
        if self.selection_anchor.is_some() {
            self.selection_focus = Some(self.clamp_position(row, column));
        }
    }

    pub(crate) fn finish_selection(&mut self) {
        if self.selection_anchor == self.selection_focus {
            self.clear_selection();
        }
    }

    pub(crate) fn clear_selection(&mut self) {
        self.selection_anchor = None;
        self.selection_focus = None;
    }

    pub(crate) fn selected_text(&self) -> Option<String> {
        let (start, end) = self.selection()?;
        let screen = self.parser.screen();
        let mut selected = String::new();

        for row in start.row..=end.row {
            let first_column = if row == start.row { start.column } else { 0 };
            let last_column = if row == end.row {
                end.column
            } else {
                self.columns.saturating_sub(1)
            };
            let line_start = selected.len();
            for column in first_column..=last_column {
                let Some(cell) = screen.cell(row, column) else {
                    selected.push(' ');
                    continue;
                };
                if cell.is_wide_continuation() {
                    continue;
                }
                if cell.has_contents() {
                    selected.push_str(cell.contents());
                } else {
                    selected.push(' ');
                }
            }
            while selected.len() > line_start && selected.ends_with(' ') {
                selected.pop();
            }
            if row < end.row {
                selected.push('\n');
            }
        }

        (!selected.is_empty()).then_some(selected)
    }

    fn clamp_position(&self, row: u16, column: u16) -> CellPosition {
        CellPosition {
            row: row.min(self.rows.saturating_sub(1)),
            column: column.min(self.columns.saturating_sub(1)),
        }
    }

    fn selection(&self) -> Option<(CellPosition, CellPosition)> {
        let anchor = self.selection_anchor?;
        let focus = self.selection_focus?;
        Some(if anchor <= focus {
            (anchor, focus)
        } else {
            (focus, anchor)
        })
    }

    fn is_selected(&self, row: u16, column: u16) -> bool {
        self.selection().is_some_and(|(start, end)| {
            let position = CellPosition { row, column };
            position >= start && position <= end
        })
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

pub(crate) fn encode_paste(contents: &str, bracketed: bool) -> Vec<u8> {
    let normalized = contents.replace("\r\n", "\n").replace('\r', "\n");
    let normalized = if bracketed {
        normalized
    } else {
        normalized.replace('\n', "\r")
    };
    let prefix = if bracketed {
        b"\x1b[200~".as_slice()
    } else {
        &[]
    };
    let suffix = if bracketed {
        b"\x1b[201~".as_slice()
    } else {
        &[]
    };
    let budget = MAX_PTY_CHUNK_SIZE.saturating_sub(prefix.len() + suffix.len());
    let mut end = normalized.len().min(budget);
    while !normalized.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }

    let mut bytes = Vec::with_capacity(prefix.len() + end + suffix.len());
    bytes.extend_from_slice(prefix);
    bytes.extend_from_slice(&normalized.as_bytes()[..end]);
    bytes.extend_from_slice(suffix);
    bytes
}

pub(crate) fn display_runs(terminal: &TerminalState, focused: bool) -> Vec<DisplayRun> {
    let screen = terminal.parser.screen();
    let (cursor_row, cursor_column) = screen.cursor_position();
    let show_cursor = terminal.attached
        && terminal.role == AttachmentRole::Controller
        && !terminal.is_scrolled_back()
        && !screen.hide_cursor();

    let mut last_row = cursor_row;
    for row in 0..terminal.rows {
        if (0..terminal.columns).any(|column| {
            terminal.is_selected(row, column)
                || screen.cell(row, column).is_some_and(|cell| {
                    cell.has_contents() || cell.bgcolor() != vt100::Color::Default
                })
        }) {
            last_row = row;
        }
    }

    let mut runs = Vec::new();
    for row in 0..=last_row.min(terminal.rows.saturating_sub(1)) {
        let last_column = (0..terminal.columns)
            .rfind(|column| {
                (show_cursor && row == cursor_row && *column == cursor_column)
                    || terminal.is_selected(row, *column)
                    || screen.cell(row, *column).is_some_and(|cell| {
                        cell.has_contents() || cell.bgcolor() != vt100::Color::Default
                    })
            })
            .unwrap_or(0);
        for column in 0..=last_column {
            let cell = screen.cell(row, column);
            if cell.is_some_and(vt100::Cell::is_wide_continuation) {
                continue;
            }
            let cursor = show_cursor && row == cursor_row && column == cursor_column;
            let text = if cursor && focused {
                "█"
            } else if cursor {
                "▯"
            } else if let Some(cell) = cell.filter(|cell| cell.has_contents()) {
                cell.contents()
            } else {
                " "
            };
            let style = TerminalStyle {
                foreground: cell.map_or(vt100::Color::Default, vt100::Cell::fgcolor),
                background: cell.map_or(vt100::Color::Default, vt100::Cell::bgcolor),
                bold: cell.is_some_and(vt100::Cell::bold),
                dim: cell.is_some_and(vt100::Cell::dim),
                italic: cell.is_some_and(vt100::Cell::italic),
                underline: cell.is_some_and(vt100::Cell::underline),
                inverse: cell.is_some_and(vt100::Cell::inverse),
                selected: terminal.is_selected(row, column),
                cursor: cursor && focused,
            };
            push_run(&mut runs, text, style);
        }
        if row < last_row {
            push_run(&mut runs, "\n", TerminalStyle::default());
        }
    }
    runs
}

fn push_run(runs: &mut Vec<DisplayRun>, text: &str, style: TerminalStyle) {
    if let Some(last) = runs.last_mut()
        && last.style == style
    {
        last.text.push_str(text);
        return;
    }
    runs.push(DisplayRun {
        text: text.to_owned(),
        style,
    });
}

/// Produces a plain-text view of the maintained terminal screen and adds a
/// visible cursor marker. Provider escape sequences stay inside the VT parser.
#[cfg(test)]
fn display_contents(terminal: &TerminalState, focused: bool) -> String {
    display_runs(terminal, focused)
        .into_iter()
        .map(|run| run.text)
        .collect()
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

    #[test]
    fn ansi_styles_survive_terminal_rendering() {
        let mut terminal = TerminalState::new(AttachmentRole::Controller, 3, 20, 100);
        terminal.parser.process(b"\x1b[1;31merror\x1b[0m plain");

        let runs = display_runs(&terminal, true);

        assert!(runs.iter().any(|run| {
            run.text == "error" && run.style.bold && run.style.foreground == vt100::Color::Idx(1)
        }));
        assert!(runs.iter().any(|run| run.text.contains("plain")));
    }

    #[test]
    fn selected_terminal_text_can_be_copied() {
        let mut terminal = TerminalState::new(AttachmentRole::Controller, 3, 20, 100);
        terminal.parser.process(b"alpha beta");
        terminal.begin_selection(0, 0);
        terminal.update_selection(0, 4);
        terminal.finish_selection();

        assert_eq!(terminal.selected_text().as_deref(), Some("alpha"));
    }

    #[test]
    fn paste_honors_bracketed_mode_and_protocol_limit() {
        let pasted = encode_paste("one\r\ntwo", true);

        assert!(pasted.starts_with(b"\x1b[200~"));
        assert!(pasted.ends_with(b"\x1b[201~"));
        assert!(pasted.windows(7).any(|window| window == b"one\ntwo"));
        assert!(pasted.len() <= sylvops_core::protocol::MAX_PTY_CHUNK_SIZE);
    }
}
