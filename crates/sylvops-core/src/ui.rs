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
pub const DEFAULT_TUI_NAVIGATOR_WIDTH: u16 = 34;
pub const MIN_TUI_NAVIGATOR_WIDTH: u16 = 24;
pub const MAX_TUI_NAVIGATOR_WIDTH: u16 = 80;

/// Main content shown beside the explorer.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MainTab {
    #[default]
    Terminal,
    Changes,
    Details,
}

/// Main content shown by the terminal UI.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TuiContentView {
    Terminal,
    Changes,
    Details,
    #[default]
    #[serde(other)]
    Sessions,
}

impl TuiContentView {
    pub const ALL: [Self; 4] = [Self::Terminal, Self::Changes, Self::Details, Self::Sessions];

    #[must_use]
    pub const fn shortcut(self) -> char {
        match self {
            Self::Terminal => '1',
            Self::Changes => '2',
            Self::Details => '3',
            Self::Sessions => '4',
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Terminal => "Terminal",
            Self::Changes => "Changes",
            Self::Details => "Details",
            Self::Sessions => "Sessions",
        }
    }

    #[must_use]
    pub const fn from_shortcut(shortcut: char) -> Option<Self> {
        match shortcut {
            '1' => Some(Self::Terminal),
            '2' => Some(Self::Changes),
            '3' => Some(Self::Details),
            '4' => Some(Self::Sessions),
            _ => None,
        }
    }
}

/// Presentation used by the TUI Active Sessions overview.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TuiSessionPresentation {
    List,
    #[default]
    #[serde(other)]
    Cards,
}

impl TuiSessionPresentation {
    pub const ALL: [Self; 2] = [Self::Cards, Self::List];
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TuiNavigatorView {
    Active,
    #[default]
    #[serde(other)]
    Explorer,
}

/// Navigation state safe to persist between TUI runs.
///
/// Deliberately excludes form values, searches, terminal data, and credentials.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TuiState {
    pub selected_project_id: Option<ProjectId>,
    pub selected_worktree_id: Option<WorktreeId>,
    pub selected_session_id: Option<SessionId>,
    #[serde(alias = "selected_main_tab")]
    pub selected_content_view: TuiContentView,
    pub session_presentation: TuiSessionPresentation,
    pub navigator_view: TuiNavigatorView,
    pub navigator_width: u16,
    pub theme: TuiTheme,
    pub density: TuiDensity,
}

impl Default for TuiState {
    fn default() -> Self {
        Self {
            selected_project_id: None,
            selected_worktree_id: None,
            selected_session_id: None,
            selected_content_view: TuiContentView::Sessions,
            session_presentation: TuiSessionPresentation::Cards,
            navigator_view: TuiNavigatorView::Explorer,
            navigator_width: DEFAULT_TUI_NAVIGATOR_WIDTH,
            theme: TuiTheme::TerminalDefault,
            density: TuiDensity::Comfortable,
        }
    }
}

impl TuiState {
    #[must_use]
    pub fn normalized(mut self) -> Self {
        if self.selected_content_view == TuiContentView::Sessions {
            self.selected_content_view = TuiContentView::Terminal;
        }
        self.navigator_width = self
            .navigator_width
            .clamp(MIN_TUI_NAVIGATOR_WIDTH, MAX_TUI_NAVIGATOR_WIDTH);
        self
    }
}

/// Terminal-safe application chrome themes. Provider-owned terminal output is
/// intentionally rendered independently from these colors.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TuiTheme {
    Nord,
    TokyoNight,
    Catppuccin,
    GruvboxDark,
    SolarizedLight,
    #[default]
    #[serde(other)]
    TerminalDefault,
}

impl TuiTheme {
    pub const ALL: [Self; 6] = [
        Self::TerminalDefault,
        Self::Nord,
        Self::TokyoNight,
        Self::Catppuccin,
        Self::GruvboxDark,
        Self::SolarizedLight,
    ];
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TuiDensity {
    Compact,
    #[default]
    #[serde(other)]
    Comfortable,
}

impl TuiDensity {
    pub const ALL: [Self; 2] = [Self::Comfortable, Self::Compact];
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesktopTheme {
    #[default]
    System,
    #[serde(alias = "light")]
    Grove,
    #[serde(alias = "dark")]
    Canopy,
    Midnight,
    Nord,
    TokyoNight,
    Catppuccin,
    Dracula,
    GruvboxDark,
    SolarizedLight,
    SolarizedDark,
}

impl DesktopTheme {
    pub const ALL: [Self; 11] = [
        Self::System,
        Self::Grove,
        Self::Canopy,
        Self::Midnight,
        Self::Nord,
        Self::TokyoNight,
        Self::Catppuccin,
        Self::Dracula,
        Self::GruvboxDark,
        Self::SolarizedLight,
        Self::SolarizedDark,
    ];
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
            Self::Grove => "Grove",
            Self::Canopy => "Canopy",
            Self::Midnight => "Midnight",
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

impl fmt::Display for TuiTheme {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::TerminalDefault => "Terminal Default",
            Self::Nord => "Nord",
            Self::TokyoNight => "Tokyo Night",
            Self::Catppuccin => "Catppuccin",
            Self::GruvboxDark => "Gruvbox Dark",
            Self::SolarizedLight => "Solarized Light",
        })
    }
}

impl fmt::Display for TuiDensity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Comfortable => "Comfortable",
            Self::Compact => "Compact",
        })
    }
}

