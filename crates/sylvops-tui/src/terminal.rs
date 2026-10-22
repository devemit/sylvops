use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use sylvops_core::{domain::AttachmentRole, ids::SessionId};

pub(crate) const TERMINAL_SCROLLBACK_ROWS: usize = 10_000;
const WHEEL_SCROLL_LINES: isize = 3;

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum TerminalWheelAction {
    Local,
    Application(Vec<u8>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TerminalHistoryAction {
    PageUp,
    PageDown,
    Oldest,
    Latest,
}

pub(crate) struct AttachedTerminal {
    pub session_id: SessionId,
    pub role: AttachmentRole,
    pub parser: vt100::Parser,
    pub last_sequence: u64,
    pub columns: u16,
    pub rows: u16,
}

impl AttachedTerminal {
    pub fn scroll_lines(&mut self, lines: isize) {
        let current = self.parser.screen().scrollback();
        let next = current.saturating_add_signed(lines);
        self.parser.screen_mut().set_scrollback(next);
    }

    pub fn scroll_to_latest(&mut self) {
        self.parser.screen_mut().set_scrollback(0);
    }

    pub fn scroll_page(&mut self, pages: isize) {
        let page_rows = usize::from(self.rows.saturating_sub(1).max(1));
        let lines =
            isize::try_from(page_rows.saturating_mul(pages.unsigned_abs())).unwrap_or(isize::MAX);
        self.scroll_lines(if pages.is_positive() { lines } else { -lines });
    }

    pub fn scroll_to_oldest(&mut self) {
        self.parser.screen_mut().set_scrollback(usize::MAX);
    }

    pub fn is_scrolled_back(&self) -> bool {
        self.parser.screen().scrollback() > 0
    }

    pub fn apply_history_action(&mut self, action: TerminalHistoryAction) -> TerminalWheelAction {
        let application_page = self.parser.screen().alternate_screen()
            || (!self.is_scrolled_back()
                && self.parser.screen().mouse_protocol_mode() != vt100::MouseProtocolMode::None);
        if application_page && self.role == AttachmentRole::Controller {
            match action {
                TerminalHistoryAction::PageUp => {
                    return TerminalWheelAction::Application(b"\x1b[5~".to_vec());
                }
                TerminalHistoryAction::PageDown => {
                    return TerminalWheelAction::Application(b"\x1b[6~".to_vec());
                }
                _ => {}
            }
        }
        match action {
            TerminalHistoryAction::PageUp => self.scroll_page(1),
            TerminalHistoryAction::PageDown => self.scroll_page(-1),
            TerminalHistoryAction::Oldest => self.scroll_to_oldest(),
            TerminalHistoryAction::Latest => self.scroll_to_latest(),
        }
        TerminalWheelAction::Local
    }

    pub fn wheel_action(&mut self, scroll_up: bool, row: u16, column: u16) -> TerminalWheelAction {
        let (alternate_screen, mouse_mode, mouse_encoding) = {
            let screen = self.parser.screen();
            (
                screen.alternate_screen(),
                screen.mouse_protocol_mode(),
                screen.mouse_protocol_encoding(),
            )
        };
        if self.role == AttachmentRole::Controller
            && !self.is_scrolled_back()
            && mouse_mode != vt100::MouseProtocolMode::None
        {
            let row = row.min(self.rows.saturating_sub(1));
            let column = column.min(self.columns.saturating_sub(1));
            return TerminalWheelAction::Application(encode_mouse_wheel_event(
                mouse_encoding,
                scroll_up,
                row,
                column,
            ));
        }
        if alternate_screen && self.role == AttachmentRole::Controller {
            return self.apply_history_action(if scroll_up {
                TerminalHistoryAction::PageUp
            } else {
                TerminalHistoryAction::PageDown
            });
        }
        if !alternate_screen {
            self.scroll_lines(if scroll_up {
                WHEEL_SCROLL_LINES
            } else {
                -WHEEL_SCROLL_LINES
            });
        }
        TerminalWheelAction::Local
    }
}

pub(crate) fn history_action(key: KeyEvent) -> Option<TerminalHistoryAction> {
    if !key.modifiers.contains(KeyModifiers::SHIFT) {
        return None;
    }
    match key.code {
        KeyCode::PageUp => Some(TerminalHistoryAction::PageUp),
        KeyCode::PageDown => Some(TerminalHistoryAction::PageDown),
        KeyCode::Home => Some(TerminalHistoryAction::Oldest),
        KeyCode::End => Some(TerminalHistoryAction::Latest),
        _ => None,
    }
}

fn encode_mouse_wheel_event(
    encoding: vt100::MouseProtocolEncoding,
    scroll_up: bool,
    row: u16,
    column: u16,
) -> Vec<u8> {
    let button = if scroll_up { 64_u16 } else { 65_u16 };
    let x = column.saturating_add(1);
    let y = row.saturating_add(1);
    match encoding {
        vt100::MouseProtocolEncoding::Sgr => format!("\x1b[<{button};{x};{y}M").into_bytes(),
        vt100::MouseProtocolEncoding::Utf8 => {
            let mut bytes = b"\x1b[M".to_vec();
            for value in [
                button.saturating_add(32),
                x.saturating_add(32),
                y.saturating_add(32),
            ] {
                let character = char::from_u32(u32::from(value)).unwrap_or('\u{fffd}');
                let mut encoded = [0_u8; 4];
                bytes.extend_from_slice(character.encode_utf8(&mut encoded).as_bytes());
            }
            bytes
        }
        vt100::MouseProtocolEncoding::Default => vec![
            0x1b,
            b'[',
            b'M',
            u8::try_from(button.saturating_add(32)).unwrap_or(u8::MAX),
            u8::try_from(x.min(223).saturating_add(32)).unwrap_or(u8::MAX),
            u8::try_from(y.min(223).saturating_add(32)).unwrap_or(u8::MAX),
        ],
    }
}

pub(crate) fn is_detach_key(key: KeyEvent) -> bool {
    key.code == KeyCode::Char(']') && key.modifiers.contains(KeyModifiers::CONTROL)
}

pub(crate) fn encode_paste(contents: &str, bracketed: bool) -> Vec<u8> {
    if !bracketed {
        return contents.as_bytes().to_vec();
    }
    let normalized = contents.replace("\r\n", "\n").replace('\r', "\n");
    let mut bytes = b"\x1b[200~".to_vec();
    bytes.extend_from_slice(normalized.as_bytes());
    bytes.extend_from_slice(b"\x1b[201~");
    bytes
}

pub(crate) fn encode_key(key: KeyEvent) -> Option<Vec<u8>> {
    // Pending-question navigation uses modified arrows; plain arrows are different actions.
    let modifier = 1
        + u8::from(key.modifiers.contains(KeyModifiers::SHIFT))
        + 2 * u8::from(key.modifiers.contains(KeyModifiers::ALT))
        + 4 * u8::from(key.modifiers.contains(KeyModifiers::CONTROL));
    let cursor_key = |suffix| {
        if modifier == 1 {
            format!("\x1b[{suffix}").into_bytes()
        } else {
            format!("\x1b[1;{modifier}{suffix}").into_bytes()
        }
    };
    let bytes = match key.code {
        KeyCode::Char(character)
            if key.modifiers.contains(KeyModifiers::CONTROL) && character.is_ascii() =>
        {
            let byte = u8::try_from(u32::from(character.to_ascii_uppercase())).ok()?;
            vec![byte & 0x1f]
        }
        KeyCode::Char(character) => character.to_string().into_bytes(),
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => vec![b'\n'],
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => cursor_key('A'),
        KeyCode::Down => cursor_key('B'),
        KeyCode::Right => cursor_key('C'),
        KeyCode::Left => cursor_key('D'),
        KeyCode::Home => cursor_key('H'),
        KeyCode::End => cursor_key('F'),
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
    fn draft_newline_is_distinct_from_submit_and_queue() {
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
            Some(vec![b'\n'])
        );
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(vec![b'\r'])
        );
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            Some(vec![b'\t'])
        );
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Some(vec![0x1b])
        );
    }

    #[test]
    fn draft_paste_keeps_newlines_inside_provider_paste_boundaries() {
        assert_eq!(
            encode_paste("one\r\ntwo\n", true),
            b"\x1b[200~one\ntwo\n\x1b[201~"
        );
        assert_eq!(encode_paste("one\rtwo", false), b"one\rtwo");
    }

    fn attached_terminal(role: AttachmentRole, rows: u16, columns: u16) -> AttachedTerminal {
        AttachedTerminal {
            session_id: SessionId::new(),
            role,
            parser: vt100::Parser::new(rows, columns, TERMINAL_SCROLLBACK_ROWS),
            last_sequence: 0,
            columns,
            rows,
        }
    }

    #[test]
    fn question_navigation_preserves_modified_arrow_keys() {
        for (code, modifiers, expected) in [
            (KeyCode::Up, KeyModifiers::SHIFT, "\x1b[1;2A"),
            (KeyCode::Down, KeyModifiers::SHIFT, "\x1b[1;2B"),
            (KeyCode::Down, KeyModifiers::ALT, "\x1b[1;3B"),
            (KeyCode::Up, KeyModifiers::CONTROL, "\x1b[1;5A"),
            (
                KeyCode::Up,
                KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::CONTROL,
                "\x1b[1;8A",
            ),
            (KeyCode::Up, KeyModifiers::NONE, "\x1b[A"),
        ] {
            assert_eq!(
                encode_key(KeyEvent::new(code, modifiers)),
                Some(expected.as_bytes().to_vec()),
                "{code:?} {modifiers:?}"
            );
        }
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

    #[test]
    fn attached_terminal_scrolls_older_output_and_returns_to_latest() {
        let mut terminal = attached_terminal(AttachmentRole::Controller, 3, 12);
        terminal
            .parser
            .process(b"one\r\ntwo\r\nthree\r\nfour\r\nfive");
        assert!(terminal.parser.screen().contents().contains("five"));

        terminal.scroll_lines(2);
        assert!(terminal.is_scrolled_back());
        assert!(terminal.parser.screen().contents().contains("two"));
        let history_view = terminal.parser.screen().contents();

        terminal.parser.process(b"\r\nsix");
        assert_eq!(terminal.parser.screen().contents(), history_view);

        terminal.scroll_to_latest();
        assert!(!terminal.is_scrolled_back());
        assert!(terminal.parser.screen().contents().contains("six"));
    }

    #[test]
    fn normal_screen_wheel_scrolls_locally() {
        let mut terminal = attached_terminal(AttachmentRole::Controller, 3, 12);
        terminal
            .parser
            .process(b"one\r\ntwo\r\nthree\r\nfour\r\nfive");

        assert_eq!(
            terminal.wheel_action(true, 1, 1),
            TerminalWheelAction::Local
        );
        assert!(terminal.is_scrolled_back());
    }

    #[test]
    fn normal_screen_application_requesting_mouse_receives_wheel_input() {
        let mut terminal = attached_terminal(AttachmentRole::Controller, 20, 80);
        terminal
            .parser
            .process(b"\x1b[?1000h\x1b[?1006hconversation");
        assert_eq!(
            terminal.wheel_action(true, 2, 3),
            TerminalWheelAction::Application(b"\x1b[<64;4;3M".to_vec())
        );
    }

    #[test]
    fn full_screen_history_keys_reach_the_application_instead_of_empty_local_history() {
        let mut terminal = attached_terminal(AttachmentRole::Controller, 20, 80);
        terminal.parser.process(b"\x1b[?1049hconversation");
        assert_eq!(
            terminal.apply_history_action(TerminalHistoryAction::PageUp),
            TerminalWheelAction::Application(b"\x1b[5~".to_vec())
        );
        assert_eq!(
            terminal.apply_history_action(TerminalHistoryAction::PageDown),
            TerminalWheelAction::Application(b"\x1b[6~".to_vec())
        );
    }

    #[test]
    fn alternate_screen_wheel_uses_requested_mouse_encoding() {
        let mut terminal = attached_terminal(AttachmentRole::Controller, 20, 80);
        terminal
            .parser
            .process(b"\x1b[?1049h\x1b[?1000h\x1b[?1006h");

        assert_eq!(
            terminal.wheel_action(true, 2, 3),
            TerminalWheelAction::Application(b"\x1b[<64;4;3M".to_vec())
        );
        assert_eq!(
            terminal.wheel_action(false, 2, 3),
            TerminalWheelAction::Application(b"\x1b[<65;4;3M".to_vec())
        );

        terminal.parser.process(b"\x1b[?1006l\x1b[?1005h");
        assert_eq!(
            terminal.wheel_action(false, 2, 3),
            TerminalWheelAction::Application(vec![0x1b, b'[', b'M', 97, 36, 35])
        );

        terminal.parser.process(b"\x1b[?1005l");
        assert_eq!(
            terminal.wheel_action(true, 2, 3),
            TerminalWheelAction::Application(vec![0x1b, b'[', b'M', 96, 36, 35])
        );
    }

    #[test]
    fn observer_never_forwards_wheel_input() {
        let mut terminal = attached_terminal(AttachmentRole::Observer, 20, 80);
        terminal
            .parser
            .process(b"\x1b[?1049h\x1b[?1000h\x1b[?1006h");

        assert_eq!(
            terminal.wheel_action(true, 2, 3),
            TerminalWheelAction::Local
        );
    }

    #[test]
    fn shifted_navigation_keys_control_local_history() {
        for (code, expected) in [
            (KeyCode::PageUp, Some(TerminalHistoryAction::PageUp)),
            (KeyCode::PageDown, Some(TerminalHistoryAction::PageDown)),
            (KeyCode::Home, Some(TerminalHistoryAction::Oldest)),
            (KeyCode::End, Some(TerminalHistoryAction::Latest)),
        ] {
            assert_eq!(
                history_action(KeyEvent::new(code, KeyModifiers::SHIFT)),
                expected
            );
            assert_eq!(
                history_action(KeyEvent::new(code, KeyModifiers::NONE)),
                None
            );
        }
    }
}
