use iced::keyboard::{self, Key, key::Named};
use sylvops_core::domain::AttachmentRole;

pub(crate) struct TerminalState {
    pub role: AttachmentRole,
    pub parser: vt100::Parser,
    pub last_sequence: u64,
    pub attached: bool,
    pub columns: u16,
    pub rows: u16,
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
    let show_cursor =
        terminal.attached && terminal.role == AttachmentRole::Controller && !screen.hide_cursor();
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
        let mut terminal = TerminalState {
            role: AttachmentRole::Controller,
            parser: vt100::Parser::new(4, 12, 100),
            last_sequence: 0,
            attached: true,
            columns: 12,
            rows: 4,
        };
        terminal.parser.process(b"ready");

        assert_eq!(display_contents(&terminal, true), "ready█");
        assert_eq!(display_contents(&terminal, false), "ready▯");
    }
}
