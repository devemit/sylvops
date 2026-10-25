use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use sylvops_core::{domain::AttachmentRole, ids::SessionId};

pub(crate) struct AttachedTerminal {
    pub session_id: SessionId,
    pub role: AttachmentRole,
    pub parser: vt100::Parser,
    pub last_sequence: u64,
    pub terminal_revision: u64,
    pub columns: u16,
    pub rows: u16,
    pub scrollback_capacity_bytes: usize,
}

impl AttachedTerminal {
    pub fn apply_snapshot(
        &mut self,
        revision: u64,
        sequence: u64,
        rows: u16,
        columns: u16,
        snapshot: &[u8],
    ) {
        if revision < self.terminal_revision || sequence < self.last_sequence {
            return;
        }
        self.parser.screen_mut().set_scrollback(0);
        self.parser
            .screen_mut()
            .set_size_reflow(rows, columns, self.scrollback_capacity_bytes);
        self.parser
            .process_snapshot(snapshot, sequence == self.last_sequence);
        self.last_sequence = sequence;
        self.terminal_revision = revision;
        self.rows = rows;
        self.columns = columns;
    }

    pub fn scroll(&mut self, delta: i32) {
        let screen = self.parser.screen_mut();
        if screen.alternate_screen() {
            return;
        }
        let offset = screen.scrollback();
        screen.set_scrollback(if delta >= 0 {
            offset.saturating_add(usize::try_from(delta).unwrap_or(0))
        } else {
            offset.saturating_sub(usize::try_from(delta.unsigned_abs()).unwrap_or(0))
        });
    }
}

pub(crate) fn draw_screen(
    frame: &mut ratatui::Frame<'_>,
    screen: &vt100::Screen,
    area: ratatui::layout::Rect,
) {
    use ratatui::style::{Color, Modifier, Style};
    let color = |value| match value {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(index) => Color::Indexed(index),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    };
    let (rows, columns) = screen.size();
    let buffer = frame.buffer_mut();
    for row in 0..area.height.min(rows) {
        for col in 0..area.width.min(columns) {
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };
            if cell.is_wide_continuation() {
                continue;
            }
            // A wide character at the viewport edge cannot occupy half a cell.
            if cell.is_wide() && col + 1 >= area.width {
                continue;
            }
            let mut style = Style::default()
                .fg(color(cell.fgcolor()))
                .bg(color(cell.bgcolor()));
            for (enabled, modifier) in [
                (cell.bold(), Modifier::BOLD),
                (cell.dim(), Modifier::DIM),
                (cell.italic(), Modifier::ITALIC),
                (cell.underline(), Modifier::UNDERLINED),
                (cell.inverse(), Modifier::REVERSED),
            ] {
                if enabled {
                    style = style.add_modifier(modifier);
                }
            }
            let symbol = if cell.has_contents() {
                cell.contents()
            } else {
                " "
            };
            buffer[(area.x + col, area.y + row)]
                .set_symbol(symbol)
                .set_style(style);
        }
    }
    let (row, col) = screen.cursor_position();
    if !screen.hide_cursor() && screen.scrollback() == 0 && row < area.height && col < area.width {
        frame.set_cursor_position((area.x + col, area.y + row));
    }
}

pub(crate) fn is_detach_key(key: KeyEvent) -> bool {
    key.code == KeyCode::Char(']') && key.modifiers.contains(KeyModifiers::CONTROL)
}

pub(crate) fn encode_key(key: KeyEvent) -> Option<Vec<u8>> {
    let bytes = match key.code {
        KeyCode::Char(character)
            if key.modifiers.contains(KeyModifiers::CONTROL) && character.is_ascii() =>
        {
            let byte = u8::try_from(u32::from(character.to_ascii_uppercase())).ok()?;
            vec![byte & 0x1f]
        }
        KeyCode::Char(character) => character.to_string().into_bytes(),
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        _ => return None,
    };
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cell_rendering_preserves_styles_and_physical_rows() {
        let mut parser = vt100::Parser::new(3, 6, 0);
        parser.process("\x1b[1;31mAB界\x1b[0m\r\nnext".as_bytes());
        let backend = ratatui::backend::TestBackend::new(6, 3);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_screen(frame, parser.screen(), frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].symbol(), "A");
        assert_eq!(buffer[(0, 0)].fg, ratatui::style::Color::Indexed(1));
        assert!(
            buffer[(0, 0)]
                .modifier
                .contains(ratatui::style::Modifier::BOLD)
        );
        assert_eq!(buffer[(2, 0)].symbol(), "界");
        assert_eq!(buffer[(0, 1)].symbol(), "n");
    }

    #[test]
    fn bounded_history_scroll_is_local_and_disabled_in_alternate_screen() {
        let mut attached = AttachedTerminal {
            session_id: SessionId::new(),
            role: sylvops_core::domain::AttachmentRole::Observer,
            parser: vt100::Parser::new(2, 20, 10),
            last_sequence: 0,
            terminal_revision: 0,
            columns: 20,
            rows: 2,
            scrollback_capacity_bytes: 64 * 1024,
        };
        attached.parser.process(b"one\r\ntwo\r\nthree\r\nfour");
        attached.scroll(3);
        assert!(attached.parser.screen().scrollback() > 0);
        assert!(attached.parser.screen().contents().contains("one"));
        attached.scroll(-100);
        assert_eq!(attached.parser.screen().scrollback(), 0);
        attached.parser.process(b"\x1b[?1049h");
        attached.scroll(100);
        assert_eq!(attached.parser.screen().scrollback(), 0);
    }

    #[test]
    fn recovered_snapshot_rejects_older_resize_with_the_same_output_sequence() {
        let mut attached = AttachedTerminal {
            session_id: SessionId::new(),
            role: sylvops_core::domain::AttachmentRole::Controller,
            parser: vt100::Parser::new(8, 80, 0),
            last_sequence: 5,
            terminal_revision: 0,
            columns: 80,
            rows: 8,
            scrollback_capacity_bytes: 64 * 1024,
        };
        let mut latest = vt100::Parser::new(8, 44, 0);
        latest.process(b"latest snapshot MARKER");
        attached.apply_snapshot(3, 5, 8, 44, &latest.screen().state_formatted());
        let mut stale = vt100::Parser::new(8, 20, 0);
        stale.process(b"older snapshot");
        attached.apply_snapshot(2, 5, 8, 20, &stale.screen().state_formatted());
        assert_eq!((attached.columns, attached.rows), (44, 8));
        assert_eq!(
            attached.parser.screen().contents(),
            "latest snapshot MARKER"
        );
    }

    #[test]
    fn detach_is_reserved_but_escape_is_forwarded() {
        assert!(is_detach_key(KeyEvent::new(
            KeyCode::Char(']'),
            KeyModifiers::CONTROL
        )));
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Some(vec![0x1b])
        );
    }
}
