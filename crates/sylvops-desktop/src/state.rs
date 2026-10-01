use sylvops_core::ui::{DesktopPanel, DesktopState};

pub(crate) fn reset_layout(state: &mut DesktopState) {
    let defaults = DesktopState::default();
    state.panel_ratios = defaults.panel_ratios;
    state.window_width = defaults.window_width;
    state.window_height = defaults.window_height;
    state.compact_panel = DesktopPanel::Projects;
}

pub(crate) fn panel_ratios_fit(width: u16, ratios: [u16; 3]) -> bool {
    let width = u32::from(width);
    let explorer = width.saturating_mul(u32::from(ratios[0])) / 1000;
    explorer >= 180 && width.saturating_sub(explorer) >= 680
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(panel_ratios_fit(1180, [350, 350, 350]));
        assert!(!panel_ratios_fit(1180, [100, 350, 350]));
    }
}
