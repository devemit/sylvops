use ratatui::style::{Color, Modifier, Style};
use sylvops_core::ui::TuiTheme;

use crate::app::FlashKind;

#[derive(Clone, Copy, Debug)]
pub(crate) struct Theme {
    pub background: Style,
    pub text: Style,
    pub border: Style,
    pub focus: Style,
    pub selected: Style,
    pub success: Style,
    pub attention: Style,
    pub danger: Style,
    pub muted: Style,
    pub diff_file_header: Style,
    pub diff_hunk_header: Style,
    pub diff_addition: Style,
    pub diff_deletion: Style,
}

#[derive(Clone, Copy)]
struct Palette {
    background: Color,
    text: Color,
    border: Color,
    focus: Color,
    selection: Color,
    success: Color,
    attention: Color,
    danger: Color,
    muted: Color,
}

impl Theme {
    pub fn resolve(choice: TuiTheme) -> Self {
        let palette = match choice {
            TuiTheme::TerminalDefault => {
                return Self {
                    background: Style::default(),
                    text: Style::default(),
                    border: Style::default(),
                    focus: Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                    selected: Style::default().add_modifier(Modifier::REVERSED),
                    success: Style::default().fg(Color::Green),
                    attention: Style::default().fg(Color::Yellow),
                    danger: Style::default().fg(Color::Red),
                    muted: Style::default().fg(Color::DarkGray),
                    diff_file_header: Style::default().add_modifier(Modifier::BOLD),
                    diff_hunk_header: Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                    diff_addition: Style::default().fg(Color::Green),
                    diff_deletion: Style::default().fg(Color::Red),
                };
            }
            TuiTheme::Nord => Palette {
                background: rgb(46, 52, 64),
                text: rgb(216, 222, 233),
                border: rgb(76, 86, 106),
                focus: rgb(136, 192, 208),
                selection: rgb(67, 76, 94),
                success: rgb(163, 190, 140),
                attention: rgb(235, 203, 139),
                danger: rgb(191, 97, 106),
                muted: rgb(129, 161, 193),
            },
            TuiTheme::TokyoNight => Palette {
                background: rgb(26, 27, 38),
                text: rgb(192, 202, 245),
                border: rgb(65, 72, 104),
                focus: rgb(122, 162, 247),
                selection: rgb(41, 46, 66),
                success: rgb(158, 206, 106),
                attention: rgb(224, 175, 104),
                danger: rgb(247, 118, 142),
                muted: rgb(86, 95, 137),
            },
            TuiTheme::Catppuccin => Palette {
                background: rgb(30, 30, 46),
                text: rgb(205, 214, 244),
                border: rgb(88, 91, 112),
                focus: rgb(137, 180, 250),
                selection: rgb(49, 50, 68),
                success: rgb(166, 227, 161),
                attention: rgb(249, 226, 175),
                danger: rgb(243, 139, 168),
                muted: rgb(147, 153, 178),
            },
            TuiTheme::GruvboxDark => Palette {
                background: rgb(40, 40, 40),
                text: rgb(235, 219, 178),
                border: rgb(102, 92, 84),
                focus: rgb(131, 165, 152),
                selection: rgb(60, 56, 54),
                success: rgb(184, 187, 38),
                attention: rgb(250, 189, 47),
                danger: rgb(251, 73, 52),
                muted: rgb(168, 153, 132),
            },
            TuiTheme::SolarizedLight => Palette {
                background: rgb(253, 246, 227),
                text: rgb(88, 110, 117),
                border: rgb(147, 161, 161),
                focus: rgb(38, 139, 210),
                selection: rgb(238, 232, 213),
                success: rgb(133, 153, 0),
                attention: rgb(181, 137, 0),
                danger: rgb(220, 50, 47),
                muted: rgb(101, 123, 131),
            },
        };
        Self::from_palette(palette)
    }

    fn from_palette(palette: Palette) -> Self {
        Self {
            background: Style::default().fg(palette.text).bg(palette.background),
            text: Style::default().fg(palette.text).bg(palette.background),
            border: Style::default().fg(palette.border).bg(palette.background),
            focus: Style::default()
                .fg(palette.focus)
                .bg(palette.background)
                .add_modifier(Modifier::BOLD),
            selected: Style::default()
                .fg(palette.text)
                .bg(palette.selection)
                .add_modifier(Modifier::BOLD),
            success: Style::default().fg(palette.success).bg(palette.background),
            attention: Style::default()
                .fg(palette.attention)
                .bg(palette.background),
            danger: Style::default().fg(palette.danger).bg(palette.background),
            muted: Style::default().fg(palette.muted).bg(palette.background),
            diff_file_header: Style::default()
                .fg(palette.text)
                .bg(palette.selection)
                .add_modifier(Modifier::BOLD),
            diff_hunk_header: Style::default()
                .fg(palette.focus)
                .bg(blend(palette.background, palette.focus, 14))
                .add_modifier(Modifier::BOLD),
            diff_addition: Style::default().fg(palette.success).bg(blend(
                palette.background,
                palette.success,
                14,
            )),
            diff_deletion: Style::default().fg(palette.danger).bg(blend(
                palette.background,
                palette.danger,
                14,
            )),
        }
    }

    pub fn flash(self, kind: FlashKind) -> Style {
        match kind {
            FlashKind::Info => self.focus,
            FlashKind::Success => self.success,
            FlashKind::Error => self.danger,
        }
    }
}

const fn rgb(red: u8, green: u8, blue: u8) -> Color {
    Color::Rgb(red, green, blue)
}

fn blend(base: Color, accent: Color, accent_percent: u16) -> Color {
    let (Color::Rgb(base_red, base_green, base_blue), Color::Rgb(red, green, blue)) =
        (base, accent)
    else {
        return accent;
    };
    let base_percent = 100_u16.saturating_sub(accent_percent);
    let channel = |base: u8, accent: u8| {
        u8::try_from((u16::from(base) * base_percent + u16::from(accent) * accent_percent) / 100)
            .unwrap_or(u8::MAX)
    };
    rgb(
        channel(base_red, red),
        channel(base_green, green),
        channel(base_blue, blue),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_theme_distinguishes_added_and_deleted_diff_lines() {
        for choice in TuiTheme::ALL {
            let theme = Theme::resolve(choice);
            assert_ne!(theme.diff_addition, theme.diff_deletion, "{choice}");
            assert_ne!(theme.diff_hunk_header, theme.text, "{choice}");
            assert_ne!(theme.diff_file_header, theme.muted, "{choice}");
        }
    }

    #[test]
    fn named_themes_tint_added_and_deleted_row_backgrounds() {
        for choice in TuiTheme::ALL
            .into_iter()
            .filter(|choice| *choice != TuiTheme::TerminalDefault)
        {
            let theme = Theme::resolve(choice);
            assert_ne!(theme.diff_addition.bg, theme.background.bg, "{choice}");
            assert_ne!(theme.diff_deletion.bg, theme.background.bg, "{choice}");
            assert_ne!(theme.diff_addition.bg, theme.diff_deletion.bg, "{choice}");
        }
    }
}
