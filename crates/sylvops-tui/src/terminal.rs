use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use sylvops_core::{domain::AttachmentRole, ids::SessionId};

pub(crate) struct AttachedTerminal {
    pub session_id: SessionId,
    pub role: AttachmentRole,
    pub parser: vt100::Parser,
    pub last_sequence: u64,
    pub columns: u16,
    pub rows: u16,
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
