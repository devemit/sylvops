use iced::{Color, Theme, theme::Palette};
use sylvops_core::ui::DesktopTheme;

pub(crate) fn resolve(choice: DesktopTheme, system: iced::theme::Mode) -> Theme {
    match choice {
        DesktopTheme::System => match system {
            iced::theme::Mode::Light => light(),
            iced::theme::Mode::None | iced::theme::Mode::Dark => dark(),
        },
        DesktopTheme::Light => light(),
        DesktopTheme::Dark => dark(),
        DesktopTheme::Nord => Theme::Nord,
        DesktopTheme::TokyoNight => Theme::TokyoNight,
        DesktopTheme::Catppuccin => Theme::CatppuccinMocha,
        DesktopTheme::Dracula => Theme::Dracula,
        DesktopTheme::GruvboxDark => Theme::GruvboxDark,
        DesktopTheme::SolarizedLight => Theme::SolarizedLight,
        DesktopTheme::SolarizedDark => Theme::SolarizedDark,
    }
}

pub(crate) const fn label(theme: DesktopTheme) -> &'static str {
    match theme {
        DesktopTheme::System => "System",
        DesktopTheme::Light => "Light",
        DesktopTheme::Dark => "Dark",
        DesktopTheme::Nord => "Nord",
        DesktopTheme::TokyoNight => "Tokyo Night",
        DesktopTheme::Catppuccin => "Catppuccin",
        DesktopTheme::Dracula => "Dracula",
        DesktopTheme::GruvboxDark => "Gruvbox Dark",
        DesktopTheme::SolarizedLight => "Solarized Light",
        DesktopTheme::SolarizedDark => "Solarized Dark",
    }
}

fn dark() -> Theme {
    Theme::custom(
        "SylvOps Dark",
        Palette {
            background: Color::from_rgb8(21, 24, 30),
            text: Color::from_rgb8(224, 226, 231),
            primary: Color::from_rgb8(108, 142, 239),
            success: Color::from_rgb8(102, 187, 131),
            warning: Color::from_rgb8(220, 170, 92),
            danger: Color::from_rgb8(218, 103, 111),
        },
    )
}

fn light() -> Theme {
    Theme::custom(
        "SylvOps Light",
        Palette {
            background: Color::from_rgb8(247, 248, 250),
            text: Color::from_rgb8(31, 38, 49),
            primary: Color::from_rgb8(50, 98, 210),
            success: Color::from_rgb8(37, 135, 74),
            warning: Color::from_rgb8(177, 108, 18),
            danger: Color::from_rgb8(190, 57, 67),
        },
    )
}
