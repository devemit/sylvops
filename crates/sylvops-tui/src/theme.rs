use ratatui::style::{Color, Modifier, Style};

use crate::app::FlashKind;

#[derive(Clone, Copy, Debug)]
pub(crate) struct Theme {
    pub focus: Style,
    pub selected: Style,
    pub success: Style,
    pub warning: Style,
    pub failure: Style,
    pub muted: Style,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            focus: Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            selected: Style::default().add_modifier(Modifier::REVERSED),
            success: Style::default().fg(Color::Green),
            warning: Style::default().fg(Color::Yellow),
            failure: Style::default().fg(Color::Red),
            muted: Style::default().fg(Color::DarkGray),
        }
    }
}

impl Theme {
    pub fn flash(self, kind: FlashKind) -> Style {
        match kind {
            FlashKind::Info => self.focus,
            FlashKind::Success => self.success,
            FlashKind::Error => self.failure,
        }
    }
}
