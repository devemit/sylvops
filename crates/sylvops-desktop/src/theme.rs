use iced::{
    Color, Theme,
    theme::{Palette, palette},
};

use crate::presentation::{PresentationTheme, Rgb};

pub(crate) fn resolve(presentation: PresentationTheme) -> Theme {
    let tokens = presentation.tokens;
    Theme::custom_with_fn(
        format!("SylvOps {}", presentation.label),
        Palette {
            background: color(tokens.canvas),
            text: color(tokens.text),
            primary: color(tokens.interaction),
            success: color(tokens.success),
            warning: color(tokens.attention),
            danger: color(tokens.danger),
        },
        move |_palette| palette::Extended {
            background: palette::Background {
                base: pair(tokens.canvas, tokens.text),
                weakest: pair(tokens.surface, tokens.text),
                weaker: pair(tokens.surface_raised, tokens.text),
                weak: pair(tokens.surface_sunken, tokens.text),
                neutral: pair(tokens.surface, tokens.text),
                strong: pair(tokens.border, tokens.text),
                stronger: pair(tokens.border_strong, tokens.text),
                strongest: pair(tokens.surface_raised, tokens.text),
            },
            primary: palette::Primary {
                base: pair(tokens.interaction, tokens.interaction_text),
                weak: pair(tokens.selection, tokens.selection_text),
                strong: pair(tokens.focus, tokens.interaction_text),
            },
            secondary: palette::Secondary {
                base: pair(tokens.surface, tokens.text_muted),
                weak: pair(tokens.surface_sunken, tokens.text),
                strong: pair(tokens.surface_raised, tokens.text),
            },
            success: palette::Success {
                base: pair(tokens.success, tokens.success_surface),
                weak: pair(tokens.success_surface, tokens.success),
                strong: pair(tokens.success, tokens.success_surface),
            },
            warning: palette::Warning {
                base: pair(tokens.attention, tokens.attention_surface),
                weak: pair(tokens.attention_surface, tokens.attention),
                strong: pair(tokens.attention, tokens.attention_surface),
            },
            danger: palette::Danger {
                base: pair(tokens.danger, tokens.danger_surface),
                weak: pair(tokens.danger_surface, tokens.danger),
                strong: pair(tokens.danger, tokens.danger_surface),
            },
            is_dark: palette::is_dark(color(tokens.canvas)),
        },
    )
}

const fn pair(background: Rgb, foreground: Rgb) -> palette::Pair {
    palette::Pair {
        color: color(background),
        text: color(foreground),
    }
}

pub(crate) const fn color(rgb: Rgb) -> Color {
    Color::from_rgb8(rgb.red, rgb.green, rgb.blue)
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub(crate) fn rgb(color: Color) -> Rgb {
    let channel = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
    Rgb {
        red: channel(color.r),
        green: channel(color.g),
        blue: channel(color.b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presentation::SystemAppearance;
    use sylvops_core::ui::DesktopTheme;

    #[test]
    fn iced_adapter_preserves_every_authored_semantic_role() {
        for choice in DesktopTheme::ALL {
            let presentation = PresentationTheme::resolve(choice, SystemAppearance::Dark);
            let tokens = presentation.tokens;
            let theme = resolve(presentation);
            let palette = theme.extended_palette();

            assert_eq!(palette.background.base.color, color(tokens.canvas));
            assert_eq!(palette.background.weakest.color, color(tokens.surface));
            assert_eq!(
                palette.background.weaker.color,
                color(tokens.surface_raised)
            );
            assert_eq!(palette.background.weak.color, color(tokens.surface_sunken));
            assert_eq!(palette.background.strong.color, color(tokens.border));
            assert_eq!(
                palette.background.stronger.color,
                color(tokens.border_strong)
            );
            assert_eq!(palette.secondary.base.text, color(tokens.text_muted));
            assert_eq!(palette.primary.base.color, color(tokens.interaction));
            assert_eq!(palette.primary.strong.color, color(tokens.focus));
            assert_eq!(palette.primary.weak.color, color(tokens.selection));
            assert_eq!(palette.primary.weak.text, color(tokens.selection_text));
            assert_eq!(palette.success.base.color, color(tokens.success));
            assert_eq!(palette.success.weak.color, color(tokens.success_surface));
            assert_eq!(palette.warning.base.color, color(tokens.attention));
            assert_eq!(palette.warning.weak.color, color(tokens.attention_surface));
            assert_eq!(palette.danger.base.color, color(tokens.danger));
            assert_eq!(palette.danger.weak.color, color(tokens.danger_surface));
        }
    }
}
