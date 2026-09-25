//! Bounded, non-sensitive state shared by replaceable user interfaces.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::ids::{ProjectId, SessionId, WorktreeId};

pub const MAX_OPEN_DESKTOP_SESSIONS: usize = 16;
pub const MIN_TERMINAL_FONT_SIZE: u8 = 10;
pub const MAX_TERMINAL_FONT_SIZE: u8 = 22;
pub const MIN_DESKTOP_WIDTH: u16 = 680;
pub const MIN_DESKTOP_HEIGHT: u16 = 480;
pub const MAX_DESKTOP_WIDTH: u16 = 7680;
pub const MAX_DESKTOP_HEIGHT: u16 = 4320;

/// Main content shown beside the explorer.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MainTab {
    #[default]
    Terminal,
    Changes,
    Details,
}

/// Navigation state safe to persist between TUI runs.
///
/// Deliberately excludes form values, searches, terminal data, and credentials.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct TuiState {
    pub selected_project_id: Option<ProjectId>,
    pub selected_worktree_id: Option<WorktreeId>,
    pub selected_session_id: Option<SessionId>,
    pub selected_main_tab: MainTab,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesktopTheme {
    #[default]
    System,
    Light,
    Dark,
    Nord,
    TokyoNight,
    Catppuccin,
    Dracula,
    GruvboxDark,
    SolarizedLight,
    SolarizedDark,
}

/// Terminal typeface preference. Named fonts intentionally fall back through
/// the renderer when they are not installed on the current machine.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesktopTerminalFont {
    #[default]
    System,
    JetBrainsMono,
    CascadiaCode,
    FiraCode,
}

/// Cursor shape rendered by the native desktop terminal.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesktopTerminalCursor {
    #[default]
    Block,
    Line,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesktopDensity {
    #[default]
    Comfortable,
    Compact,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesktopPanel {
    #[default]
    Projects,
    Worktrees,
    Sessions,
}

impl fmt::Display for DesktopTheme {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::System => "System",
            Self::Light => "Light",
            Self::Dark => "Dark",
            Self::Nord => "Nord",
            Self::TokyoNight => "Tokyo Night",
            Self::Catppuccin => "Catppuccin",
            Self::Dracula => "Dracula",
            Self::GruvboxDark => "Gruvbox Dark",
            Self::SolarizedLight => "Solarized Light",
            Self::SolarizedDark => "Solarized Dark",
        })
    }
}

impl fmt::Display for DesktopTerminalFont {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::System => "System mono",
            Self::JetBrainsMono => "JetBrains Mono",
            Self::CascadiaCode => "Cascadia Code",
            Self::FiraCode => "Fira Code",
        })
    }
}

impl fmt::Display for DesktopTerminalCursor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Block => "Block",
            Self::Line => "Line",
        })
    }
}

impl fmt::Display for DesktopDensity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Comfortable => "Comfortable",
            Self::Compact => "Compact",
        })
    }
}

/// Bounded, non-sensitive state restored by the native desktop client.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DesktopState {
    pub selected_project_id: Option<ProjectId>,
    pub selected_worktree_id: Option<WorktreeId>,
    pub selected_session_id: Option<SessionId>,
    pub selected_main_tab: MainTab,
    pub open_session_ids: Vec<SessionId>,
    pub theme: DesktopTheme,
    pub density: DesktopDensity,
    pub terminal_font: DesktopTerminalFont,
    pub terminal_cursor: DesktopTerminalCursor,
    pub terminal_font_size: u8,
    pub window_width: u16,
    pub window_height: u16,
    /// Navigator widths in thousandths of the usable desktop width.
    pub panel_ratios: [u16; 3],
    pub compact_panel: DesktopPanel,
}

impl Default for DesktopState {
    fn default() -> Self {
        Self {
            selected_project_id: None,
            selected_worktree_id: None,
            selected_session_id: None,
            selected_main_tab: MainTab::Terminal,
            open_session_ids: Vec::new(),
            theme: DesktopTheme::System,
            density: DesktopDensity::Comfortable,
            terminal_font: DesktopTerminalFont::System,
            terminal_cursor: DesktopTerminalCursor::Block,
            terminal_font_size: 13,
            window_width: 1440,
            window_height: 900,
            panel_ratios: [160, 190, 210],
            compact_panel: DesktopPanel::Projects,
        }
    }
}

impl DesktopState {
    /// Clamps untrusted persisted values and removes duplicate session tabs.
    #[must_use]
    pub fn normalized(mut self) -> Self {
        let mut unique =
            Vec::with_capacity(self.open_session_ids.len().min(MAX_OPEN_DESKTOP_SESSIONS));
        for id in self.open_session_ids {
            if !unique.contains(&id) {
                unique.push(id);
                if unique.len() == MAX_OPEN_DESKTOP_SESSIONS {
                    break;
                }
            }
        }
        self.open_session_ids = unique;
        self.terminal_font_size = self
            .terminal_font_size
            .clamp(MIN_TERMINAL_FONT_SIZE, MAX_TERMINAL_FONT_SIZE);
        self.window_width = self
            .window_width
            .clamp(MIN_DESKTOP_WIDTH, MAX_DESKTOP_WIDTH);
        self.window_height = self
            .window_height
            .clamp(MIN_DESKTOP_HEIGHT, MAX_DESKTOP_HEIGHT);
        for ratio in &mut self.panel_ratios {
            *ratio = (*ratio).clamp(100, 350);
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_state_is_bounded_and_deduplicated() {
        let repeated = SessionId::new();
        let mut state = DesktopState {
            terminal_font_size: 255,
            window_width: 1,
            window_height: u16::MAX,
            panel_ratios: [0, 1, u16::MAX],
            ..DesktopState::default()
        };
        state.open_session_ids = vec![repeated; MAX_OPEN_DESKTOP_SESSIONS + 4];
        let state = state.normalized();
        assert_eq!(state.open_session_ids, vec![repeated]);
        assert_eq!(state.terminal_font_size, MAX_TERMINAL_FONT_SIZE);
        assert_eq!(state.terminal_cursor, DesktopTerminalCursor::Block);
        assert_eq!(state.window_width, MIN_DESKTOP_WIDTH);
        assert_eq!(state.window_height, MAX_DESKTOP_HEIGHT);
        assert_eq!(state.panel_ratios, [100, 100, 350]);
    }

    #[test]
    fn appearance_choices_have_stable_user_facing_labels() {
        assert_eq!(DesktopTheme::TokyoNight.to_string(), "Tokyo Night");
        assert_eq!(
            DesktopTerminalFont::JetBrainsMono.to_string(),
            "JetBrains Mono"
        );
        assert_eq!(DesktopTerminalCursor::Line.to_string(), "Line");
        assert_eq!(DesktopDensity::Compact.to_string(), "Compact");
    }
}
