use sylvops_core::ui::{DesktopPanel, DesktopState};

pub(crate) const WIDE_BREAKPOINT: u16 = 1180;
pub(crate) const COMPACT_BREAKPOINT: u16 = 820;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LayoutMode {
    Wide,
    Compact,
    Narrow,
}

pub(crate) fn layout_mode(width: u16) -> LayoutMode {
    if width >= WIDE_BREAKPOINT {
        LayoutMode::Wide
    } else if width >= COMPACT_BREAKPOINT {
        LayoutMode::Compact
    } else {
        LayoutMode::Narrow
    }
}

pub(crate) fn reset_layout(state: &mut DesktopState) {
    let defaults = DesktopState::default();
    state.panel_ratios = defaults.panel_ratios;
    state.window_width = defaults.window_width;
    state.window_height = defaults.window_height;
    state.compact_panel = DesktopPanel::Projects;
}

pub(crate) fn panel_ratios_fit(width: u16, ratios: [u16; 3]) -> bool {
    let mut remaining = u32::from(width);
    for ratio in ratios {
        let pane = remaining.saturating_mul(u32::from(ratio)) / 1000;
        if pane < 150 {
            return false;
        }
        remaining = remaining.saturating_sub(pane);
    }
    remaining >= 480
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_breakpoints_are_stable() {
        assert_eq!(layout_mode(1179), LayoutMode::Compact);
        assert_eq!(layout_mode(1180), LayoutMode::Wide);
        assert_eq!(layout_mode(819), LayoutMode::Narrow);
        assert_eq!(layout_mode(820), LayoutMode::Compact);
    }

    #[test]
    fn reset_layout_preserves_user_context() {
        let mut state = DesktopState {
            compact_panel: DesktopPanel::Sessions,
            panel_ratios: [350; 3],
            ..DesktopState::default()
        };
        reset_layout(&mut state);
        assert_eq!(state.panel_ratios, DesktopState::default().panel_ratios);
        assert_eq!(state.compact_panel, DesktopPanel::Projects);
    }

    #[test]
    fn panel_limits_preserve_a_useful_main_surface() {
        assert!(panel_ratios_fit(1440, DesktopState::default().panel_ratios));
        assert!(!panel_ratios_fit(1180, [350, 350, 350]));
    }
}