impl fmt::Display for TuiSessionPresentation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Cards => "Cards",
            Self::List => "List",
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
    pub periodic_update_checks: bool,
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
            periodic_update_checks: true,
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
    fn tui_appearance_preferences_are_backward_compatible_and_normalized() {
        let legacy: TuiState = serde_json::from_str(
            r#"{"selected_project_id":null,"selected_worktree_id":null,"selected_session_id":null,"selected_main_tab":"terminal"}"#,
        )
        .unwrap();
        assert_eq!(legacy.selected_content_view, TuiContentView::Terminal);
        assert_eq!(legacy.session_presentation, TuiSessionPresentation::Cards);
        assert_eq!(legacy.theme, TuiTheme::TerminalDefault);
        assert_eq!(legacy.density, TuiDensity::Comfortable);

        let unknown: TuiState =
            serde_json::from_str(r#"{"theme":"ultraviolet","density":"microscopic"}"#).unwrap();
        assert_eq!(unknown.theme, TuiTheme::TerminalDefault);
        assert_eq!(unknown.density, TuiDensity::Comfortable);

        let configured = TuiState {
            theme: TuiTheme::TokyoNight,
            density: TuiDensity::Compact,
            selected_content_view: TuiContentView::Sessions,
            session_presentation: TuiSessionPresentation::List,
            ..TuiState::default()
        };
        let encoded = serde_json::to_value(configured).unwrap();
        assert_eq!(encoded["theme"], "tokyo_night");
        assert_eq!(encoded["density"], "compact");
        let mut keys = encoded
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "density",
                "navigator_view",
                "navigator_width",
                "selected_content_view",
                "selected_project_id",
                "selected_session_id",
                "selected_worktree_id",
                "session_presentation",
                "theme",
            ]
        );
        assert_eq!(TuiTheme::TerminalDefault.to_string(), "Terminal Default");
        assert_eq!(TuiTheme::TokyoNight.to_string(), "Tokyo Night");
        assert_eq!(TuiDensity::Compact.to_string(), "Compact");
        assert_eq!(TuiSessionPresentation::List.to_string(), "List");
    }

    #[test]
    fn tui_session_overview_defaults_and_legacy_content_view_are_stable() {
        let fresh = TuiState::default();
        assert_eq!(fresh.selected_content_view, TuiContentView::Sessions);
        assert_eq!(fresh.session_presentation, TuiSessionPresentation::Cards);
        assert_eq!(fresh.navigator_view, TuiNavigatorView::Explorer);
        assert_eq!(fresh.navigator_width, DEFAULT_TUI_NAVIGATOR_WIDTH);
        assert_eq!(
            fresh.clone().normalized().selected_content_view,
            TuiContentView::Terminal
        );
        assert_eq!(
            TuiContentView::ALL.map(|view| (view.shortcut(), view.label())),
            [
                ('1', "Terminal"),
                ('2', "Changes"),
                ('3', "Details"),
                ('4', "Sessions"),
            ]
        );
        assert_eq!(
            TuiContentView::from_shortcut('4'),
            Some(TuiContentView::Sessions)
        );
        assert_eq!(TuiContentView::from_shortcut('5'), None);

        let legacy: TuiState = serde_json::from_str(
            r#"{"selected_main_tab":"details","theme":"nord","density":"compact"}"#,
        )
        .unwrap();
        assert_eq!(legacy.selected_content_view, TuiContentView::Details);
        assert_eq!(legacy.session_presentation, TuiSessionPresentation::Cards);

        let unknown: TuiState = serde_json::from_str(
            r#"{"selected_content_view":"unknown","session_presentation":"unknown"}"#,
        )
        .unwrap();
        assert_eq!(unknown.selected_content_view, TuiContentView::Sessions);
        assert_eq!(unknown.session_presentation, TuiSessionPresentation::Cards);
        assert_eq!(unknown.navigator_view, TuiNavigatorView::Explorer);
    }

    #[test]
    fn appearance_choices_have_stable_user_facing_labels() {
        assert_eq!(DesktopTheme::Grove.to_string(), "Grove");
        assert_eq!(DesktopTheme::Canopy.to_string(), "Canopy");
        assert_eq!(DesktopTheme::Midnight.to_string(), "Midnight");
        assert_eq!(DesktopTheme::TokyoNight.to_string(), "Tokyo Night");
        assert_eq!(
            DesktopTerminalFont::JetBrainsMono.to_string(),
            "JetBrains Mono"
        );
        assert_eq!(DesktopTerminalCursor::Line.to_string(), "Line");
        assert_eq!(DesktopDensity::Compact.to_string(), "Compact");
    }

    #[test]
    fn legacy_light_and_dark_preferences_migrate_to_signature_themes() {
        let light: DesktopState = serde_json::from_str(r#"{"theme":"light"}"#).unwrap();
        let dark: DesktopState = serde_json::from_str(r#"{"theme":"dark"}"#).unwrap();

        assert_eq!(light.theme, DesktopTheme::Grove);
        assert_eq!(dark.theme, DesktopTheme::Canopy);
        assert_eq!(
            serde_json::to_value(light).unwrap()["theme"],
            serde_json::json!("grove")
        );
        assert_eq!(
            serde_json::to_value(dark).unwrap()["theme"],
            serde_json::json!("canopy")
        );
    }

    #[test]
    fn previous_named_theme_preferences_remain_readable() {
        for name in [
            "nord",
            "tokyo_night",
            "catppuccin",
            "dracula",
            "gruvbox_dark",
            "solarized_light",
            "solarized_dark",
        ] {
            let encoded = format!(r#"{{"theme":"{name}"}}"#);
            let state: DesktopState = serde_json::from_str(&encoded).unwrap();
            assert_eq!(serde_json::to_value(state).unwrap()["theme"], name);
        }
    }
}
