#![allow(clippy::unreadable_literal)] // Six-digit values intentionally mirror CSS RGB notation.

use sylvops_core::{
    domain::{DaemonSnapshot, SessionState, WorktreeStatus, session_can_resume},
    ids::{ProjectId, SessionId, WorkspaceId, WorktreeId},
    ui::{DesktopDensity, DesktopPanel, DesktopState, DesktopTheme, MainTab},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Rgb {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
}

impl Rgb {
    pub(crate) const fn hex(value: u32) -> Self {
        Self {
            red: ((value >> 16) & 0xff) as u8,
            green: ((value >> 8) & 0xff) as u8,
            blue: (value & 0xff) as u8,
        }
    }

    fn mix(self, other: Self, other_percent: u16) -> Self {
        let own_percent = 100_u16.saturating_sub(other_percent);
        let channel = |own: u8, other: u8| {
            u8::try_from((u16::from(own) * own_percent + u16::from(other) * other_percent) / 100)
                .unwrap_or(u8::MAX)
        };
        Self {
            red: channel(self.red, other.red),
            green: channel(self.green, other.green),
            blue: channel(self.blue, other.blue),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ButtonIntent {
    Primary,
    Secondary,
    Quiet,
    Danger,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ControlState {
    Rest,
    Hovered,
    Pressed,
    Disabled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ButtonVisual {
    pub background: Rgb,
    pub foreground: Rgb,
    pub border: Rgb,
}

#[derive(Clone, Copy)]
pub(crate) struct ButtonTokens {
    pub canvas: Rgb,
    pub surface: Rgb,
    pub surface_raised: Rgb,
    pub surface_sunken: Rgb,
    pub border: Rgb,
    pub border_strong: Rgb,
    pub text: Rgb,
    pub text_muted: Rgb,
    pub interaction: Rgb,
    pub interaction_text: Rgb,
    pub danger: Rgb,
    pub danger_surface: Rgb,
}

impl From<SemanticTokens> for ButtonTokens {
    fn from(tokens: SemanticTokens) -> Self {
        Self {
            canvas: tokens.canvas,
            surface: tokens.surface,
            surface_raised: tokens.surface_raised,
            surface_sunken: tokens.surface_sunken,
            border: tokens.border,
            border_strong: tokens.border_strong,
            text: tokens.text,
            text_muted: tokens.text_muted,
            interaction: tokens.interaction,
            interaction_text: tokens.interaction_text,
            danger: tokens.danger,
            danger_surface: tokens.danger_surface,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SemanticTokens {
    pub canvas: Rgb,
    pub surface: Rgb,
    pub surface_raised: Rgb,
    pub surface_sunken: Rgb,
    pub border: Rgb,
    pub border_strong: Rgb,
    pub text: Rgb,
    pub text_muted: Rgb,
    pub interaction: Rgb,
    pub interaction_text: Rgb,
    pub focus: Rgb,
    pub selection: Rgb,
    pub selection_text: Rgb,
    pub success: Rgb,
    pub success_surface: Rgb,
    pub attention: Rgb,
    pub attention_surface: Rgb,
    pub danger: Rgb,
    pub danger_surface: Rgb,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TerminalPalette {
    pub foreground: Rgb,
    pub background: Rgb,
    pub ansi: [Rgb; 16],
    pub selection_foreground: Rgb,
    pub selection_background: Rgb,
    pub cursor: Rgb,
}

#[derive(Clone, Copy)]
struct ThemeDefinition {
    label: &'static str,
    tokens: SemanticTokens,
    ansi: [Rgb; 16],
}

impl TerminalPalette {
    pub(crate) fn resolve(self, color: vt100::Color, default: Rgb) -> Rgb {
        match color {
            vt100::Color::Default => default,
            vt100::Color::Rgb(red, green, blue) => Rgb { red, green, blue },
            vt100::Color::Idx(index @ 0..=15) => self.ansi[usize::from(index)],
            vt100::Color::Idx(index @ 16..=231) => {
                let offset = index - 16;
                let levels = [0, 95, 135, 175, 215, 255];
                Rgb {
                    red: levels[usize::from(offset / 36)],
                    green: levels[usize::from((offset % 36) / 6)],
                    blue: levels[usize::from(offset % 6)],
                }
            }
            vt100::Color::Idx(index @ 232..=255) => {
                let value = 8 + (index - 232) * 10;
                Rgb {
                    red: value,
                    green: value,
                    blue: value,
                }
            }
        }
    }

    pub(crate) fn cell_colors(
        self,
        foreground: vt100::Color,
        background: vt100::Color,
        inverse: bool,
    ) -> (Rgb, Rgb) {
        let mut foreground = self.resolve(foreground, self.foreground);
        let mut background = self.resolve(background, self.background);
        if inverse {
            std::mem::swap(&mut foreground, &mut background);
        }
        (foreground, background)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Viewport {
    pub width: u16,
    pub height: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PresentationLayout {
    Wide,
    Compact,
    Narrow,
}

impl PresentationLayout {
    pub(crate) const fn for_width(width: u16) -> Self {
        if width >= 1_180 {
            Self::Wide
        } else if width >= 820 {
            Self::Compact
        } else {
            Self::Narrow
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DensityMetrics {
    pub navigation_height: u32,
    pub panel_header_height: u32,
    pub row_height: u32,
    pub control_height: u32,
    pub navigation_padding: [u16; 2],
    pub region_spacing: u32,
    pub tab_height: u32,
    pub footer_height: u32,
}

impl DensityMetrics {
    const fn for_choice(choice: DesktopDensity) -> Self {
        match choice {
            DesktopDensity::Comfortable => Self {
                navigation_height: 44,
                panel_header_height: 42,
                row_height: 36,
                control_height: 34,
                navigation_padding: [8, 9],
                region_spacing: 10,
                tab_height: 42,
                footer_height: 32,
            },
            DesktopDensity::Compact => Self {
                navigation_height: 38,
                panel_header_height: 36,
                row_height: 30,
                control_height: 30,
                navigation_padding: [5, 8],
                region_spacing: 6,
                tab_height: 36,
                footer_height: 28,
            },
        }
    }
}

impl Viewport {
    pub(crate) const fn new(width: u16, height: u16) -> Self {
        Self { width, height }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SystemAppearance {
    Light,
    Dark,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct InteractionState {
    pub selected_project_id: Option<ProjectId>,
    pub selected_worktree_id: Option<WorktreeId>,
    pub selected_session_id: Option<SessionId>,
    pub active_session_id: Option<SessionId>,
    pub open_session_ids: Vec<SessionId>,
    pub active_session_attached: bool,
    pub main_tab: MainTab,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PresentationSelection {
    pub workspace_id: Option<WorkspaceId>,
    pub project_id: Option<ProjectId>,
    pub worktree_id: Option<WorktreeId>,
    pub session_id: Option<SessionId>,
    pub active_session_id: Option<SessionId>,
    pub main_tab: MainTab,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum NavigatorNodeId {
    Repository(ProjectId),
    Checkout(WorktreeId),
    Session(SessionId),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NavigatorNodeKind {
    Repository,
    Checkout,
    Session,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SessionIndicator {
    Ready,
    Working,
    NeedsFeedback,
    Finished,
    Failed,
    Stopped,
    Disconnected,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum NavigatorAttention {
    Finished,
    NeedsFeedback,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NavigatorAction {
    DeleteCheckout(WorktreeId),
}

impl SessionIndicator {
    const fn for_state(state: SessionState) -> Self {
        match state {
            SessionState::Fresh => Self::Ready,
            SessionState::Starting | SessionState::Running => Self::Working,
            SessionState::NeedsFeedback => Self::NeedsFeedback,
            SessionState::FinishedUnseen | SessionState::FinishedSeen => Self::Finished,
            SessionState::Failed => Self::Failed,
            SessionState::Terminated => Self::Stopped,
            SessionState::Disconnected => Self::Disconnected,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NavigatorRow {
    pub id: NavigatorNodeId,
    pub kind: NavigatorNodeKind,
    pub label: String,
    pub detail: String,
    pub indicator: Option<SessionIndicator>,
    pub attention: Option<NavigatorAttention>,
    pub actions: Vec<NavigatorAction>,
    pub selected: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct NavigatorPresentation {
    pub repositories: Vec<NavigatorRow>,
    pub checkouts: Vec<NavigatorRow>,
    pub sessions: Vec<NavigatorRow>,
    pub mode: NavigatorMode,
    pub active_panel: DesktopPanel,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum NavigatorMode {
    #[default]
    Columns,
    Tabs,
    Drawer,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WorkspaceNavigation {
    ScrollableTabs,
    LabeledSwitcher,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ActiveTerminalAction {
    Open,
    Leave,
    ViewOutput,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OpenSessionPresentation {
    pub id: SessionId,
    pub label: String,
    pub state: String,
    pub active: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ActiveSessionContext {
    pub session_id: SessionId,
    pub repository: String,
    pub checkout: String,
    pub branch: String,
    pub session: String,
    pub state: String,
    pub terminal_action: Option<ActiveTerminalAction>,
    pub can_stop: bool,
    pub can_resume: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ActiveSessionPresentation {
    pub tabs: Vec<OpenSessionPresentation>,
    pub context: Option<ActiveSessionContext>,
}

pub(crate) struct PresentationInput<'a> {
    pub daemon: &'a DaemonSnapshot,
    pub preferences: &'a DesktopState,
    pub viewport: Viewport,
    pub system_appearance: SystemAppearance,
    pub interaction: InteractionState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ResolvedTheme {
    Grove,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PresentationTheme {
    pub name: ResolvedTheme,
    pub label: &'static str,
    pub tokens: SemanticTokens,
    pub terminal: TerminalPalette,
}

impl PresentationTheme {
    pub(crate) fn resolve(choice: DesktopTheme, system_appearance: SystemAppearance) -> Self {
        let name = match choice {
            DesktopTheme::System => match system_appearance {
                SystemAppearance::Light => ResolvedTheme::Grove,
                SystemAppearance::Dark => ResolvedTheme::Canopy,
            },
            DesktopTheme::Grove => ResolvedTheme::Grove,
            DesktopTheme::Canopy => ResolvedTheme::Canopy,
            DesktopTheme::Midnight => ResolvedTheme::Midnight,
            DesktopTheme::Nord => ResolvedTheme::Nord,
            DesktopTheme::TokyoNight => ResolvedTheme::TokyoNight,
            DesktopTheme::Catppuccin => ResolvedTheme::Catppuccin,
            DesktopTheme::Dracula => ResolvedTheme::Dracula,
            DesktopTheme::GruvboxDark => ResolvedTheme::GruvboxDark,
            DesktopTheme::SolarizedLight => ResolvedTheme::SolarizedLight,
            DesktopTheme::SolarizedDark => ResolvedTheme::SolarizedDark,
        };
        Self::for_resolved(name)
    }

    pub(crate) fn for_resolved(name: ResolvedTheme) -> Self {
        let definition = theme_definition(name);
        Self {
            name,
            label: definition.label,
            tokens: definition.tokens,
            terminal: terminal_palette(definition),
        }
    }

    pub(crate) fn button(self, intent: ButtonIntent, state: ControlState) -> ButtonVisual {
        button_visual(self.tokens.into(), intent, state)
    }
}

pub(crate) fn button_visual(
    tokens: ButtonTokens,
    intent: ButtonIntent,
    state: ControlState,
) -> ButtonVisual {
    if state == ControlState::Disabled {
        return ButtonVisual {
            background: tokens.surface_sunken,
            foreground: tokens.text_muted,
            border: tokens.border,
        };
    }

    match intent {
        ButtonIntent::Primary => {
            let background = match state {
                ControlState::Rest => tokens.interaction,
                ControlState::Hovered => tokens.interaction.mix(tokens.interaction_text, 8),
                ControlState::Pressed => tokens.interaction.mix(tokens.text, 14),
                ControlState::Disabled => unreachable!(),
            };
            ButtonVisual {
                background,
                foreground: tokens.interaction_text,
                border: tokens.interaction,
            }
        }
        ButtonIntent::Secondary => ButtonVisual {
            background: match state {
                ControlState::Rest => tokens.surface,
                ControlState::Hovered => tokens.surface_raised,
                ControlState::Pressed => tokens.surface_sunken,
                ControlState::Disabled => unreachable!(),
            },
            foreground: tokens.text,
            border: tokens.border_strong,
        },
        ButtonIntent::Quiet => ButtonVisual {
            background: match state {
                ControlState::Rest => tokens.canvas,
                ControlState::Hovered => tokens.surface,
                ControlState::Pressed => tokens.surface_sunken,
                ControlState::Disabled => unreachable!(),
            },
            foreground: tokens.text,
            border: tokens.border,
        },
        ButtonIntent::Danger => ButtonVisual {
            background: tokens.danger_surface,
            foreground: tokens.danger,
            border: match state {
                ControlState::Rest | ControlState::Hovered => tokens.danger,
                ControlState::Pressed => tokens.text,
                ControlState::Disabled => unreachable!(),
            },
        },
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DesktopPresentation {
    pub theme: PresentationTheme,
    pub density: DensityMetrics,
    pub layout: PresentationLayout,
    pub selection: PresentationSelection,
    pub navigator: NavigatorPresentation,
    pub workspace_navigation: WorkspaceNavigation,
    pub active_session: ActiveSessionPresentation,
}

impl DesktopPresentation {
    pub(crate) fn build(input: &PresentationInput<'_>) -> Self {
        let theme = PresentationTheme::resolve(input.preferences.theme, input.system_appearance);
        let layout = PresentationLayout::for_width(input.viewport.width);
        let workspace_id = input
            .daemon
            .workspaces
            .iter()
            .find(|workspace| workspace.is_open)
            .or_else(|| input.daemon.workspaces.first())
            .map(|workspace| workspace.id);
        let project_id = input.interaction.selected_project_id.filter(|project_id| {
            input.daemon.projects.iter().any(|project| {
                project.id == *project_id && Some(project.workspace_id) == workspace_id
            })
        });
        let worktree_id = input
            .interaction
            .selected_worktree_id
            .filter(|worktree_id| {
                input.daemon.worktrees.iter().any(|worktree| {
                    worktree.id == *worktree_id && Some(worktree.project_id) == project_id
                })
            });
        let session_id = input.interaction.selected_session_id.filter(|session_id| {
            input.daemon.sessions.iter().any(|session| {
                session.id == *session_id && Some(session.worktree_id) == worktree_id
            })
        });
        let active_session_id = input.interaction.active_session_id.filter(|session_id| {
            input
                .daemon
                .sessions
                .iter()
                .any(|session| session.id == *session_id)
        });
        let navigator = build_navigator(
            input.daemon,
            workspace_id,
            project_id,
            worktree_id,
            session_id,
            match layout {
                PresentationLayout::Wide => NavigatorMode::Columns,
                PresentationLayout::Compact => NavigatorMode::Tabs,
                PresentationLayout::Narrow => NavigatorMode::Drawer,
            },
            input.preferences.compact_panel,
        );
        let active_session = build_active_session(
            input.daemon,
            &input.interaction.open_session_ids,
            active_session_id,
            input.interaction.active_session_attached,
        );
        Self {
            theme,
            density: DensityMetrics::for_choice(input.preferences.density),
            layout,
            selection: PresentationSelection {
                workspace_id,
                project_id,
                worktree_id,
                session_id,
                active_session_id,
                main_tab: input.interaction.main_tab,
            },
            navigator,
            workspace_navigation: if layout == PresentationLayout::Narrow {
                WorkspaceNavigation::LabeledSwitcher
            } else {
                WorkspaceNavigation::ScrollableTabs
            },
            active_session,
        }
    }
}

fn build_navigator(
    daemon: &DaemonSnapshot,
    workspace_id: Option<WorkspaceId>,
    selected_project_id: Option<ProjectId>,
    selected_worktree_id: Option<WorktreeId>,
    selected_session_id: Option<SessionId>,
    mode: NavigatorMode,
    active_panel: DesktopPanel,
) -> NavigatorPresentation {
    NavigatorPresentation {
        repositories: build_repository_rows(daemon, workspace_id, selected_project_id),
        checkouts: build_checkout_rows(daemon, selected_project_id, selected_worktree_id),
        sessions: build_session_rows(daemon, selected_worktree_id, selected_session_id),
        mode,
        active_panel,
    }
}

fn build_repository_rows(
    daemon: &DaemonSnapshot,
    workspace_id: Option<WorkspaceId>,
    selected_project_id: Option<ProjectId>,
) -> Vec<NavigatorRow> {
    let mut projects: Vec<_> = daemon
        .projects
        .iter()
        .filter(|project| Some(project.workspace_id) == workspace_id)
        .collect();
    projects.sort_by(|left, right| {
        right
            .last_activity_at
            .cmp(&left.last_activity_at)
            .then_with(|| left.name.cmp(&right.name))
    });

    projects
        .into_iter()
        .map(|project| {
            let worktrees: Vec<_> = daemon
                .worktrees
                .iter()
                .filter(|worktree| {
                    worktree.project_id == project.id && worktree.status != WorktreeStatus::Removed
                })
                .collect();
            let attention = worktrees
                .iter()
                .flat_map(|worktree| {
                    daemon
                        .sessions
                        .iter()
                        .filter(move |session| session.worktree_id == worktree.id)
                })
                .filter_map(|session| attention_for_state(session.state))
                .max();
            NavigatorRow {
                id: NavigatorNodeId::Repository(project.id),
                kind: NavigatorNodeKind::Repository,
                label: project.name.clone(),
                detail: "Repository".into(),
                indicator: None,
                attention,
                actions: Vec::new(),
                selected: selected_project_id == Some(project.id),
            }
        })
        .collect()
}

fn build_checkout_rows(
    daemon: &DaemonSnapshot,
    selected_project_id: Option<ProjectId>,
    selected_worktree_id: Option<WorktreeId>,
) -> Vec<NavigatorRow> {
    let mut worktrees: Vec<_> = daemon
        .worktrees
        .iter()
        .filter(|worktree| {
            Some(worktree.project_id) == selected_project_id
                && worktree.status != WorktreeStatus::Removed
        })
        .collect();
    worktrees.sort_by(|left, right| {
        right
            .is_root_checkout
            .cmp(&left.is_root_checkout)
            .then_with(|| right.last_activity_at.cmp(&left.last_activity_at))
            .then_with(|| left.name.cmp(&right.name))
    });
    worktrees
        .into_iter()
        .map(|worktree| {
            let sessions: Vec<_> = daemon
                .sessions
                .iter()
                .filter(|session| session.worktree_id == worktree.id)
                .collect();
            let mut actions = Vec::new();
            if !worktree.is_root_checkout && worktree.status == WorktreeStatus::Active {
                actions.push(NavigatorAction::DeleteCheckout(worktree.id));
            }
            NavigatorRow {
                id: NavigatorNodeId::Checkout(worktree.id),
                kind: NavigatorNodeKind::Checkout,
                label: worktree.name.clone(),
                detail: format!(
                    "{} · {}",
                    worktree.branch.as_deref().unwrap_or("detached"),
                    if worktree.is_root_checkout {
                        "Root"
                    } else {
                        "Managed"
                    }
                ),
                indicator: None,
                attention: sessions
                    .iter()
                    .filter_map(|session| attention_for_state(session.state))
                    .max(),
                actions,
                selected: selected_worktree_id == Some(worktree.id),
            }
        })
        .collect()
}

fn build_session_rows(
    daemon: &DaemonSnapshot,
    selected_worktree_id: Option<WorktreeId>,
    selected_session_id: Option<SessionId>,
) -> Vec<NavigatorRow> {
    let mut sessions: Vec<_> = daemon
        .sessions
        .iter()
        .filter(|session| Some(session.worktree_id) == selected_worktree_id)
        .collect();
    sessions.sort_by(|left, right| {
        right
            .last_activity_at
            .cmp(&left.last_activity_at)
            .then_with(|| left.display_name.cmp(&right.display_name))
    });
    sessions
        .into_iter()
        .map(|session| NavigatorRow {
            id: NavigatorNodeId::Session(session.id),
            kind: NavigatorNodeKind::Session,
            label: session.display_name.clone(),
            detail: session_state_label(session.state).to_owned(),
            indicator: Some(SessionIndicator::for_state(session.state)),
            attention: attention_for_state(session.state),
            actions: Vec::new(),
            selected: selected_session_id == Some(session.id),
        })
        .collect()
}

fn build_active_session(
    daemon: &DaemonSnapshot,
    open_session_ids: &[SessionId],
    active_session_id: Option<SessionId>,
    active_session_attached: bool,
) -> ActiveSessionPresentation {
    let tabs = open_session_ids
        .iter()
        .filter_map(|session_id| {
            daemon
                .sessions
                .iter()
                .find(|session| session.id == *session_id)
        })
        .map(|session| OpenSessionPresentation {
            id: session.id,
            label: session.display_name.clone(),
            state: session_state_label(session.state).to_owned(),
            active: active_session_id == Some(session.id),
        })
        .collect();
    let context = active_session_id.and_then(|session_id| {
        let session = daemon
            .sessions
            .iter()
            .find(|session| session.id == session_id)?;
        let worktree = daemon
            .worktrees
            .iter()
            .find(|worktree| worktree.id == session.worktree_id)?;
        let project = daemon
            .projects
            .iter()
            .find(|project| project.id == worktree.project_id)?;
        Some(ActiveSessionContext {
            session_id,
            repository: project.name.clone(),
            checkout: worktree.name.clone(),
            branch: worktree.branch.as_deref().unwrap_or("detached").to_owned(),
            session: session.display_name.clone(),
            state: session_state_label(session.state).to_owned(),
            terminal_action: if active_session_attached {
                Some(ActiveTerminalAction::Leave)
            } else if session_state_can_stop(session.state) {
                Some(ActiveTerminalAction::Open)
            } else if session_state_can_replay(session.state) {
                Some(ActiveTerminalAction::ViewOutput)
            } else {
                None
            },
            can_stop: session_state_can_stop(session.state),
            can_resume: session_can_resume(session, &daemon.sessions),
        })
    });
    ActiveSessionPresentation { tabs, context }
}

const fn attention_for_state(state: SessionState) -> Option<NavigatorAttention> {
    match state {
        SessionState::FinishedUnseen => Some(NavigatorAttention::Finished),
        SessionState::NeedsFeedback => Some(NavigatorAttention::NeedsFeedback),
        SessionState::Failed => Some(NavigatorAttention::Failed),
        SessionState::Fresh
        | SessionState::Starting
        | SessionState::Running
        | SessionState::FinishedSeen
        | SessionState::Terminated
        | SessionState::Disconnected => None,
    }
}

pub(crate) const fn session_state_can_stop(state: SessionState) -> bool {
    matches!(
        state,
        SessionState::Fresh
            | SessionState::Starting
            | SessionState::Running
            | SessionState::NeedsFeedback
    )
}

pub(crate) const fn session_state_can_replay(state: SessionState) -> bool {
    matches!(
        state,
        SessionState::FinishedUnseen | SessionState::FinishedSeen | SessionState::Failed
    )
}

pub(crate) const fn session_state_label(state: SessionState) -> &'static str {
    match state {
        SessionState::Fresh => "Ready",
        SessionState::Starting => "Starting",
        SessionState::Running => "Running",
        SessionState::NeedsFeedback => "Needs feedback",
        SessionState::FinishedUnseen | SessionState::FinishedSeen => "Finished",
        SessionState::Failed => "Failed",
        SessionState::Terminated => "Stopped",
        SessionState::Disconnected => "Disconnected",
    }
}

#[allow(clippy::too_many_arguments)]
const fn tokens(
    canvas: u32,
    surface: u32,
    surface_raised: u32,
    surface_sunken: u32,
    border: u32,
    border_strong: u32,
    text: u32,
    text_muted: u32,
    interaction: u32,
    success: u32,
    attention: u32,
    danger: u32,
    light: bool,
) -> SemanticTokens {
    let interaction_text = if light {
        Rgb::hex(0xFFFFFF)
    } else {
        Rgb::hex(canvas)
    };
    SemanticTokens {
        canvas: Rgb::hex(canvas),
        surface: Rgb::hex(surface),
        surface_raised: Rgb::hex(surface_raised),
        surface_sunken: Rgb::hex(surface_sunken),
        border: Rgb::hex(border),
        border_strong: Rgb::hex(border_strong),
        text: Rgb::hex(text),
        text_muted: Rgb::hex(text_muted),
        interaction: Rgb::hex(interaction),
        interaction_text,
        focus: Rgb::hex(interaction),
        selection: Rgb::hex(interaction),
        selection_text: interaction_text,
        success: Rgb::hex(success),
        success_surface: Rgb::hex(if light {
            surface_raised
        } else {
            surface_sunken
        }),
        attention: Rgb::hex(attention),
        attention_surface: Rgb::hex(if light {
            surface_raised
        } else {
            surface_sunken
        }),
        danger: Rgb::hex(danger),
        danger_surface: Rgb::hex(if light {
            surface_raised
        } else {
            surface_sunken
        }),
    }
}

const fn theme_definition(theme: ResolvedTheme) -> ThemeDefinition {
    match theme {
        ResolvedTheme::Grove => ThemeDefinition {
            label: "Grove",
            tokens: tokens(
                0xF4F1E8, 0xFCFAF4, 0xFFFFFF, 0xEAE7DC, 0xD8D2C3, 0xB9B09D, 0x1C2924, 0x59665F,
                0x117681, 0x217A4A, 0x8A5700, 0xB23645, true,
            ),
            ansi: GROVE_ANSI,
        },
        ResolvedTheme::Canopy => ThemeDefinition {
            label: "Canopy",
            tokens: tokens(
                0x101915, 0x16231D, 0x1C2C24, 0x0C1310, 0x2A4136, 0x3B5A4B, 0xE7EEE9, 0xA7B5AC,
                0x69CBD3, 0x6FCF92, 0xF0BC65, 0xF17E88, false,
            ),
            ansi: CANOPY_ANSI,
        },
        ResolvedTheme::Midnight => ThemeDefinition {
            label: "Midnight",
            tokens: tokens(
                0x0C111B, 0x121A28, 0x192438, 0x080C13, 0x28364A, 0x3A4C65, 0xE6ECF4, 0xA2AEC0,
                0x70C7E6, 0x71D39A, 0xF0B96A, 0xF08089, false,
            ),
            ansi: MIDNIGHT_ANSI,
        },
        ResolvedTheme::Nord => ThemeDefinition {
            label: "Nord",
            tokens: tokens(
                0x2E3440, 0x3B4252, 0x434C5E, 0x242933, 0x4C566A, 0x5E6B82, 0xECEFF4, 0xD8DEE9,
                0x88C0D0, 0xA3BE8C, 0xEBCB8B, 0xE5868F, false,
            ),
            ansi: DEFAULT_ANSI,
        },
        ResolvedTheme::TokyoNight => ThemeDefinition {
            label: "Tokyo Night",
            tokens: tokens(
                0x1A1B26, 0x24283B, 0x292E42, 0x16161E, 0x414868, 0x565F89, 0xC0CAF5, 0xA9B1D6,
                0x7AA2F7, 0x9ECE6A, 0xE0AF68, 0xF7768E, false,
            ),
            ansi: DEFAULT_ANSI,
        },
        ResolvedTheme::Catppuccin => ThemeDefinition {
            label: "Catppuccin",
            tokens: tokens(
                0x1E1E2E, 0x313244, 0x45475A, 0x181825, 0x585B70, 0x6C7086, 0xCDD6F4, 0xBAC2DE,
                0x89B4FA, 0xA6E3A1, 0xF9E2AF, 0xF38BA8, false,
            ),
            ansi: DEFAULT_ANSI,
        },
        ResolvedTheme::Dracula => ThemeDefinition {
            label: "Dracula",
            tokens: tokens(
                0x282A36, 0x343746, 0x44475A, 0x21222C, 0x6272A4, 0x7C8BC0, 0xF8F8F2, 0xD6D6D1,
                0x8BE9FD, 0x50FA7B, 0xF1FA8C, 0xFF5555, false,
            ),
            ansi: DEFAULT_ANSI,
        },
        ResolvedTheme::GruvboxDark => ThemeDefinition {
            label: "Gruvbox Dark",
            tokens: tokens(
                0x282828, 0x3C3836, 0x504945, 0x1D2021, 0x665C54, 0x7C6F64, 0xEBDBB2, 0xD5C4A1,
                0x83A598, 0xB8BB26, 0xFABD2F, 0xFB4934, false,
            ),
            ansi: DEFAULT_ANSI,
        },
        ResolvedTheme::SolarizedLight => ThemeDefinition {
            label: "Solarized Light",
            tokens: tokens(
                0xFDF6E3, 0xEEE8D5, 0xFFFFFF, 0xE5DFCC, 0xD6CEB8, 0xB8AF98, 0x073642, 0x4B6066,
                0x006D82, 0x287000, 0x8A5700, 0xC52B3A, true,
            ),
            ansi: DEFAULT_ANSI,
        },
        ResolvedTheme::SolarizedDark => ThemeDefinition {
            label: "Solarized Dark",
            tokens: tokens(
                0x002B36, 0x073642, 0x0D4652, 0x001F27, 0x2B5963, 0x47727B, 0xEEE8D5, 0xAAB8B6,
                0x2AAFC0, 0x8FBF32, 0xE7B94C, 0xF36B72, false,
            ),
            ansi: DEFAULT_ANSI,
        },
    }
}

const DEFAULT_ANSI: [Rgb; 16] = [
    Rgb::hex(0x1E1E1E),
    Rgb::hex(0xF14C4C),
    Rgb::hex(0x23D18B),
    Rgb::hex(0xE5E510),
    Rgb::hex(0x3B8EEA),
    Rgb::hex(0xD670D6),
    Rgb::hex(0x29B8DB),
    Rgb::hex(0xE5E5E5),
    Rgb::hex(0x666666),
    Rgb::hex(0xF14C4C),
    Rgb::hex(0x23D18B),
    Rgb::hex(0xF5F543),
    Rgb::hex(0x3B8EEA),
    Rgb::hex(0xD670D6),
    Rgb::hex(0x29B8DB),
    Rgb::hex(0xFFFFFF),
];

const GROVE_ANSI: [Rgb; 16] = [
    Rgb::hex(0x27312C),
    Rgb::hex(0xB23645),
    Rgb::hex(0x217A4A),
    Rgb::hex(0x8A5700),
    Rgb::hex(0x117681),
    Rgb::hex(0x76538A),
    Rgb::hex(0x247783),
    Rgb::hex(0xD8D2C3),
    Rgb::hex(0x59665F),
    Rgb::hex(0xD04A58),
    Rgb::hex(0x31945D),
    Rgb::hex(0xA86E11),
    Rgb::hex(0x258E99),
    Rgb::hex(0x8D67A0),
    Rgb::hex(0x3895A0),
    Rgb::hex(0xFCFAF4),
];

const CANOPY_ANSI: [Rgb; 16] = [
    Rgb::hex(0x0C1310),
    Rgb::hex(0xF17E88),
    Rgb::hex(0x6FCF92),
    Rgb::hex(0xF0BC65),
    Rgb::hex(0x69CBD3),
    Rgb::hex(0xC69BDD),
    Rgb::hex(0x5DC4B5),
    Rgb::hex(0xD7E0DA),
    Rgb::hex(0x607268),
    Rgb::hex(0xFF9AA2),
    Rgb::hex(0x8BE5AA),
    Rgb::hex(0xFFD07D),
    Rgb::hex(0x83E0E7),
    Rgb::hex(0xDBB5EE),
    Rgb::hex(0x79DDD0),
    Rgb::hex(0xF5FAF7),
];

const MIDNIGHT_ANSI: [Rgb; 16] = [
    Rgb::hex(0x080C13),
    Rgb::hex(0xF08089),
    Rgb::hex(0x71D39A),
    Rgb::hex(0xF0B96A),
    Rgb::hex(0x70C7E6),
    Rgb::hex(0xB9A3E3),
    Rgb::hex(0x61C8CC),
    Rgb::hex(0xD6DEEA),
    Rgb::hex(0x5C6879),
    Rgb::hex(0xFF9BA2),
    Rgb::hex(0x8CE8B0),
    Rgb::hex(0xFFD083),
    Rgb::hex(0x8ADCF5),
    Rgb::hex(0xD0B9F4),
    Rgb::hex(0x7DE0E2),
    Rgb::hex(0xF6F8FC),
];

const fn terminal_palette(definition: ThemeDefinition) -> TerminalPalette {
    let tokens = definition.tokens;
    TerminalPalette {
        foreground: tokens.text,
        background: tokens.surface_sunken,
        ansi: definition.ansi,
        selection_foreground: tokens.selection_text,
        selection_background: tokens.selection,
        cursor: tokens.focus,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sylvops_core::{
        domain::{
            DaemonSnapshot, Project, ProviderKind, Session, SessionState, Workspace, Worktree,
            WorktreeStatus,
        },
        ids::{ProjectId, SessionId, WorkspaceId, WorktreeId},
        ui::{DesktopDensity, DesktopState},
    };

    struct HierarchyFixture {
        snapshot: DaemonSnapshot,
        project_id: ProjectId,
        worktree_id: WorktreeId,
        session_id: SessionId,
    }

    fn hierarchy_fixture(session_state: SessionState) -> HierarchyFixture {
        let workspace_id = WorkspaceId::new();
        let project_id = ProjectId::new();
        let worktree_id = WorktreeId::new();
        let session_id = SessionId::new();
        HierarchyFixture {
            snapshot: DaemonSnapshot {
                workspaces: vec![Workspace {
                    id: workspace_id,
                    name: "Local".into(),
                    created_at: 1,
                    updated_at: 1,
                    last_opened_at: Some(1),
                    is_open: true,
                }],
                projects: vec![Project {
                    id: project_id,
                    workspace_id,
                    name: "SylvOps".into(),
                    repository_path: "C:/repo".into(),
                    canonical_repository_path: "C:/repo".into(),
                    default_branch: Some("main".into()),
                    remote_url: None,
                    created_at: 1,
                    last_activity_at: 3,
                }],
                worktrees: vec![Worktree {
                    id: worktree_id,
                    project_id,
                    name: "Primary".into(),
                    path: "C:/repo".into(),
                    canonical_path: "C:/repo".into(),
                    branch: Some("main".into()),
                    base_ref: "main".into(),
                    base_commit: "abc".into(),
                    is_root_checkout: true,
                    status: WorktreeStatus::Active,
                    created_at: 1,
                    last_activity_at: 2,
                    removed_at: None,
                }],
                sessions: vec![Session {
                    id: session_id,
                    worktree_id,
                    provider_profile_id: None,
                    provider_kind: ProviderKind::Codex,
                    display_name: "Review issue 65".into(),
                    state: session_state,
                    process_id: None,
                    external_session_id: None,
                    command: "codex".into(),
                    arguments_json: "[]".into(),
                    cwd: "C:/repo".into(),
                    created_at: 1,
                    started_at: Some(1),
                    ended_at: None,
                    last_activity_at: 1,
                    last_seen_output_sequence: 0,
                    exit_code: None,
                    failure_reason: None,
                }],
                ..DaemonSnapshot::default()
            },
            project_id,
            worktree_id,
            session_id,
        }
    }

    #[test]
    fn system_appearance_selects_the_matching_signature_theme() {
        let snapshot = DaemonSnapshot::default();
        let preferences = DesktopState::default();
        let interaction = InteractionState::default();

        let light = DesktopPresentation::build(&PresentationInput {
            daemon: &snapshot,
            preferences: &preferences,
            viewport: Viewport::new(1_440, 900),
            system_appearance: SystemAppearance::Light,
            interaction: interaction.clone(),
        });
        let dark = DesktopPresentation::build(&PresentationInput {
            daemon: &snapshot,
            preferences: &preferences,
            viewport: Viewport::new(1_440, 900),
            system_appearance: SystemAppearance::Dark,
            interaction,
        });

        assert_eq!(light.theme.name, ResolvedTheme::Grove);
        assert_eq!(dark.theme.name, ResolvedTheme::Canopy);
    }

    #[test]
    fn signature_themes_publish_the_authored_semantic_foundations() {
        let snapshot = DaemonSnapshot::default();
        let mut preferences = DesktopState::default();
        let interaction = InteractionState::default();
        let mut present = |theme| {
            preferences.theme = theme;
            DesktopPresentation::build(&PresentationInput {
                daemon: &snapshot,
                preferences: &preferences,
                viewport: Viewport::new(1_440, 900),
                system_appearance: SystemAppearance::Dark,
                interaction: interaction.clone(),
            })
            .theme
            .tokens
        };

        let grove = present(DesktopTheme::Grove);
        assert_eq!(grove.canvas, Rgb::hex(0xF4F1E8));
        assert_eq!(grove.surface, Rgb::hex(0xFCFAF4));
        assert_eq!(grove.surface_raised, Rgb::hex(0xFFFFFF));
        assert_eq!(grove.surface_sunken, Rgb::hex(0xEAE7DC));
        assert_eq!(grove.border, Rgb::hex(0xD8D2C3));
        assert_eq!(grove.border_strong, Rgb::hex(0xB9B09D));
        assert_eq!(grove.text, Rgb::hex(0x1C2924));
        assert_eq!(grove.text_muted, Rgb::hex(0x59665F));
        assert_eq!(grove.interaction, Rgb::hex(0x117681));
        assert_eq!(grove.success, Rgb::hex(0x217A4A));
        assert_eq!(grove.attention, Rgb::hex(0x8A5700));
        assert_eq!(grove.danger, Rgb::hex(0xB23645));

        let canopy = present(DesktopTheme::Canopy);
        assert_eq!(canopy.canvas, Rgb::hex(0x101915));
        assert_eq!(canopy.surface, Rgb::hex(0x16231D));
        assert_eq!(canopy.interaction, Rgb::hex(0x69CBD3));

        let midnight = present(DesktopTheme::Midnight);
        assert_eq!(midnight.canvas, Rgb::hex(0x0C111B));
        assert_eq!(midnight.surface, Rgb::hex(0x121A28));
        assert_eq!(midnight.interaction, Rgb::hex(0x70C7E6));

        for tokens in [grove, canopy, midnight] {
            assert_ne!(tokens.focus, tokens.canvas);
            assert_ne!(tokens.selection, tokens.canvas);
            assert_ne!(tokens.success_surface, tokens.canvas);
            assert_ne!(tokens.attention_surface, tokens.canvas);
            assert_ne!(tokens.danger_surface, tokens.canvas);
        }
    }

    fn linear(channel: u8) -> f64 {
        let value = f64::from(channel) / 255.0;
        if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    }

    fn contrast(foreground: Rgb, background: Rgb) -> f64 {
        let foreground = 0.2126 * linear(foreground.red)
            + 0.7152 * linear(foreground.green)
            + 0.0722 * linear(foreground.blue);
        let background = 0.2126 * linear(background.red)
            + 0.7152 * linear(background.green)
            + 0.0722 * linear(background.blue);
        (foreground.max(background) + 0.05) / (foreground.min(background) + 0.05)
    }

    #[test]
    fn every_used_semantic_pair_meets_wcag_aa_in_every_theme_and_state() {
        let snapshot = DaemonSnapshot::default();
        let interaction = InteractionState::default();

        for choice in DesktopTheme::ALL {
            let preferences = DesktopState {
                theme: choice,
                ..DesktopState::default()
            };
            let theme = DesktopPresentation::build(&PresentationInput {
                daemon: &snapshot,
                preferences: &preferences,
                viewport: Viewport::new(1_440, 900),
                system_appearance: SystemAppearance::Dark,
                interaction: interaction.clone(),
            })
            .theme;
            let tokens = theme.tokens;
            let semantic_pairs = [
                ("canvas", tokens.text, tokens.canvas),
                ("surface", tokens.text, tokens.surface),
                ("raised surface", tokens.text, tokens.surface_raised),
                ("sunken surface", tokens.text, tokens.surface_sunken),
                ("muted canvas", tokens.text_muted, tokens.canvas),
                ("muted surface", tokens.text_muted, tokens.surface),
                ("muted raised", tokens.text_muted, tokens.surface_raised),
                ("muted sunken", tokens.text_muted, tokens.surface_sunken),
                ("selection", tokens.selection_text, tokens.selection),
                ("success", tokens.success, tokens.success_surface),
                ("attention", tokens.attention, tokens.attention_surface),
                ("danger", tokens.danger, tokens.danger_surface),
            ];
            for (role, foreground, background) in semantic_pairs {
                assert!(
                    contrast(foreground, background) >= 4.5,
                    "{choice} {role} contrast was {}",
                    contrast(foreground, background)
                );
            }

            for intent in [
                ButtonIntent::Primary,
                ButtonIntent::Secondary,
                ButtonIntent::Quiet,
                ButtonIntent::Danger,
            ] {
                for state in [
                    ControlState::Rest,
                    ControlState::Hovered,
                    ControlState::Pressed,
                    ControlState::Disabled,
                ] {
                    let button = theme.button(intent, state);
                    assert!(
                        contrast(button.foreground, button.background) >= 4.5,
                        "{choice} {intent:?} {state:?} contrast was {}",
                        contrast(button.foreground, button.background)
                    );
                }
            }
        }
    }

    #[test]
    fn viewport_and_density_are_resolved_independently_from_terminal_text_size() {
        let snapshot = DaemonSnapshot::default();
        let interaction = InteractionState::default();
        let present = |density, terminal_font_size, width| {
            let preferences = DesktopState {
                density,
                terminal_font_size,
                ..DesktopState::default()
            };
            DesktopPresentation::build(&PresentationInput {
                daemon: &snapshot,
                preferences: &preferences,
                viewport: Viewport::new(width, 900),
                system_appearance: SystemAppearance::Dark,
                interaction: interaction.clone(),
            })
        };

        let comfortable_small = present(DesktopDensity::Comfortable, 10, 1_440);
        let comfortable_large = present(DesktopDensity::Comfortable, 22, 1_440);
        let compact = present(DesktopDensity::Compact, 22, 900);
        let narrow = present(DesktopDensity::Compact, 10, 680);

        assert_eq!(comfortable_small.density, comfortable_large.density);
        assert!(comfortable_small.density.row_height > compact.density.row_height);
        assert!(comfortable_small.density.control_height > compact.density.control_height);
        assert_eq!(comfortable_small.layout, PresentationLayout::Wide);
        assert_eq!(compact.layout, PresentationLayout::Compact);
        assert_eq!(narrow.layout, PresentationLayout::Narrow);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn interaction_selection_is_validated_against_the_daemon_hierarchy() {
        use sylvops_core::domain::{
            Project, ProviderKind, Session, SessionState, Workspace, Worktree, WorktreeStatus,
        };
        use sylvops_core::ids::{ProjectId, SessionId, WorkspaceId, WorktreeId};

        let workspace_id = WorkspaceId::new();
        let project_id = ProjectId::new();
        let worktree_id = WorktreeId::new();
        let session_id = SessionId::new();
        let snapshot = DaemonSnapshot {
            workspaces: vec![Workspace {
                id: workspace_id,
                name: "Local".into(),
                created_at: 1,
                updated_at: 1,
                last_opened_at: Some(1),
                is_open: true,
            }],
            projects: vec![Project {
                id: project_id,
                workspace_id,
                name: "SylvOps".into(),
                repository_path: "C:/repo".into(),
                canonical_repository_path: "C:/repo".into(),
                default_branch: Some("main".into()),
                remote_url: None,
                created_at: 1,
                last_activity_at: 1,
            }],
            worktrees: vec![Worktree {
                id: worktree_id,
                project_id,
                name: "main".into(),
                path: "C:/repo".into(),
                canonical_path: "C:/repo".into(),
                branch: Some("main".into()),
                base_ref: "main".into(),
                base_commit: "abc".into(),
                is_root_checkout: true,
                status: WorktreeStatus::Active,
                created_at: 1,
                last_activity_at: 1,
                removed_at: None,
            }],
            sessions: vec![Session {
                id: session_id,
                worktree_id,
                provider_profile_id: None,
                provider_kind: ProviderKind::Codex,
                display_name: "Codex".into(),
                state: SessionState::Running,
                process_id: None,
                external_session_id: None,
                command: "codex".into(),
                arguments_json: "[]".into(),
                cwd: "C:/repo".into(),
                created_at: 1,
                started_at: Some(1),
                ended_at: None,
                last_activity_at: 1,
                last_seen_output_sequence: 0,
                exit_code: None,
                failure_reason: None,
            }],
            ..DaemonSnapshot::default()
        };
        let preferences = DesktopState::default();
        let valid = DesktopPresentation::build(&PresentationInput {
            daemon: &snapshot,
            preferences: &preferences,
            viewport: Viewport::new(1_440, 900),
            system_appearance: SystemAppearance::Dark,
            interaction: InteractionState {
                selected_project_id: Some(project_id),
                selected_worktree_id: Some(worktree_id),
                selected_session_id: Some(session_id),
                active_session_id: Some(session_id),
                main_tab: MainTab::Terminal,
                ..InteractionState::default()
            },
        });
        assert_eq!(valid.selection.workspace_id, Some(workspace_id));
        assert_eq!(valid.selection.project_id, Some(project_id));
        assert_eq!(valid.selection.worktree_id, Some(worktree_id));
        assert_eq!(valid.selection.session_id, Some(session_id));
        assert_eq!(valid.selection.active_session_id, Some(session_id));

        let invalid = DesktopPresentation::build(&PresentationInput {
            daemon: &snapshot,
            preferences: &preferences,
            viewport: Viewport::new(1_440, 900),
            system_appearance: SystemAppearance::Dark,
            interaction: InteractionState {
                selected_project_id: Some(ProjectId::new()),
                selected_worktree_id: Some(WorktreeId::new()),
                selected_session_id: Some(SessionId::new()),
                active_session_id: Some(SessionId::new()),
                main_tab: MainTab::Details,
                ..InteractionState::default()
            },
        });
        assert_eq!(invalid.selection.workspace_id, Some(workspace_id));
        assert_eq!(invalid.selection.project_id, None);
        assert_eq!(invalid.selection.worktree_id, None);
        assert_eq!(invalid.selection.session_id, None);
        assert_eq!(invalid.selection.active_session_id, None);
        assert_eq!(invalid.selection.main_tab, MainTab::Details);
    }

    #[test]
    fn terminal_palettes_are_coordinated_without_overriding_provider_rgb_or_inverse() {
        let snapshot = DaemonSnapshot::default();
        let interaction = InteractionState::default();
        let present = |choice| {
            let preferences = DesktopState {
                theme: choice,
                ..DesktopState::default()
            };
            DesktopPresentation::build(&PresentationInput {
                daemon: &snapshot,
                preferences: &preferences,
                viewport: Viewport::new(1_440, 900),
                system_appearance: SystemAppearance::Dark,
                interaction: interaction.clone(),
            })
            .theme
            .terminal
        };

        let grove = present(DesktopTheme::Grove);
        let canopy = present(DesktopTheme::Canopy);
        let midnight = present(DesktopTheme::Midnight);
        assert_ne!(grove.ansi, canopy.ansi);
        assert_ne!(canopy.ansi, midnight.ansi);
        assert_eq!(grove.background, Rgb::hex(0xEAE7DC));
        assert_eq!(canopy.background, Rgb::hex(0x0C1310));
        assert_eq!(midnight.background, Rgb::hex(0x080C13));

        let provider_rgb = vt100::Color::Rgb(12, 34, 56);
        assert_eq!(
            canopy.resolve(provider_rgb, canopy.foreground),
            Rgb::hex(0x0C2238)
        );
        assert_eq!(
            canopy.resolve(vt100::Color::Idx(1), canopy.foreground),
            canopy.ansi[1]
        );
        assert_eq!(
            canopy.resolve(vt100::Color::Idx(196), canopy.foreground),
            Rgb::hex(0xFF0000)
        );
        assert_eq!(
            canopy.cell_colors(provider_rgb, vt100::Color::Default, false),
            (Rgb::hex(0x0C2238), canopy.background)
        );
        assert_eq!(
            canopy.cell_colors(provider_rgb, vt100::Color::Default, true),
            (canopy.background, Rgb::hex(0x0C2238))
        );
    }

    #[test]
    fn navigator_presents_independent_selected_columns() {
        let fixture = hierarchy_fixture(SessionState::Running);
        let preferences = DesktopState::default();
        let presentation = DesktopPresentation::build(&PresentationInput {
            daemon: &fixture.snapshot,
            preferences: &preferences,
            viewport: Viewport::new(1_440, 900),
            system_appearance: SystemAppearance::Dark,
            interaction: InteractionState {
                selected_project_id: Some(fixture.project_id),
                selected_worktree_id: Some(fixture.worktree_id),
                selected_session_id: Some(fixture.session_id),
                active_session_id: Some(fixture.session_id),
                main_tab: MainTab::Terminal,
                ..InteractionState::default()
            },
        });

        assert_eq!(presentation.navigator.repositories[0].label, "SylvOps");
        assert_eq!(presentation.navigator.checkouts[0].detail, "main · Root");
        assert_eq!(
            presentation.navigator.sessions[0].indicator,
            Some(SessionIndicator::Working)
        );
        assert!(presentation.navigator.repositories[0].selected);
        assert!(presentation.navigator.checkouts[0].selected);
        assert!(presentation.navigator.sessions[0].selected);
    }

    #[test]
    fn navigator_orders_each_column_deterministically() {
        let mut fixture = hierarchy_fixture(SessionState::Running);
        let mut recent_project = fixture.snapshot.projects[0].clone();
        recent_project.id = ProjectId::new();
        recent_project.name = "Recent repository".into();
        recent_project.last_activity_at = 10;
        fixture.snapshot.projects.push(recent_project);

        let mut managed_worktree = fixture.snapshot.worktrees[0].clone();
        managed_worktree.id = WorktreeId::new();
        managed_worktree.name = "Feature".into();
        managed_worktree.is_root_checkout = false;
        managed_worktree.last_activity_at = 20;
        fixture.snapshot.worktrees.push(managed_worktree);

        let mut recent_session = fixture.snapshot.sessions[0].clone();
        recent_session.id = SessionId::new();
        recent_session.display_name = "Newest session".into();
        recent_session.last_activity_at = 20;
        fixture.snapshot.sessions.push(recent_session);

        let preferences = DesktopState::default();
        let presentation = DesktopPresentation::build(&PresentationInput {
            daemon: &fixture.snapshot,
            preferences: &preferences,
            viewport: Viewport::new(1_440, 900),
            system_appearance: SystemAppearance::Dark,
            interaction: InteractionState {
                selected_project_id: Some(fixture.project_id),
                selected_worktree_id: Some(fixture.worktree_id),
                ..InteractionState::default()
            },
        });

        assert_eq!(
            presentation
                .navigator
                .repositories
                .iter()
                .map(|row| row.label.as_str())
                .collect::<Vec<_>>(),
            vec!["Recent repository", "SylvOps"]
        );
        assert_eq!(
            presentation
                .navigator
                .checkouts
                .iter()
                .map(|row| row.label.as_str())
                .collect::<Vec<_>>(),
            vec!["Primary", "Feature"]
        );
        assert_eq!(
            presentation
                .navigator
                .sessions
                .iter()
                .map(|row| row.label.as_str())
                .collect::<Vec<_>>(),
            vec!["Newest session", "Review issue 65"]
        );
    }

    #[test]
    fn navigator_rolls_urgent_session_state_up_to_ancestors() {
        let fixture = hierarchy_fixture(SessionState::NeedsFeedback);
        let preferences = DesktopState::default();
        let presentation = DesktopPresentation::build(&PresentationInput {
            daemon: &fixture.snapshot,
            preferences: &preferences,
            viewport: Viewport::new(1_440, 900),
            system_appearance: SystemAppearance::Dark,
            interaction: InteractionState {
                selected_project_id: Some(fixture.project_id),
                selected_worktree_id: Some(fixture.worktree_id),
                ..InteractionState::default()
            },
        });
        assert_eq!(
            presentation.navigator.repositories[0].attention,
            Some(NavigatorAttention::NeedsFeedback)
        );
        assert_eq!(
            presentation.navigator.checkouts[0].attention,
            Some(NavigatorAttention::NeedsFeedback)
        );
    }

    #[test]
    fn navigator_rows_publish_contextual_actions_without_hover() {
        let mut fixture = hierarchy_fixture(SessionState::Running);
        fixture.snapshot.worktrees[0].is_root_checkout = false;
        let preferences = DesktopState::default();
        let presentation = DesktopPresentation::build(&PresentationInput {
            daemon: &fixture.snapshot,
            preferences: &preferences,
            viewport: Viewport::new(1_440, 900),
            system_appearance: SystemAppearance::Dark,
            interaction: InteractionState {
                selected_project_id: Some(fixture.project_id),
                selected_worktree_id: Some(fixture.worktree_id),
                ..InteractionState::default()
            },
        });

        assert!(presentation.navigator.repositories[0].actions.is_empty());
        assert_eq!(
            presentation.navigator.checkouts[0].actions,
            vec![NavigatorAction::DeleteCheckout(fixture.worktree_id)]
        );
        assert!(presentation.navigator.sessions[0].actions.is_empty());
    }

    #[test]
    fn responsive_shell_keeps_workspace_and_active_session_context_explicit() {
        let fixture = hierarchy_fixture(SessionState::Running);
        let preferences = DesktopState::default();
        let present = |width| {
            DesktopPresentation::build(&PresentationInput {
                daemon: &fixture.snapshot,
                preferences: &preferences,
                viewport: Viewport::new(width, 700),
                system_appearance: SystemAppearance::Dark,
                interaction: InteractionState {
                    selected_project_id: Some(fixture.project_id),
                    selected_worktree_id: Some(fixture.worktree_id),
                    selected_session_id: Some(fixture.session_id),
                    active_session_id: Some(fixture.session_id),
                    open_session_ids: vec![fixture.session_id],
                    active_session_attached: true,
                    main_tab: MainTab::Changes,
                },
            })
        };

        let wide = present(1_180);
        assert_eq!(wide.navigator.mode, NavigatorMode::Columns);
        assert_eq!(
            wide.workspace_navigation,
            WorkspaceNavigation::ScrollableTabs
        );
        let compact_edge = present(1_179);
        assert_eq!(compact_edge.navigator.mode, NavigatorMode::Tabs);
        let compact = present(900);
        assert_eq!(compact.navigator.mode, NavigatorMode::Tabs);
        assert_eq!(
            compact.workspace_navigation,
            WorkspaceNavigation::ScrollableTabs
        );
        let narrow = present(819);
        assert_eq!(narrow.navigator.mode, NavigatorMode::Drawer);
        assert_eq!(
            narrow.workspace_navigation,
            WorkspaceNavigation::LabeledSwitcher
        );
        let compact_lower_edge = present(820);
        assert_eq!(compact_lower_edge.navigator.mode, NavigatorMode::Tabs);
        assert_eq!(
            compact_lower_edge.workspace_navigation,
            WorkspaceNavigation::ScrollableTabs
        );

        assert_eq!(wide.active_session.tabs.len(), 1);
        let active = wide
            .active_session
            .context
            .as_ref()
            .expect("active session context");
        assert_eq!(active.repository, "SylvOps");
        assert_eq!(active.checkout, "Primary");
        assert_eq!(active.branch, "main");
        assert_eq!(active.session, "Review issue 65");
        assert_eq!(active.state, "Running");
        assert_eq!(active.terminal_action, Some(ActiveTerminalAction::Leave));
        assert!(active.can_stop);
        assert_eq!(wide.selection.main_tab, MainTab::Changes);
    }
}
