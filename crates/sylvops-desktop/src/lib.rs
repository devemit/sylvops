//! Native, mouse-first `SylvOps` client backed exclusively by authenticated daemon IPC.

mod bridge;
mod forms;
mod state;
mod terminal;
mod theme;

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use bridge::{Bridge, BridgeEvent, Operation};
use forms::{Confirmation, FormModal, Modal};
use iced::{
    Background, Border, Center, Color, Element, Fill, Font, Length, Subscription, Task, Theme,
    alignment::Vertical,
    keyboard,
    keyboard::{Key, key::Named},
    system, time,
    widget::{
        button, column, container, mouse_area, opaque, pane_grid, row, rule, scrollable, slider,
        space, stack, text, text_input,
    },
    window,
};
use iced::{font::Weight, widget::button::Status};
use sylvops_core::{
    domain::{
        AttachmentRole, DaemonSnapshot, Project, Session, SessionState, Workspace, Worktree,
        WorktreeStatus,
    },
    ids::{ProjectId, SessionId, WorkspaceId, WorktreeId},
    protocol::{ClientRequest, DaemonEvent, DaemonResponse},
    provider::ProviderHealth,
    ui::{
        DesktopDensity, DesktopPanel, DesktopState, DesktopTheme, MAX_OPEN_DESKTOP_SESSIONS,
        MAX_TERMINAL_FONT_SIZE, MIN_DESKTOP_HEIGHT, MIN_DESKTOP_WIDTH, MIN_TERMINAL_FONT_SIZE,
        MainTab,
    },
    ui_forms::{Form, FormKind},
};
use sylvops_daemon::runtime::RuntimePaths;
use terminal::{
    TerminalState, display_contents as terminal_display_contents, encode_key as encode_terminal_key,
};

const EVENT_TICK: Duration = Duration::from_millis(16);
const SAVE_DEBOUNCE: Duration = Duration::from_millis(500);
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(75);
const SUCCESS_DURATION: Duration = Duration::from_secs(4);
const MAX_EVENTS_PER_TICK: usize = 512;
const NAV_HEIGHT: f32 = 40.0;
const FOOTER_HEIGHT: f32 = 28.0;
const SESSION_TAB_HEIGHT: f32 = 40.0;
const PANEL_HEADER_HEIGHT: f32 = 40.0;
const UI_TEXT_SIZE: f32 = 13.0;
const UI_META_SIZE: f32 = 11.0;
const ACTION_HEIGHT: f32 = 32.0;
#[cfg(windows)]
const UI_FONT: Font = Font::with_name("Segoe UI");
#[cfg(target_os = "macos")]
const UI_FONT: Font = Font::with_name("SF Pro Text");
#[cfg(all(not(windows), not(target_os = "macos")))]
const UI_FONT: Font = Font::DEFAULT;
#[cfg(windows)]
const TERMINAL_FONT: Font = Font::with_name("Consolas");
#[cfg(target_os = "macos")]
const TERMINAL_FONT: Font = Font::with_name("Menlo");
#[cfg(all(not(windows), not(target_os = "macos")))]
const TERMINAL_FONT: Font = Font::MONOSPACE;
const UI_MEDIUM: Font = Font {
    weight: Weight::Medium,
    ..UI_FONT
};
const UI_SEMIBOLD: Font = Font {
    weight: Weight::Semibold,
    ..UI_FONT
};

/// Opens the native `SylvOps` desktop client.
///
/// # Errors
///
/// Returns an error when the native window or renderer cannot be initialized.
pub fn run(paths: RuntimePaths) -> iced::Result {
    iced::application(
        move || {
            (
                DesktopApp::new(paths.clone()),
                system::theme().map(Message::SystemThemeChanged),
            )
        },
        DesktopApp::update,
        DesktopApp::view,
    )
    .title("SylvOps")
    .theme(DesktopApp::theme)
    .subscription(subscription)
    .default_font(UI_FONT)
    .antialiasing(true)
    .window(iced::window::Settings {
        size: iced::Size::new(1_440.0, 900.0),
        min_size: Some(iced::Size::new(
            f32::from(MIN_DESKTOP_WIDTH),
            f32::from(MIN_DESKTOP_HEIGHT),
        )),
        exit_on_close_request: false,
        ..iced::window::Settings::default()
    })
    .run()
}

struct DesktopApp {
    bridge: Bridge,
    snapshot: DaemonSnapshot,
    providers: Vec<ProviderHealth>,
    connection: ConnectionState,
    selected_project_id: Option<ProjectId>,
    selected_worktree_id: Option<WorktreeId>,
    selected_session_id: Option<SessionId>,
    open_sessions: Vec<SessionId>,
    active_session_id: Option<SessionId>,
    main_tab: MainTab,
    terminals: HashMap<SessionId, TerminalState>,
    diff: Option<String>,
    error: Option<String>,
    snapshot_pending: bool,
    modal: Option<Modal>,
    desktop_state: DesktopState,
    system_theme: iced::theme::Mode,
    state_dirty_at: Option<Instant>,
    state_save_pending: bool,
    success: Option<(String, Instant)>,
    window_id: Option<window::Id>,
    closing_since: Option<Instant>,
    pending_resize: Option<(SessionId, u16, u16, Instant)>,
    panes: pane_grid::State<DesktopPane>,
    pane_splits: [pane_grid::Split; 3],
    restore_window_size: Option<(u16, u16)>,
    narrow_main: bool,
    keyboard_panel: DesktopPanel,
    terminal_focus: TerminalFocus,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DesktopPane {
    Projects,
    Worktrees,
    Sessions,
    Main,
}

#[derive(Clone, Debug)]
enum ConnectionState {
    Connecting,
    Connected,
    Disconnected,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum TerminalFocus {
    #[default]
    Unfocused,
    Focused,
}

impl TerminalFocus {
    const fn is_focused(self) -> bool {
        matches!(self, Self::Focused)
    }
}

#[derive(Clone, Debug)]
enum Message {
    Tick,
    Keyboard(keyboard::Event),
    SelectWorkspace(WorkspaceId),
    SelectProject(ProjectId),
    SelectWorktree(WorktreeId),
    SelectSession(SessionId),
    SelectOpenSession(SessionId),
    CloseSessionTab(SessionId),
    SelectMainTab(MainTab),
    NewWorkspace,
    NewProject,
    NewWorktree,
    NewSession,
    RenameProject,
    RenameWorktree,
    RenameSession,
    RemoveSelectedWorktree,
    FormInput(usize, String),
    PreviousProvider,
    NextProvider,
    SubmitForm,
    CancelModal,
    BrowseRepository,
    FolderPicked(Result<Option<String>, String>),
    ConfirmAction,
    Attach,
    Detach,
    FocusTerminal,
    Stop,
    Refresh,
    ToggleSettings,
    SelectTheme(DesktopTheme),
    SelectDensity(DesktopDensity),
    SetTerminalFontSize(u8),
    ResetLayout,
    SelectCompactPanel(DesktopPanel),
    WindowResized(window::Id, iced::Size),
    SystemThemeChanged(iced::theme::Mode),
    CloseRequested(window::Id),
    PaneResized(pane_grid::ResizeEvent),
    ShowNarrowNavigator,
    ShowNarrowMain,
    ClearError,
}

impl DesktopApp {
    fn new(paths: RuntimePaths) -> Self {
        let (mut panes, projects) = pane_grid::State::new(DesktopPane::Projects);
        let (worktrees, first) = panes
            .split(pane_grid::Axis::Vertical, projects, DesktopPane::Worktrees)
            .expect("initial desktop pane split");
        let (sessions, second) = panes
            .split(pane_grid::Axis::Vertical, worktrees, DesktopPane::Sessions)
            .expect("initial desktop pane split");
        let (_, third) = panes
            .split(pane_grid::Axis::Vertical, sessions, DesktopPane::Main)
            .expect("initial desktop pane split");
        let defaults = DesktopState::default();
        panes.resize(first, f32::from(defaults.panel_ratios[0]) / 1000.0);
        panes.resize(second, f32::from(defaults.panel_ratios[1]) / 1000.0);
        panes.resize(third, f32::from(defaults.panel_ratios[2]) / 1000.0);
        Self {
            bridge: Bridge::spawn(paths),
            snapshot: DaemonSnapshot::default(),
            providers: Vec::new(),
            connection: ConnectionState::Connecting,
            selected_project_id: None,
            selected_worktree_id: None,
            selected_session_id: None,
            open_sessions: Vec::new(),
            active_session_id: None,
            main_tab: MainTab::Terminal,
            terminals: HashMap::new(),
            diff: None,
            error: None,
            snapshot_pending: false,
            modal: None,
            desktop_state: DesktopState::default(),
            system_theme: iced::theme::Mode::Dark,
            state_dirty_at: None,
            state_save_pending: false,
            success: None,
            window_id: None,
            closing_since: None,
            pending_resize: None,
            panes,
            pane_splits: [first, second, third],
            restore_window_size: None,
            narrow_main: false,
            keyboard_panel: DesktopPanel::Projects,
            terminal_focus: TerminalFocus::Unfocused,
        }
    }

    #[allow(clippy::too_many_lines)]
    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Tick => {
                self.process_bridge_events();
                self.flush_timers();
                if let (Some((width, height)), Some(window_id)) =
                    (self.restore_window_size, self.window_id)
                {
                    self.restore_window_size = None;
                    return window::resize(
                        window_id,
                        iced::Size::new(f32::from(width), f32::from(height)),
                    );
                }
                if let (Some(started), Some(window_id)) = (self.closing_since, self.window_id)
                    && (!self.state_save_pending || started.elapsed() >= Duration::from_secs(1))
                {
                    return window::close(window_id);
                }
            }
            Message::Keyboard(event) => self.handle_keyboard(event),
            Message::SelectWorkspace(workspace_id) => {
                self.terminal_focus = TerminalFocus::Unfocused;
                self.send_request(
                    Operation::OpenWorkspace(workspace_id),
                    ClientRequest::OpenWorkspace { workspace_id },
                );
            }
            Message::SelectProject(project_id) => self.select_project(project_id),
            Message::SelectWorktree(worktree_id) => self.select_worktree(worktree_id),
            Message::SelectSession(session_id) => self.select_session(session_id),
            Message::SelectOpenSession(session_id) => {
                self.keyboard_panel = DesktopPanel::Sessions;
                self.terminal_focus = TerminalFocus::Unfocused;
                self.select_session_context(session_id);
                self.active_session_id = Some(session_id);
                self.mark_state_dirty();
            }
            Message::CloseSessionTab(session_id) => {
                self.terminal_focus = TerminalFocus::Unfocused;
                if self
                    .terminals
                    .get(&session_id)
                    .is_some_and(|terminal| terminal.attached)
                {
                    self.send_request(
                        Operation::Detach(session_id),
                        ClientRequest::DetachSession { session_id },
                    );
                }
                self.open_sessions.retain(|id| *id != session_id);
                self.terminals.remove(&session_id);
                if self.active_session_id == Some(session_id) {
                    self.active_session_id = self.open_sessions.last().copied();
                    if let Some(next) = self.active_session_id {
                        self.select_session_context(next);
                    }
                }
                self.mark_state_dirty();
            }
            Message::SelectMainTab(tab) => self.select_main_tab(tab),
            Message::NewWorkspace => {
                self.terminal_focus = TerminalFocus::Unfocused;
                self.modal = Some(Modal::Form(FormModal::new(Form::workspace())));
            }
            Message::NewProject => self.open_project_form(),
            Message::NewWorktree => self.open_worktree_form(),
            Message::NewSession => self.open_session_form(),
            Message::RenameProject => self.open_project_rename_form(),
            Message::RenameWorktree => self.open_worktree_rename_form(),
            Message::RenameSession => self.open_session_rename_form(),
            Message::RemoveSelectedWorktree => self.inspect_worktree_removal(),
            Message::FormInput(index, value) => self.update_form_field(index, value),
            Message::PreviousProvider => self.change_provider(-1),
            Message::NextProvider => self.change_provider(1),
            Message::SubmitForm => self.submit_form(),
            Message::CancelModal => self.modal = None,
            Message::BrowseRepository => {
                return Task::perform(pick_repository_folder(), Message::FolderPicked);
            }
            Message::FolderPicked(result) => self.apply_picked_folder(result),
            Message::ConfirmAction => self.confirm_action(),
            Message::Attach => self.attach_active(),
            Message::Detach => self.detach_active(),
            Message::FocusTerminal => self.focus_terminal(),
            Message::Stop => self.open_stop_confirmation(),
            Message::Refresh => self.request_snapshot(),
            Message::ToggleSettings => {
                self.terminal_focus = TerminalFocus::Unfocused;
                self.modal = if matches!(self.modal, Some(Modal::Settings)) {
                    None
                } else {
                    Some(Modal::Settings)
                };
            }
            Message::SelectTheme(theme) => {
                self.desktop_state.theme = theme;
                self.mark_state_dirty();
            }
            Message::SelectDensity(density) => {
                self.desktop_state.density = density;
                self.mark_state_dirty();
            }
            Message::SetTerminalFontSize(size) => {
                self.desktop_state.terminal_font_size =
                    size.clamp(MIN_TERMINAL_FONT_SIZE, MAX_TERMINAL_FONT_SIZE);
                self.mark_state_dirty();
                self.queue_terminal_resize();
            }
            Message::ResetLayout => {
                state::reset_layout(&mut self.desktop_state);
                for (index, split) in self.pane_splits.iter().copied().enumerate() {
                    self.panes.resize(
                        split,
                        f32::from(self.desktop_state.panel_ratios[index]) / 1000.0,
                    );
                }
                self.mark_state_dirty();
                self.queue_terminal_resize();
            }
            Message::SelectCompactPanel(panel) => {
                self.keyboard_panel = panel;
                self.terminal_focus = TerminalFocus::Unfocused;
                self.desktop_state.compact_panel = panel;
                self.mark_state_dirty();
            }
            Message::WindowResized(id, size) => self.window_resized(id, size),
            Message::SystemThemeChanged(mode) => self.system_theme = mode,
            Message::CloseRequested(id) => {
                self.window_id = Some(id);
                self.begin_close();
            }
            Message::PaneResized(event) => {
                if let Some(index) = self
                    .pane_splits
                    .iter()
                    .position(|split| *split == event.split)
                {
                    let mut ratios = self.desktop_state.panel_ratios;
                    ratios[index] = bounded_u16(event.ratio * 1000.0, 100, 350);
                    if state::panel_ratios_fit(self.desktop_state.window_width, ratios) {
                        self.panes.resize(event.split, event.ratio);
                        self.desktop_state.panel_ratios = ratios;
                        self.mark_state_dirty();
                        self.queue_terminal_resize();
                    }
                }
            }
            Message::ShowNarrowNavigator => {
                self.terminal_focus = TerminalFocus::Unfocused;
                self.narrow_main = false;
            }
            Message::ShowNarrowMain => {
                self.terminal_focus = TerminalFocus::Unfocused;
                self.narrow_main = true;
            }
            Message::ClearError => self.error = None,
        }
        Task::none()
    }

    fn theme(&self) -> Theme {
        theme::resolve(self.desktop_state.theme, self.system_theme)
    }

    fn view(&self) -> Element<'_, Message> {
        let top = self.top_bar();
        let mission_control = self.mission_control();
        let body: Element<'_, Message> = if let Some(modal) = &self.modal {
            let overlay = container(self.modal_view(modal))
                .width(Fill)
                .height(Fill)
                .center_x(Fill)
                .center_y(Fill)
                .style(modal_scrim);
            let overlay: Element<'_, Message> = if matches!(modal, Modal::Settings) {
                mouse_area(overlay).on_press(Message::ToggleSettings).into()
            } else {
                overlay.into()
            };
            stack([mission_control, opaque(overlay)]).into()
        } else {
            mission_control
        };
        let footer = self.footer();
        let mut content = column![top, rule::horizontal(1), body, rule::horizontal(1), footer]
            .width(Fill)
            .height(Fill);
        if let Some(error) = &self.error {
            content = content.push(
                container(
                    row![
                        text(error).style(text::danger),
                        space::horizontal(),
                        button("Dismiss").on_press(Message::ClearError)
                    ]
                    .align_y(Center),
                )
                .padding([8, 12]),
            );
        }
        if let Some((message, _)) = &self.success {
            content = content.push(
                container(text(message).style(text::success))
                    .padding([6, 12])
                    .width(Fill),
            );
        }
        container(content).width(Fill).height(Fill).into()
    }

    fn top_bar(&self) -> Element<'_, Message> {
        let worktree_context = self.selected_worktree().map_or_else(
            || "Worktree: none selected".to_owned(),
            |worktree| {
                format!(
                    "Worktree: {}  ·  {}",
                    worktree.name,
                    worktree.branch.as_deref().unwrap_or("detached")
                )
            },
        );
        let brand = container(
            row![
                text("✦").size(16).style(text::primary),
                text("SylvOps").font(UI_SEMIBOLD).size(15)
            ]
            .spacing(7)
            .align_y(Center),
        )
        .width(Length::Fixed(170.0))
        .padding([0, 8]);
        let mut workspaces = row![brand].spacing(4).align_y(Center);
        for workspace in &self.snapshot.workspaces {
            let label = workspace.name.clone();
            let active = workspace.is_open;
            let action = button(text(label).font(UI_MEDIUM).size(UI_TEXT_SIZE))
                .on_press(Message::SelectWorkspace(workspace.id))
                .height(ACTION_HEIGHT)
                .padding([6, 12])
                .style(move |theme, status| workspace_tab_style(theme, status, active));
            workspaces = workspaces.push(action);
        }
        workspaces = workspaces
            .push(
                button(text("+").font(UI_MEDIUM).size(16))
                    .on_press(Message::NewWorkspace)
                    .height(ACTION_HEIGHT)
                    .padding([5, 10])
                    .style(chrome_action_style),
            )
            .push(space::horizontal())
            .push(
                container(text(worktree_context).font(UI_MEDIUM).size(UI_META_SIZE))
                    .height(ACTION_HEIGHT)
                    .padding([7, 11])
                    .align_y(Vertical::Center)
                    .style(selected_context_style),
            )
            .push(
                button(text("↻").size(17))
                    .on_press(Message::Refresh)
                    .height(ACTION_HEIGHT)
                    .padding([5, 10])
                    .style(chrome_action_style),
            )
            .push(
                button(text("Settings").font(UI_MEDIUM).size(12))
                    .on_press(Message::ToggleSettings)
                    .height(ACTION_HEIGHT)
                    .padding([6, 11])
                    .style(chrome_action_style),
            );
        container(workspaces)
            .height(NAV_HEIGHT)
            .width(Fill)
            .padding([4, 8])
            .align_y(Vertical::Center)
            .style(chrome_surface)
            .into()
    }

    fn mission_control(&self) -> Element<'_, Message> {
        match state::layout_mode(self.desktop_state.window_width) {
            state::LayoutMode::Wide => pane_grid(&self.panes, |_pane, kind, _maximized| {
                let content = match kind {
                    DesktopPane::Projects => self.projects_column(),
                    DesktopPane::Worktrees => self.worktrees_column(),
                    DesktopPane::Sessions => self.sessions_column(),
                    DesktopPane::Main => self.workspace_view(),
                };
                pane_grid::Content::new(content)
            })
            .spacing(1)
            .min_size(150)
            .on_resize(8, Message::PaneResized)
            .into(),
            state::LayoutMode::Compact => row![
                self.compact_navigator(),
                rule::vertical(1),
                self.workspace_view(),
            ]
            .width(Fill)
            .height(Fill)
            .into(),
            state::LayoutMode::Narrow => {
                let toggle = row![
                    button("Navigator")
                        .on_press(Message::ShowNarrowNavigator)
                        .style(if self.narrow_main {
                            button::secondary
                        } else {
                            button::primary
                        }),
                    button("Main")
                        .on_press(Message::ShowNarrowMain)
                        .style(if self.narrow_main {
                            button::primary
                        } else {
                            button::secondary
                        }),
                ]
                .spacing(4);
                let content = if self.narrow_main {
                    self.workspace_view()
                } else {
                    self.compact_navigator()
                };
                column![container(toggle).padding([4, 6]), content]
                    .width(Fill)
                    .height(Fill)
                    .into()
            }
        }
    }

    fn projects_column(&self) -> Element<'_, Message> {
        let mut items = column![column_heading("Projects", Some(Message::NewProject))].spacing(4);
        for project in self.visible_projects() {
            items = items.push(select_button(
                &project.name,
                self.selected_project_id == Some(project.id),
                Message::SelectProject(project.id),
                self.desktop_state.density,
            ));
        }
        if self.visible_projects().is_empty() {
            items = items.push(empty_hint("No repositories in this workspace."));
        }
        panel(scrollable(items), 190.0)
    }

    fn worktrees_column(&self) -> Element<'_, Message> {
        let create = self.selected_project_id.map(|_| Message::NewWorktree);
        let mut items = column![column_heading("Worktrees", create)].spacing(4);
        if let Some(project_id) = self.selected_project_id {
            for worktree in self.worktrees_for(project_id) {
                let branch = worktree.branch.as_deref().unwrap_or("detached");
                let label = if worktree.is_root_checkout {
                    format!("{}  ·  {branch}  ·  root", worktree.name)
                } else {
                    format!("{}  ·  {branch}", worktree.name)
                };
                items = items.push(select_button(
                    &label,
                    self.selected_worktree_id == Some(worktree.id),
                    Message::SelectWorktree(worktree.id),
                    self.desktop_state.density,
                ));
            }
        }
        if self.selected_project_id.is_none() {
            items = items.push(empty_hint("Select a project."));
        }
        panel(scrollable(items), 225.0)
    }

    fn sessions_column(&self) -> Element<'_, Message> {
        let create = self.selected_worktree_id.map(|_| Message::NewSession);
        let mut items = column![column_heading("Sessions", create)].spacing(4);
        if let Some(worktree_id) = self.selected_worktree_id {
            for session in self.sessions_for(worktree_id) {
                let label = format!(
                    "{}  ·  {} {}",
                    status_symbol(session.state),
                    session.display_name,
                    session.state
                );
                items = items.push(select_button(
                    &label,
                    self.selected_session_id == Some(session.id),
                    Message::SelectSession(session.id),
                    self.desktop_state.density,
                ));
            }
        }
        if self.selected_worktree_id.is_none() {
            items = items.push(empty_hint("Select a worktree."));
        }
        panel(scrollable(items), 255.0)
    }

    fn compact_navigator(&self) -> Element<'_, Message> {
        let tabs = row![
            compact_panel_button(
                "Projects",
                DesktopPanel::Projects,
                self.desktop_state.compact_panel
            ),
            compact_panel_button(
                "Worktrees",
                DesktopPanel::Worktrees,
                self.desktop_state.compact_panel
            ),
            compact_panel_button(
                "Sessions",
                DesktopPanel::Sessions,
                self.desktop_state.compact_panel
            ),
        ]
        .spacing(3);
        let panel = match self.desktop_state.compact_panel {
            DesktopPanel::Projects => self.projects_column(),
            DesktopPanel::Worktrees => self.worktrees_column(),
            DesktopPanel::Sessions => self.sessions_column(),
        };
        container(column![container(tabs).padding([4, 5]), panel])
            .width(
                if state::layout_mode(self.desktop_state.window_width) == state::LayoutMode::Compact
                {
                    Length::Fixed(280.0)
                } else {
                    Fill
                },
            )
            .height(Fill)
            .into()
    }

    fn workspace_view(&self) -> Element<'_, Message> {
        let tabs = self.session_tabs();
        let views = container(
            row![
                tab_button("Terminal", MainTab::Terminal, self.main_tab),
                tab_button("Changes", MainTab::Changes, self.main_tab),
                tab_button("Details", MainTab::Details, self.main_tab),
                space::horizontal(),
                self.session_actions(),
            ]
            .spacing(4)
            .align_y(Center),
        )
        .height(42)
        .padding([4, 8])
        .width(Fill)
        .style(tab_strip_surface);
        let content = match self.main_tab {
            MainTab::Terminal => self.terminal_view(),
            MainTab::Changes => self.changes_view(),
            MainTab::Details => self.details_view(),
        };
        container(column![
            tabs,
            rule::horizontal(1),
            views,
            rule::horizontal(1),
            content
        ])
        .width(Fill)
        .height(Fill)
        .into()
    }

    fn session_tabs(&self) -> Element<'_, Message> {
        let mut tabs = row![].spacing(4).align_y(Center);
        for session_id in &self.open_sessions {
            if let Some(session) = self.session(*session_id) {
                let label = format!("{} {}", status_symbol(session.state), session.display_name);
                tabs = tabs
                    .push(
                        button(text(label).font(UI_MEDIUM).size(UI_TEXT_SIZE))
                            .on_press(Message::SelectOpenSession(session.id))
                            .height(ACTION_HEIGHT)
                            .padding([6, 10])
                            .style(move |theme, status| {
                                session_tab_style(
                                    theme,
                                    status,
                                    self.active_session_id == Some(session.id),
                                )
                            }),
                    )
                    .push(
                        button("×")
                            .on_press(Message::CloseSessionTab(session.id))
                            .height(ACTION_HEIGHT)
                            .padding([5, 8])
                            .style(chrome_action_style),
                    );
            }
        }
        if self.open_sessions.is_empty() {
            tabs = tabs.push(text("Select a session to open it").style(text::secondary));
        }
        container(tabs)
            .height(SESSION_TAB_HEIGHT)
            .padding([3, 8])
            .width(Fill)
            .style(tab_strip_surface)
            .into()
    }

    fn session_actions(&self) -> Element<'_, Message> {
        let Some(session_id) = self.active_session_id else {
            return text("No session selected").style(text::secondary).into();
        };
        let attached = self
            .terminals
            .get(&session_id)
            .is_some_and(|terminal| terminal.attached);
        let mut actions = row![].spacing(6);
        if attached {
            actions = actions.push(
                button(text("Detach").font(UI_MEDIUM).size(12))
                    .on_press(Message::Detach)
                    .height(ACTION_HEIGHT)
                    .padding([6, 10])
                    .style(chrome_action_style),
            );
        } else {
            actions = actions.push(
                button(text("Attach").font(UI_MEDIUM).size(12))
                    .on_press(Message::Attach)
                    .height(ACTION_HEIGHT)
                    .padding([6, 10])
                    .style(button::primary),
            );
        }
        actions
            .push(
                button(text("Stop").font(UI_MEDIUM).size(12))
                    .on_press(Message::Stop)
                    .height(ACTION_HEIGHT)
                    .padding([6, 10])
                    .style(button::danger),
            )
            .into()
    }

    fn terminal_view(&self) -> Element<'_, Message> {
        let Some(session_id) = self.active_session_id else {
            return centered_message("Choose a session, then click Attach.");
        };
        let Some(terminal) = self.terminals.get(&session_id) else {
            return centered_message("Session selected. Click Attach to load its terminal.");
        };
        let role = if !terminal.attached {
            "Detached  ·  click Attach to reconnect".to_owned()
        } else if terminal.role == AttachmentRole::Observer {
            "Read-only observer  ·  another client controls input".to_owned()
        } else if self.terminal_focus.is_focused() {
            "Controller  ·  keyboard active  ·  Ctrl+] detaches".to_owned()
        } else {
            "Controller  ·  click anywhere in the terminal to type".to_owned()
        };
        let contents = terminal_display_contents(terminal, self.terminal_focus.is_focused());
        let surface = container(
            column![
                container(
                    text(role)
                        .font(UI_MEDIUM)
                        .size(UI_META_SIZE)
                        .style(text::secondary)
                )
                .height(26)
                .align_y(Vertical::Center),
                scrollable(
                    text(contents)
                        .font(TERMINAL_FONT)
                        .size(u32::from(self.desktop_state.terminal_font_size))
                        .width(Fill)
                )
                .height(Fill),
            ]
            .spacing(4),
        )
        .padding([8, 12])
        .width(Fill)
        .height(Fill)
        .style(move |theme| terminal_surface(theme, self.terminal_focus.is_focused()));
        mouse_area(surface).on_press(Message::FocusTerminal).into()
    }

    fn changes_view(&self) -> Element<'_, Message> {
        let body = self.diff.as_deref().unwrap_or(
            "Select a worktree and open Changes. The daemon will load a bounded, read-only diff.",
        );
        container(scrollable(
            text(body)
                .font(TERMINAL_FONT)
                .size(u32::from(self.desktop_state.terminal_font_size)),
        ))
        .padding(14)
        .width(Fill)
        .height(Fill)
        .style(workspace_surface)
        .into()
    }

    fn details_view(&self) -> Element<'_, Message> {
        let mut content = column![text("Selection details").font(UI_SEMIBOLD).size(24)].spacing(12);
        if let Some(project) = self
            .selected_project_id
            .and_then(|id| self.snapshot.projects.iter().find(|item| item.id == id))
        {
            content = content
                .push(detail("Project", project.name.clone()))
                .push(detail(
                    "Repository",
                    project.canonical_repository_path.clone(),
                ));
        }
        if let Some(worktree) = self.selected_worktree() {
            content = content
                .push(detail("Worktree", worktree.name.clone()))
                .push(detail(
                    "Branch",
                    worktree.branch.clone().unwrap_or_else(|| "Detached".into()),
                ))
                .push(detail("Path", worktree.canonical_path.clone()));
        }
        if let Some(session) = self.active_session_id.and_then(|id| self.session(id)) {
            content = content
                .push(rule::horizontal(1))
                .push(text(&session.display_name).font(UI_SEMIBOLD).size(18))
                .push(detail("Provider", session.provider_kind.to_string()))
                .push(detail("State", session.state.to_string()))
                .push(detail(
                    "Controller",
                    self.terminals
                        .get(&session.id)
                        .map_or_else(|| "Detached".into(), |terminal| terminal.role.to_string()),
                ))
                .push(detail(
                    "Resume",
                    if session.external_session_id.is_some() {
                        "Available"
                    } else {
                        "Unavailable"
                    }
                    .into(),
                ));
        }
        let mut actions = row![].spacing(8);
        if self.selected_project_id.is_some() {
            actions = actions.push(button("Rename project").on_press(Message::RenameProject));
        }
        if self.selected_worktree_id.is_some() {
            actions = actions.push(button("Rename worktree").on_press(Message::RenameWorktree));
        }
        if self.active_session_id.is_some() {
            actions = actions
                .push(button("Rename session").on_press(Message::RenameSession))
                .push(
                    button("Stop session")
                        .on_press(Message::Stop)
                        .style(button::danger),
                );
        }
        if self
            .selected_worktree()
            .is_some_and(|worktree| !worktree.is_root_checkout)
        {
            actions = actions.push(
                button("Remove worktree")
                    .on_press(Message::RemoveSelectedWorktree)
                    .style(button::danger),
            );
        }
        content = content.push(rule::horizontal(1)).push(actions);
        container(content)
            .padding(20)
            .width(Fill)
            .height(Fill)
            .style(workspace_surface)
            .into()
    }

    fn modal_view<'a>(&'a self, modal: &'a Modal) -> Element<'a, Message> {
        match modal {
            Modal::Settings => self.settings_view(),
            Modal::Shortcuts => Self::shortcuts_view(),
            Modal::Form(form) => Self::form_view(form),
            Modal::Confirmation(confirmation) => Self::confirmation_view(confirmation),
        }
    }

    fn shortcuts_view() -> Element<'static, Message> {
        container(
            column![
                row![
                    text("Keyboard shortcuts").font(UI_SEMIBOLD).size(22),
                    space::horizontal(),
                    button("Done")
                        .on_press(Message::CancelModal)
                        .height(ACTION_HEIGHT)
                        .padding([6, 12])
                ]
                .align_y(Center),
                detail("Tab / Shift+Tab", "Change navigator panel".into()),
                detail("↑ / ↓", "Move through the active panel".into()),
                detail("Enter", "Open the selected session".into()),
                detail("1 / 2 / 3", "Terminal / Changes / Details".into()),
                detail("N / R / D", "New / rename / stop or remove".into()),
                detail("A / G", "Attach terminal / show Git changes".into()),
                detail("Ctrl+]", "Detach the focused terminal".into()),
                text("Shortcuts are paused while a form is open. When the terminal says “keyboard active”, ordinary keys go to the running process.")
                    .style(text::secondary),
            ]
            .spacing(12),
        )
        .padding(22)
        .width(Length::Fixed(540.0))
        .style(modal_card)
        .into()
    }

    fn settings_view(&self) -> Element<'_, Message> {
        let mut themes = row![].spacing(8);
        for choice in [
            DesktopTheme::System,
            DesktopTheme::Light,
            DesktopTheme::Dark,
            DesktopTheme::Nord,
            DesktopTheme::TokyoNight,
            DesktopTheme::Catppuccin,
        ] {
            themes = themes.push(
                button(theme::label(choice))
                    .on_press(Message::SelectTheme(choice))
                    .style(if self.desktop_state.theme == choice {
                        button::primary
                    } else {
                        button::secondary
                    }),
            );
        }
        container(
            column![
                row![
                    text("Settings").font(UI_SEMIBOLD).size(24),
                    space::horizontal(),
                    button(text("Done").font(UI_MEDIUM).size(12))
                        .on_press(Message::ToggleSettings)
                        .height(ACTION_HEIGHT)
                        .padding([6, 10])
                        .style(chrome_action_style)
                ]
                .align_y(Center),
                text("Appearance").font(UI_SEMIBOLD).size(16),
                text("Theme changes apply immediately. SylvOps uses the platform UI font and native monospace terminal font.")
                    .style(text::secondary),
                themes,
                row![
                    text("Density").width(Length::Fixed(120.0)),
                    button("Comfortable")
                        .on_press(Message::SelectDensity(DesktopDensity::Comfortable))
                        .style(if self.desktop_state.density == DesktopDensity::Comfortable { button::primary } else { button::secondary }),
                    button("Compact")
                        .on_press(Message::SelectDensity(DesktopDensity::Compact))
                        .style(if self.desktop_state.density == DesktopDensity::Compact { button::primary } else { button::secondary }),
                ].spacing(8).align_y(Center),
                row![
                    text(format!("Terminal font: {} px", self.desktop_state.terminal_font_size)).width(Length::Fixed(180.0)),
                    slider(
                        MIN_TERMINAL_FONT_SIZE..=MAX_TERMINAL_FONT_SIZE,
                        self.desktop_state.terminal_font_size,
                        Message::SetTerminalFontSize,
                    ).width(Length::Fixed(260.0)),
                ].spacing(8).align_y(Center),
                button("Reset layout").on_press(Message::ResetLayout),
                rule::horizontal(1),
                text("Safety").font(UI_SEMIBOLD).size(16),
                text("The desktop remains an IPC client. The daemon still owns PTYs, Git mutations, process cleanup, and audit events.")
                    .style(text::secondary),
            ]
            .spacing(16),
        )
        .padding(22)
        .width(Length::Fixed(640.0))
        .style(modal_card)
        .into()
    }

    fn form_view(modal: &FormModal) -> Element<'_, Message> {
        let form = &modal.form;
        let mut fields = column![].spacing(10);
        if form.is_session() {
            let provider = form.provider();
            let provider_text = provider.map_or_else(
                || "No providers".into(),
                |item| {
                    let status = if !item.available {
                        "unavailable"
                    } else if item.kind == sylvops_core::domain::ProviderKind::Codex
                        && !item.authenticated
                    {
                        "available, login required"
                    } else {
                        "ready"
                    };
                    format!("{} — {}", item.kind, status)
                },
            );
            fields = fields.push(
                row![
                    button("‹")
                        .on_press_maybe((!modal.pending).then_some(Message::PreviousProvider)),
                    text(provider_text).width(Fill),
                    button("›").on_press_maybe((!modal.pending).then_some(Message::NextProvider)),
                ]
                .spacing(8)
                .align_y(Center),
            );
        }
        for (index, field) in form
            .fields
            .iter()
            .enumerate()
            .filter(|(_, field)| field.visible)
        {
            let input = text_input(field.label, &field.value)
                .on_input_maybe(
                    (!modal.pending).then_some(move |value| Message::FormInput(index, value)),
                )
                .on_submit(Message::SubmitForm)
                .padding(8);
            let mut group =
                column![text(field.label).font(UI_MEDIUM).size(UI_META_SIZE), input].spacing(4);
            if let Some(error) = &field.error {
                group = group.push(text(error).style(text::danger));
            }
            if matches!(form.kind, FormKind::RegisterProject(_)) && index == 0 {
                group = group.push(button("Choose folder…").on_press(Message::BrowseRepository));
            }
            fields = fields.push(group);
        }
        if let Some(error) = &form.submission_error {
            fields = fields.push(text(error).style(text::danger));
        }
        let submit = if modal.pending {
            "Working…"
        } else {
            "Continue"
        };
        container(
            column![
                text(&form.title).font(UI_SEMIBOLD).size(22),
                fields,
                row![
                    space::horizontal(),
                    button("Cancel").on_press(Message::CancelModal),
                    button(submit)
                        .on_press_maybe((!modal.pending).then_some(Message::SubmitForm))
                        .style(button::primary),
                ]
                .spacing(8),
            ]
            .spacing(16),
        )
        .padding(22)
        .width(Length::Fixed(560.0))
        .style(modal_card)
        .into()
    }

    fn confirmation_view(confirmation: &Confirmation) -> Element<'_, Message> {
        let (title, explanation) = match confirmation {
            Confirmation::StopSession { session_name, cwd } => (
                "Stop session",
                format!(
                    "Stop “{session_name}” in {cwd} and terminate its complete process tree? Its history remains available."
                ),
            ),
            Confirmation::RemoveWorktree {
                name,
                canonical_path,
                ..
            } => (
                "Remove clean worktree",
                format!("Remove “{name}” at {canonical_path}? The Git branch is preserved."),
            ),
        };
        container(
            column![
                text(title).font(UI_SEMIBOLD).size(22),
                text(explanation),
                row![
                    space::horizontal(),
                    button("Cancel").on_press(Message::CancelModal),
                    button("Confirm")
                        .on_press(Message::ConfirmAction)
                        .style(button::danger),
                ]
                .spacing(8),
            ]
            .spacing(16),
        )
        .padding(22)
        .width(Length::Fixed(520.0))
        .style(modal_card)
        .into()
    }

    fn footer(&self) -> Element<'_, Message> {
        let workspace = self
            .active_workspace()
            .map_or("No workspace", |workspace| workspace.name.as_str());
        let branch = self
            .selected_worktree()
            .and_then(|worktree| worktree.branch.as_deref())
            .unwrap_or("No branch");
        let connection = match self.connection {
            ConnectionState::Connecting => "Connecting",
            ConnectionState::Connected => "Connected",
            ConnectionState::Disconnected => "Disconnected",
        };
        container(
            row![
                text(format!("SylvOps {}", env!("CARGO_PKG_VERSION")))
                    .font(UI_MEDIUM)
                    .size(UI_META_SIZE),
                footer_item(workspace),
                footer_item(&format!("Worktree: {branch}")),
                footer_item(&format!("{:?}", self.main_tab)),
                footer_item(&format!("Keyboard: {:?}", self.keyboard_panel)),
                space::horizontal(),
                footer_item(connection),
                footer_item("Ctrl+K Shortcuts  ·  Ctrl+] Detach"),
            ]
            .spacing(12)
            .align_y(Center),
        )
        .height(FOOTER_HEIGHT)
        .padding([3, 10])
        .width(Fill)
        .style(chrome_surface)
        .into()
    }

    fn process_bridge_events(&mut self) {
        if self.bridge.take_overflowed() {
            self.request_snapshot();
            if let Some(session_id) = self.active_session_id {
                let (columns, rows) = self.terminal_dimensions();
                let from_sequence = self
                    .terminals
                    .get(&session_id)
                    .map_or(0, |terminal| terminal.last_sequence);
                self.send_request(
                    Operation::Attach(session_id),
                    ClientRequest::AttachSession {
                        session_id,
                        from_sequence,
                        columns,
                        rows,
                    },
                );
            }
        }
        for event in self.bridge.drain(MAX_EVENTS_PER_TICK) {
            self.handle_bridge_event(event);
        }
    }

    fn handle_bridge_event(&mut self, event: BridgeEvent) {
        match event {
            BridgeEvent::Connected {
                snapshot,
                providers,
                desktop_state,
            } => {
                self.snapshot = snapshot;
                self.providers = providers;
                self.connection = ConnectionState::Connected;
                if let Some(state) = desktop_state {
                    self.apply_desktop_state(state);
                }
                self.restore_selection();
                if self.snapshot.workspaces.is_empty() {
                    self.modal = Some(Modal::Form(FormModal::new(Form::workspace())));
                }
            }
            BridgeEvent::Daemon(event) => self.handle_daemon_event(event),
            BridgeEvent::Response {
                operation,
                response,
            } => {
                self.handle_response(operation, response);
            }
            BridgeEvent::Error { operation, message } => {
                if matches!(operation, Some(Operation::RefreshSnapshot)) {
                    self.snapshot_pending = false;
                }
                if matches!(operation, Some(Operation::SaveDesktopState)) {
                    self.state_save_pending = false;
                }
                let form_operation = matches!(
                    operation,
                    Some(
                        Operation::CreateWorkspace
                            | Operation::RegisterProject
                            | Operation::CreateWorktree
                            | Operation::CreateSession(_)
                            | Operation::RenameProject
                            | Operation::RenameWorktree
                            | Operation::RenameSession
                    )
                );
                if form_operation && let Some(Modal::Form(modal)) = &mut self.modal {
                    modal.pending = false;
                    modal.form.submission_error = Some(message);
                } else {
                    self.error = Some(message);
                }
            }
            BridgeEvent::Closed => {
                self.connection = ConnectionState::Disconnected;
                self.error = Some("The daemon connection closed. Sessions remain daemon-owned; reopen SylvOps or restart the daemon.".into());
            }
        }
    }

    fn handle_daemon_event(&mut self, event: DaemonEvent) {
        match event {
            DaemonEvent::SessionOutput {
                session_id,
                sequence,
                bytes,
                ..
            } => {
                if let Some(terminal) = self.terminals.get_mut(&session_id)
                    && sequence > terminal.last_sequence
                {
                    terminal.parser.process(&bytes);
                    terminal.last_sequence = sequence;
                }
            }
            DaemonEvent::ResynchronizationRequired {
                session_id,
                snapshot_sequence,
                columns,
                rows,
                terminal_snapshot,
            } => {
                if let Some(terminal) = self.terminals.get_mut(&session_id) {
                    terminal.parser = vt100::Parser::new(rows, columns, 10_000);
                    terminal.parser.process(&terminal_snapshot);
                    terminal.last_sequence = snapshot_sequence;
                    terminal.columns = columns;
                    terminal.rows = rows;
                }
            }
            _ => self.request_snapshot(),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn handle_response(&mut self, operation: Operation, response: DaemonResponse) {
        match (operation, response) {
            (Operation::RefreshSnapshot, DaemonResponse::Snapshot(snapshot)) => {
                self.snapshot_pending = false;
                self.snapshot = snapshot;
                self.restore_selection();
            }
            (
                Operation::OpenWorkspace(workspace_id),
                DaemonResponse::WorkspaceOpened { workspace, .. },
            ) => {
                self.snapshot.workspaces.iter_mut().for_each(|item| {
                    item.is_open = item.id == workspace_id;
                });
                if let Some(item) = self
                    .snapshot
                    .workspaces
                    .iter_mut()
                    .find(|item| item.id == workspace.id)
                {
                    *item = workspace;
                }
                self.restore_selection();
                self.request_snapshot();
            }
            (
                Operation::CreateSession(worktree_id),
                DaemonResponse::SessionCreated { session, .. },
            ) => {
                self.selected_worktree_id = Some(worktree_id);
                self.selected_session_id = Some(session.id);
                self.active_session_id = Some(session.id);
                if !self.open_sessions.contains(&session.id) {
                    self.open_sessions.push(session.id);
                }
                self.modal = None;
                self.show_success("Session created.");
                self.mark_state_dirty();
                self.request_snapshot();
            }
            (Operation::Stop(session_id), DaemonResponse::Acknowledged) => {
                if let Some(terminal) = self.terminals.get_mut(&session_id) {
                    terminal.attached = false;
                }
                if self.active_session_id == Some(session_id) {
                    self.terminal_focus = TerminalFocus::Unfocused;
                }
                self.show_success("Session stopped; its history remains available.");
                self.request_snapshot();
            }
            (
                Operation::Attach(session_id),
                DaemonResponse::Attached {
                    role,
                    replay_through_sequence,
                    terminal_snapshot,
                    ..
                },
            ) => {
                let (columns, rows) = self.terminal_dimensions();
                let terminal = self
                    .terminals
                    .entry(session_id)
                    .or_insert_with(|| TerminalState {
                        role,
                        parser: vt100::Parser::new(rows, columns, 10_000),
                        last_sequence: 0,
                        attached: true,
                        columns,
                        rows,
                    });
                terminal.role = role;
                terminal.attached = true;
                terminal.columns = columns;
                terminal.rows = rows;
                if let Some(snapshot) = terminal_snapshot {
                    terminal.parser = vt100::Parser::new(rows, columns, 10_000);
                    terminal.parser.process(&snapshot);
                    terminal.last_sequence = replay_through_sequence;
                }
                self.main_tab = MainTab::Terminal;
                self.terminal_focus = if role == AttachmentRole::Controller {
                    TerminalFocus::Focused
                } else {
                    TerminalFocus::Unfocused
                };
            }
            (Operation::Detach(session_id), DaemonResponse::Acknowledged) => {
                if let Some(terminal) = self.terminals.get_mut(&session_id) {
                    terminal.attached = false;
                }
                if self.active_session_id == Some(session_id) {
                    self.terminal_focus = TerminalFocus::Unfocused;
                }
            }
            (Operation::LoadDiff(worktree_id), DaemonResponse::Diff(diff)) => {
                if self.selected_worktree_id != Some(worktree_id) {
                    return;
                }
                self.diff = Some(if diff.text.is_empty() {
                    "Working tree is clean.".into()
                } else if diff.truncated {
                    format!("{}\n\n[Diff truncated by daemon limits]", diff.text)
                } else {
                    diff.text
                });
            }
            (Operation::Input(session_id), DaemonResponse::Acknowledged) => {
                if !self.terminals.contains_key(&session_id) {
                    self.error =
                        Some("Terminal input was acknowledged after the tab closed.".into());
                }
            }
            (Operation::Resize, DaemonResponse::Acknowledged) => {}
            (Operation::SaveDesktopState, DaemonResponse::DesktopStateSaved) => {
                self.state_save_pending = false;
                if self.closing_since.is_some()
                    && let Some(id) = self.window_id
                {
                    let _ = id;
                }
            }
            (Operation::CreateWorkspace, DaemonResponse::WorkspaceAdded { workspace, .. }) => {
                self.modal = None;
                self.show_success(format!("Workspace “{}” created.", workspace.name));
                self.request_snapshot();
            }
            (
                Operation::RegisterProject,
                DaemonResponse::ProjectReady {
                    project,
                    root_worktree,
                    ..
                },
            ) => {
                self.selected_project_id = Some(project.id);
                self.selected_worktree_id = Some(root_worktree.id);
                self.selected_session_id = None;
                self.modal = None;
                self.show_success("Repository registered.");
                self.mark_state_dirty();
                self.request_snapshot();
            }
            (Operation::CreateWorktree, DaemonResponse::WorktreeCreated { worktree, .. }) => {
                self.selected_worktree_id = Some(worktree.id);
                self.selected_session_id = None;
                self.modal = None;
                self.show_success("Managed worktree created.");
                self.mark_state_dirty();
                self.request_snapshot();
            }
            (Operation::RenameProject, DaemonResponse::ProjectUpdated { .. })
            | (Operation::RenameWorktree, DaemonResponse::WorktreeUpdated { .. })
            | (Operation::RenameSession, DaemonResponse::SessionUpdated { .. }) => {
                self.modal = None;
                self.show_success("Display name updated.");
                self.request_snapshot();
            }
            (Operation::InspectRemoval(worktree_id), DaemonResponse::WorktreeStatus(state)) => {
                if !state.clean {
                    self.error = Some(format!(
                        "Worktree is not clean: {} tracked, {} untracked, and {} ignored entries. SylvOps will not remove it.",
                        state.tracked_changes, state.untracked_files, state.ignored_files,
                    ));
                } else if state.removal_confirmation_token.is_none() {
                    self.error =
                        Some("The daemon did not authorize removal of this worktree.".into());
                } else if let Some(worktree) = self
                    .snapshot
                    .worktrees
                    .iter()
                    .find(|item| item.id == worktree_id)
                {
                    self.modal = Some(Modal::Confirmation(Confirmation::RemoveWorktree {
                        state,
                        name: worktree.name.clone(),
                        canonical_path: worktree.canonical_path.clone(),
                    }));
                }
            }
            (Operation::RemoveWorktree, DaemonResponse::WorktreeRemoved { .. }) => {
                self.modal = None;
                self.show_success("Worktree removed; its branch was preserved.");
                self.request_snapshot();
            }
            (operation, DaemonResponse::Error(failure)) => {
                self.error = Some(format!("{operation:?}: {}", failure.message));
            }
            (operation, response) => {
                self.error = Some(format!(
                    "unexpected response for {operation:?}: {response:?}"
                ));
            }
        }
    }

    fn send_request(&mut self, operation: Operation, request: ClientRequest) {
        if !self.bridge.request(operation, request) {
            self.error = Some("The desktop IPC command queue is busy. Try again.".into());
        }
    }

    fn handle_keyboard(&mut self, event: keyboard::Event) {
        let keyboard::Event::KeyPressed {
            key,
            modifiers,
            text,
            ..
        } = event
        else {
            return;
        };
        if self.modal.is_some() {
            if matches!(key, Key::Named(Named::Escape)) {
                self.modal = None;
            }
            return;
        }
        if self.terminal_focus.is_focused() {
            let Some(session_id) = self.active_session_id else {
                self.terminal_focus = TerminalFocus::Unfocused;
                return;
            };
            let Some(terminal) = self.terminals.get(&session_id) else {
                self.terminal_focus = TerminalFocus::Unfocused;
                return;
            };
            if !terminal.attached || terminal.role != AttachmentRole::Controller {
                self.terminal_focus = TerminalFocus::Unfocused;
                return;
            }
            if modifiers.control() && matches!(key.as_ref(), Key::Character("]")) {
                self.detach_active();
                return;
            }
            let Some(bytes) = encode_terminal_key(&key, modifiers, text.as_deref()) else {
                return;
            };
            self.send_request(
                Operation::Input(session_id),
                ClientRequest::SessionInput { session_id, bytes },
            );
            return;
        }

        let character = match key.as_ref() {
            Key::Character(character) => Some(character),
            _ => None,
        };
        if modifiers.control()
            && character.is_some_and(|character| character.eq_ignore_ascii_case("k"))
        {
            self.modal = Some(Modal::Shortcuts);
            return;
        }
        if modifiers.control() || modifiers.alt() {
            return;
        }

        match key.as_ref() {
            Key::Named(Named::Tab) => {
                self.cycle_keyboard_panel(if modifiers.shift() { -1 } else { 1 });
            }
            Key::Named(Named::ArrowLeft) => self.cycle_keyboard_panel(-1),
            Key::Named(Named::ArrowRight) => self.cycle_keyboard_panel(1),
            Key::Named(Named::ArrowUp) => self.move_keyboard_selection(-1),
            Key::Named(Named::ArrowDown) => self.move_keyboard_selection(1),
            Key::Named(Named::Enter) => self.open_keyboard_selection(),
            Key::Character("1") => self.select_main_tab(MainTab::Terminal),
            Key::Character("2") => self.select_main_tab(MainTab::Changes),
            Key::Character("3") => self.select_main_tab(MainTab::Details),
            Key::Character(value) if value.eq_ignore_ascii_case("n") => self.shortcut_new(),
            Key::Character(value) if value.eq_ignore_ascii_case("r") => self.shortcut_rename(),
            Key::Character(value) if value.eq_ignore_ascii_case("d") => self.shortcut_delete(),
            Key::Character(value) if value.eq_ignore_ascii_case("a") => self.attach_active(),
            Key::Character(value) if value.eq_ignore_ascii_case("g") => {
                self.select_main_tab(MainTab::Changes);
            }
            Key::Character("?") => self.modal = Some(Modal::Shortcuts),
            Key::Character(value) if value.eq_ignore_ascii_case("q") => self.begin_close(),
            _ => {}
        }
    }

    fn select_main_tab(&mut self, tab: MainTab) {
        self.main_tab = tab;
        self.terminal_focus = TerminalFocus::Unfocused;
        if tab == MainTab::Changes {
            self.load_diff();
        }
        self.mark_state_dirty();
    }

    fn cycle_keyboard_panel(&mut self, direction: i8) {
        let panels = [
            DesktopPanel::Projects,
            DesktopPanel::Worktrees,
            DesktopPanel::Sessions,
        ];
        let current = panels
            .iter()
            .position(|panel| *panel == self.keyboard_panel)
            .unwrap_or_default();
        let next = if direction < 0 {
            current.checked_sub(1).unwrap_or(panels.len() - 1)
        } else {
            (current + 1) % panels.len()
        };
        self.keyboard_panel = panels[next];
        self.desktop_state.compact_panel = panels[next];
        self.narrow_main = false;
        self.mark_state_dirty();
    }

    fn move_keyboard_selection(&mut self, direction: i8) {
        match self.keyboard_panel {
            DesktopPanel::Projects => {
                let ids: Vec<_> = self.visible_projects().iter().map(|item| item.id).collect();
                if let Some(id) = next_selection(&ids, self.selected_project_id, direction) {
                    self.select_project(id);
                }
            }
            DesktopPanel::Worktrees => {
                let ids: Vec<_> = self
                    .selected_project_id
                    .map_or_else(Vec::new, |project_id| {
                        self.worktrees_for(project_id)
                            .iter()
                            .map(|item| item.id)
                            .collect()
                    });
                if let Some(id) = next_selection(&ids, self.selected_worktree_id, direction) {
                    self.select_worktree(id);
                }
            }
            DesktopPanel::Sessions => {
                let ids: Vec<_> = self
                    .selected_worktree_id
                    .map_or_else(Vec::new, |worktree_id| {
                        self.sessions_for(worktree_id)
                            .iter()
                            .map(|item| item.id)
                            .collect()
                    });
                if let Some(id) = next_selection(&ids, self.selected_session_id, direction) {
                    self.select_session(id);
                }
            }
        }
    }

    fn open_keyboard_selection(&mut self) {
        if self.keyboard_panel == DesktopPanel::Sessions
            && let Some(session_id) = self.selected_session_id
        {
            self.select_session(session_id);
            self.narrow_main = true;
        }
    }

    fn shortcut_new(&mut self) {
        self.terminal_focus = TerminalFocus::Unfocused;
        match self.keyboard_panel {
            DesktopPanel::Projects => self.open_project_form(),
            DesktopPanel::Worktrees => self.open_worktree_form(),
            DesktopPanel::Sessions => self.open_session_form(),
        }
    }

    fn shortcut_rename(&mut self) {
        self.terminal_focus = TerminalFocus::Unfocused;
        match self.keyboard_panel {
            DesktopPanel::Projects => self.open_project_rename_form(),
            DesktopPanel::Worktrees => self.open_worktree_rename_form(),
            DesktopPanel::Sessions => self.open_session_rename_form(),
        }
    }

    fn shortcut_delete(&mut self) {
        self.terminal_focus = TerminalFocus::Unfocused;
        match self.keyboard_panel {
            DesktopPanel::Projects => {
                self.error = Some("Project deletion is intentionally unavailable.".into());
            }
            DesktopPanel::Worktrees => self.inspect_worktree_removal(),
            DesktopPanel::Sessions => self.open_stop_confirmation(),
        }
    }

    fn request_snapshot(&mut self) {
        if self.snapshot_pending {
            return;
        }
        self.snapshot_pending = self
            .bridge
            .request(Operation::RefreshSnapshot, ClientRequest::GetSnapshot);
    }

    fn open_project_form(&mut self) {
        let Some(workspace_id) = self.active_workspace().map(|workspace| workspace.id) else {
            self.error = Some("Create a workspace before registering a repository.".into());
            self.modal = Some(Modal::Form(FormModal::new(Form::workspace())));
            return;
        };
        self.modal = Some(Modal::Form(FormModal::new(Form::repository(workspace_id))));
    }

    fn open_worktree_form(&mut self) {
        let Some(project_id) = self.selected_project_id else {
            self.error = Some("Select a project before creating a worktree.".into());
            return;
        };
        self.modal = Some(Modal::Form(FormModal::new(Form::worktree(project_id))));
    }

    fn open_session_form(&mut self) {
        let Some(worktree_id) = self.selected_worktree_id else {
            self.error = Some("Select a worktree before creating a session.".into());
            return;
        };
        self.modal = Some(Modal::Form(FormModal::new(Form::session(
            worktree_id,
            self.providers.clone(),
        ))));
    }

    fn open_project_rename_form(&mut self) {
        let Some(project) = self
            .selected_project_id
            .and_then(|id| self.snapshot.projects.iter().find(|item| item.id == id))
        else {
            self.error = Some("Select a project to rename.".into());
            return;
        };
        self.modal = Some(Modal::Form(FormModal::new(Form::rename(
            "Rename project",
            &project.name,
            FormKind::RenameProject(project.id),
        ))));
    }

    fn open_worktree_rename_form(&mut self) {
        let Some(worktree) = self.selected_worktree() else {
            self.error = Some("Select a worktree to rename.".into());
            return;
        };
        self.modal = Some(Modal::Form(FormModal::new(Form::rename(
            "Rename worktree label",
            &worktree.name,
            FormKind::RenameWorktree(worktree.id),
        ))));
    }

    fn open_session_rename_form(&mut self) {
        let Some(session) = self.selected_session_id.and_then(|id| self.session(id)) else {
            self.error = Some("Select a session to rename.".into());
            return;
        };
        self.modal = Some(Modal::Form(FormModal::new(Form::rename(
            "Rename session",
            &session.display_name,
            FormKind::RenameSession(session.id),
        ))));
    }

    fn update_form_field(&mut self, index: usize, value: String) {
        if value.len() > 8 * 1024 {
            return;
        }
        if let Some(Modal::Form(modal)) = &mut self.modal
            && !modal.pending
        {
            let mirror_branch_name = index == 0
                && matches!(modal.form.kind, FormKind::CreateWorktree(_))
                && modal.form.fields.get(1).is_some_and(|name| {
                    name.value.is_empty()
                        || modal
                            .form
                            .fields
                            .first()
                            .is_some_and(|branch| name.value == branch.value)
                });
            if let Some(field) = modal.form.fields.get_mut(index) {
                field.value.clone_from(&value);
                field.error = None;
            }
            modal.form.submission_error = None;
            if mirror_branch_name && let Some(name) = modal.form.fields.get_mut(1) {
                name.value = value;
            }
        }
    }

    fn change_provider(&mut self, delta: isize) {
        if let Some(Modal::Form(modal)) = &mut self.modal
            && !modal.pending
        {
            modal.form.select_next_provider(delta);
        }
    }

    fn submit_form(&mut self) {
        let Some(Modal::Form(modal)) = &mut self.modal else {
            return;
        };
        if modal.pending || !modal.form.validate() {
            return;
        }
        let form = modal.form.clone();
        modal.pending = true;
        match form.kind {
            FormKind::CreateWorkspace => self.send_request(
                Operation::CreateWorkspace,
                ClientRequest::AddWorkspace {
                    name: form.fields[0].value.trim().to_owned(),
                },
            ),
            FormKind::RegisterProject(workspace_id) => self.send_request(
                Operation::RegisterProject,
                ClientRequest::EnsureProject {
                    workspace_id,
                    repository_path: form.fields[0].value.trim().to_owned(),
                },
            ),
            FormKind::CreateWorktree(project_id) => self.send_request(
                Operation::CreateWorktree,
                ClientRequest::CreateWorktree {
                    project_id,
                    branch: form.fields[0].value.trim().to_owned(),
                    name: trimmed_option(&form.fields[1].value),
                    base_ref: trimmed_option(&form.fields[2].value),
                },
            ),
            FormKind::CreateSession(worktree_id) => {
                let Some(provider) = form.provider() else {
                    return;
                };
                let (columns, rows) = self.terminal_dimensions();
                self.send_request(
                    Operation::CreateSession(worktree_id),
                    ClientRequest::CreateSession {
                        worktree_id,
                        provider: provider.kind,
                        display_name: trimmed_option(&form.fields[0].value),
                        model: trimmed_option(&form.fields[1].value),
                        effort: trimmed_option(&form.fields[2].value),
                        initial_prompt: trimmed_option(&form.fields[3].value),
                        columns,
                        rows,
                    },
                );
            }
            FormKind::RenameProject(project_id) => self.send_request(
                Operation::RenameProject,
                ClientRequest::RenameProject {
                    project_id,
                    name: form.fields[0].value.trim().to_owned(),
                },
            ),
            FormKind::RenameWorktree(worktree_id) => self.send_request(
                Operation::RenameWorktree,
                ClientRequest::RenameWorktree {
                    worktree_id,
                    name: form.fields[0].value.trim().to_owned(),
                },
            ),
            FormKind::RenameSession(session_id) => self.send_request(
                Operation::RenameSession,
                ClientRequest::RenameSession {
                    session_id,
                    name: form.fields[0].value.trim().to_owned(),
                },
            ),
        }
    }

    fn apply_picked_folder(&mut self, result: Result<Option<String>, String>) {
        match result {
            Ok(Some(path)) => self.update_form_field(0, path),
            Ok(None) => {}
            Err(error) => {
                if let Some(Modal::Form(modal)) = &mut self.modal {
                    modal.form.submission_error = Some(error);
                }
            }
        }
    }

    fn open_stop_confirmation(&mut self) {
        let Some(session) = self.active_session_id.and_then(|id| self.session(id)) else {
            self.error = Some("Select a session before stopping it.".into());
            return;
        };
        self.modal = Some(Modal::Confirmation(Confirmation::StopSession {
            session_name: session.display_name.clone(),
            cwd: session.cwd.clone(),
        }));
    }

    fn inspect_worktree_removal(&mut self) {
        let Some(worktree) = self.selected_worktree() else {
            self.error = Some("Select a managed worktree first.".into());
            return;
        };
        if worktree.is_root_checkout {
            self.error = Some("The root checkout cannot be removed by SylvOps.".into());
            return;
        }
        if worktree.status != WorktreeStatus::Active {
            self.error = Some("Only an active managed worktree can be removed.".into());
            return;
        }
        self.send_request(
            Operation::InspectRemoval(worktree.id),
            ClientRequest::GetWorktreeStatus {
                worktree_id: worktree.id,
            },
        );
    }

    fn confirm_action(&mut self) {
        let Some(Modal::Confirmation(confirmation)) = self.modal.clone() else {
            return;
        };
        match confirmation {
            Confirmation::StopSession { .. } => {
                if let Some(session_id) = self.active_session_id {
                    self.send_request(
                        Operation::Stop(session_id),
                        ClientRequest::StopSession { session_id },
                    );
                    self.modal = None;
                }
            }
            Confirmation::RemoveWorktree { state, .. } => {
                let Some(token) = state.removal_confirmation_token else {
                    self.error =
                        Some("The removal authorization expired. Refresh and try again.".into());
                    self.modal = None;
                    return;
                };
                self.send_request(
                    Operation::RemoveWorktree,
                    ClientRequest::RemoveWorktree {
                        worktree_id: state.worktree_id,
                        confirmation_token: token,
                    },
                );
            }
        }
    }

    fn attach_active(&mut self) {
        let Some(session_id) = self.active_session_id else {
            self.error = Some("Select a session before attaching.".into());
            return;
        };
        let from_sequence = self
            .terminals
            .get(&session_id)
            .map_or(0, |terminal| terminal.last_sequence);
        let (columns, rows) = self.terminal_dimensions();
        self.send_request(
            Operation::Attach(session_id),
            ClientRequest::AttachSession {
                session_id,
                from_sequence,
                columns,
                rows,
            },
        );
    }

    fn detach_active(&mut self) {
        self.terminal_focus = TerminalFocus::Unfocused;
        if let Some(session_id) = self.active_session_id {
            self.send_request(
                Operation::Detach(session_id),
                ClientRequest::DetachSession { session_id },
            );
        }
    }

    fn focus_terminal(&mut self) {
        let Some(session_id) = self.active_session_id else {
            return;
        };
        let Some(terminal) = self.terminals.get(&session_id) else {
            self.error = Some("Click Attach before typing in this terminal.".into());
            return;
        };
        if !terminal.attached {
            self.error = Some("This terminal is detached. Click Attach to reconnect.".into());
            return;
        }
        if terminal.role != AttachmentRole::Controller {
            self.error = Some(
                "This attachment is read-only because another client controls the session.".into(),
            );
            return;
        }
        self.terminal_focus = TerminalFocus::Focused;
        self.error = None;
    }

    fn load_diff(&mut self) {
        if let Some(worktree_id) = self.selected_worktree_id {
            self.diff = Some("Loading diff…".into());
            self.send_request(
                Operation::LoadDiff(worktree_id),
                ClientRequest::GetDiff { worktree_id },
            );
        }
    }

    fn persisted_state(&self) -> DesktopState {
        DesktopState {
            selected_project_id: self.selected_project_id,
            selected_worktree_id: self.selected_worktree_id,
            selected_session_id: self.selected_session_id,
            selected_main_tab: self.main_tab,
            open_session_ids: self.open_sessions.clone(),
            ..self.desktop_state.clone()
        }
        .normalized()
    }

    fn apply_desktop_state(&mut self, state: DesktopState) {
        let state = state.normalized();
        self.selected_project_id = state.selected_project_id;
        self.selected_worktree_id = state.selected_worktree_id;
        self.selected_session_id = state.selected_session_id;
        self.active_session_id = state.selected_session_id;
        self.open_sessions = state.open_session_ids.clone();
        self.main_tab = state.selected_main_tab;
        self.restore_window_size = Some((state.window_width, state.window_height));
        self.desktop_state = state;
        for (index, split) in self.pane_splits.iter().copied().enumerate() {
            self.panes.resize(
                split,
                f32::from(self.desktop_state.panel_ratios[index]) / 1000.0,
            );
        }
    }

    fn mark_state_dirty(&mut self) {
        self.state_dirty_at = Some(Instant::now());
    }

    fn save_desktop_state(&mut self) {
        if self.state_save_pending {
            return;
        }
        let state = self.persisted_state();
        if self.bridge.request(
            Operation::SaveDesktopState,
            ClientRequest::SaveDesktopState { state },
        ) {
            self.state_dirty_at = None;
            self.state_save_pending = true;
        }
    }

    fn flush_timers(&mut self) {
        let now = Instant::now();
        if self
            .success
            .as_ref()
            .is_some_and(|(_, created)| now.duration_since(*created) >= SUCCESS_DURATION)
        {
            self.success = None;
        }
        if self
            .state_dirty_at
            .is_some_and(|changed| now.duration_since(changed) >= SAVE_DEBOUNCE)
        {
            self.save_desktop_state();
        }
        if let Some((session_id, columns, rows, changed)) = self.pending_resize
            && now.duration_since(changed) >= RESIZE_DEBOUNCE
        {
            self.pending_resize = None;
            self.send_request(
                Operation::Resize,
                ClientRequest::ResizeSession {
                    session_id,
                    columns,
                    rows,
                },
            );
        }
    }

    fn show_success(&mut self, message: impl Into<String>) {
        self.success = Some((message.into(), Instant::now()));
        self.error = None;
    }

    fn begin_close(&mut self) {
        if self.closing_since.is_none() {
            let now = Instant::now();
            self.closing_since = Some(now);
            self.state_dirty_at = Some(now.checked_sub(SAVE_DEBOUNCE).unwrap_or(now));
            self.save_desktop_state();
        }
    }

    fn window_resized(&mut self, id: window::Id, size: iced::Size) {
        self.window_id = Some(id);
        if self.restore_window_size.is_some() {
            return;
        }
        self.desktop_state.window_width = bounded_u16(size.width, MIN_DESKTOP_WIDTH, u16::MAX);
        self.desktop_state.window_height = bounded_u16(size.height, MIN_DESKTOP_HEIGHT, u16::MAX);
        self.mark_state_dirty();
        self.queue_terminal_resize();
    }

    fn queue_terminal_resize(&mut self) {
        let Some(session_id) = self.active_session_id else {
            return;
        };
        if !self.terminals.get(&session_id).is_some_and(|terminal| {
            terminal.attached && terminal.role == AttachmentRole::Controller
        }) {
            return;
        }
        let (columns, rows) = self.terminal_dimensions();
        if let Some(terminal) = self.terminals.get_mut(&session_id) {
            if terminal.columns == columns && terminal.rows == rows {
                return;
            }
            terminal.parser.screen_mut().set_size(rows, columns);
            terminal.columns = columns;
            terminal.rows = rows;
        }
        self.pending_resize = Some((session_id, columns, rows, Instant::now()));
    }

    fn terminal_dimensions(&self) -> (u16, u16) {
        let main_width = match state::layout_mode(self.desktop_state.window_width) {
            state::LayoutMode::Wide => {
                let mut remaining = u32::from(self.desktop_state.window_width);
                for ratio in self.desktop_state.panel_ratios {
                    let pane = remaining.saturating_mul(u32::from(ratio)) / 1000;
                    remaining = remaining.saturating_sub(pane);
                }
                u16::try_from(remaining).unwrap_or(u16::MAX)
            }
            state::LayoutMode::Compact => self.desktop_state.window_width.saturating_sub(285),
            state::LayoutMode::Narrow => self.desktop_state.window_width,
        };
        let font = u16::from(self.desktop_state.terminal_font_size);
        let columns = (main_width / font.saturating_mul(3).saturating_div(5).max(1)).clamp(1, 500);
        let rows = (self.desktop_state.window_height.saturating_sub(150) / font.saturating_add(3))
            .clamp(1, 200);
        (columns, rows)
    }

    fn restore_selection(&mut self) {
        let projects = self.visible_projects();
        self.selected_project_id = self
            .selected_project_id
            .filter(|id| projects.iter().any(|project| project.id == *id))
            .or_else(|| projects.first().map(|project| project.id));
        self.selected_worktree_id = self.selected_project_id.and_then(|project_id| {
            let worktrees = self.worktrees_for(project_id);
            self.selected_worktree_id
                .filter(|id| worktrees.iter().any(|worktree| worktree.id == *id))
                .or_else(|| worktrees.first().map(|worktree| worktree.id))
        });
        self.selected_session_id = self.selected_worktree_id.and_then(|worktree_id| {
            let sessions = self.sessions_for(worktree_id);
            self.selected_session_id
                .filter(|id| sessions.iter().any(|session| session.id == *id))
                .or_else(|| sessions.first().map(|session| session.id))
        });
        self.open_sessions.retain(|id| {
            self.snapshot
                .sessions
                .iter()
                .any(|session| session.id == *id)
        });
        if self
            .active_session_id
            .is_some_and(|id| !self.open_sessions.contains(&id))
        {
            self.active_session_id = self.open_sessions.last().copied();
        }
    }

    fn select_project(&mut self, project_id: ProjectId) {
        self.keyboard_panel = DesktopPanel::Projects;
        self.desktop_state.compact_panel = DesktopPanel::Projects;
        self.terminal_focus = TerminalFocus::Unfocused;
        self.selected_project_id = Some(project_id);
        self.selected_worktree_id = self
            .worktrees_for(project_id)
            .first()
            .map(|worktree| worktree.id);
        self.selected_session_id = self
            .selected_worktree_id
            .and_then(|id| self.sessions_for(id).first().map(|session| session.id));
        self.diff = None;
        self.mark_state_dirty();
    }

    fn select_worktree(&mut self, worktree_id: WorktreeId) {
        self.keyboard_panel = DesktopPanel::Worktrees;
        self.desktop_state.compact_panel = DesktopPanel::Worktrees;
        self.terminal_focus = TerminalFocus::Unfocused;
        self.selected_worktree_id = Some(worktree_id);
        self.selected_session_id = self
            .sessions_for(worktree_id)
            .first()
            .map(|session| session.id);
        self.diff = None;
        self.mark_state_dirty();
    }

    fn select_session(&mut self, session_id: SessionId) {
        self.keyboard_panel = DesktopPanel::Sessions;
        self.desktop_state.compact_panel = DesktopPanel::Sessions;
        self.terminal_focus = TerminalFocus::Unfocused;
        self.selected_session_id = Some(session_id);
        self.active_session_id = Some(session_id);
        if !self.open_sessions.contains(&session_id) {
            if self.open_sessions.len() == MAX_OPEN_DESKTOP_SESSIONS {
                self.open_sessions.remove(0);
            }
            self.open_sessions.push(session_id);
        }
        self.mark_state_dirty();
        if state::layout_mode(self.desktop_state.window_width) == state::LayoutMode::Narrow {
            self.narrow_main = true;
        }
    }

    fn select_session_context(&mut self, session_id: SessionId) {
        self.selected_session_id = Some(session_id);
        if let Some(worktree_id) = self.session(session_id).map(|session| session.worktree_id) {
            self.selected_worktree_id = Some(worktree_id);
            self.selected_project_id = self
                .snapshot
                .worktrees
                .iter()
                .find(|worktree| worktree.id == worktree_id)
                .map(|worktree| worktree.project_id);
        }
    }

    fn active_workspace(&self) -> Option<&Workspace> {
        self.snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.is_open)
            .or_else(|| self.snapshot.workspaces.first())
    }

    fn visible_projects(&self) -> Vec<&Project> {
        let workspace_id = self.active_workspace().map(|workspace| workspace.id);
        let mut projects: Vec<_> = self
            .snapshot
            .projects
            .iter()
            .filter(|project| Some(project.workspace_id) == workspace_id)
            .collect();
        projects.sort_by_key(|project| std::cmp::Reverse(project.last_activity_at));
        projects
    }

    fn worktrees_for(&self, project_id: ProjectId) -> Vec<&Worktree> {
        let mut worktrees: Vec<_> = self
            .snapshot
            .worktrees
            .iter()
            .filter(|worktree| {
                worktree.project_id == project_id && worktree.status != WorktreeStatus::Removed
            })
            .collect();
        worktrees.sort_by_key(|worktree| {
            (
                !worktree.is_root_checkout,
                std::cmp::Reverse(worktree.last_activity_at),
            )
        });
        worktrees
    }

    fn sessions_for(&self, worktree_id: WorktreeId) -> Vec<&Session> {
        let mut sessions: Vec<_> = self
            .snapshot
            .sessions
            .iter()
            .filter(|session| session.worktree_id == worktree_id)
            .collect();
        sessions.sort_by_key(|session| std::cmp::Reverse(session.last_activity_at));
        sessions
    }

    fn session(&self, session_id: SessionId) -> Option<&Session> {
        self.snapshot
            .sessions
            .iter()
            .find(|session| session.id == session_id)
    }

    fn selected_worktree(&self) -> Option<&Worktree> {
        self.selected_worktree_id.and_then(|id| {
            self.snapshot
                .worktrees
                .iter()
                .find(|worktree| worktree.id == id)
        })
    }
}

fn subscription(_: &DesktopApp) -> Subscription<Message> {
    Subscription::batch([
        time::every(EVENT_TICK).map(|_| Message::Tick),
        keyboard::listen().map(Message::Keyboard),
        window::resize_events().map(|(id, size)| Message::WindowResized(id, size)),
        window::close_requests().map(Message::CloseRequested),
        system::theme_changes().map(Message::SystemThemeChanged),
    ])
}

fn next_selection<T: Copy + PartialEq>(
    items: &[T],
    selected: Option<T>,
    direction: i8,
) -> Option<T> {
    if items.is_empty() {
        return None;
    }
    let current = selected
        .and_then(|selected| items.iter().position(|item| *item == selected))
        .unwrap_or_default();
    let next = if direction < 0 {
        current.checked_sub(1).unwrap_or(items.len() - 1)
    } else {
        (current + 1) % items.len()
    };
    items.get(next).copied()
}

fn panel<'a>(content: impl Into<Element<'a, Message>>, _width: f32) -> Element<'a, Message> {
    container(content)
        .width(Fill)
        .height(Fill)
        .padding([4, 5])
        .style(panel_surface)
        .into()
}

fn column_heading(label: &str, action: Option<Message>) -> Element<'_, Message> {
    let mut heading = row![
        text(label)
            .font(UI_SEMIBOLD)
            .size(UI_META_SIZE)
            .style(text::secondary),
        space::horizontal()
    ]
    .align_y(Center);
    if let Some(action) = action {
        heading = heading.push(
            button(text("+").font(UI_MEDIUM).size(15))
                .on_press(action)
                .height(30)
                .padding([4, 8])
                .style(chrome_action_style),
        );
    }
    container(heading)
        .height(PANEL_HEADER_HEIGHT)
        .padding([4, 7])
        .width(Fill)
        .into()
}

fn select_button(
    label: &str,
    selected: bool,
    message: Message,
    density: DesktopDensity,
) -> Element<'static, Message> {
    let (height, padding) = match density {
        DesktopDensity::Comfortable => (36, [8, 9]),
        DesktopDensity::Compact => (30, [5, 8]),
    };
    button(
        text(label.to_owned())
            .font(if selected { UI_MEDIUM } else { UI_FONT })
            .size(UI_TEXT_SIZE)
            .width(Fill),
    )
    .on_press(message)
    .style(move |theme, status| list_item_style(theme, status, selected))
    .width(Fill)
    .height(height)
    .padding(padding)
    .into()
}

fn tab_button(label: &str, tab: MainTab, active: MainTab) -> Element<'_, Message> {
    let selected = tab == active;
    button(text(label).font(UI_MEDIUM).size(UI_TEXT_SIZE))
        .on_press(Message::SelectMainTab(tab))
        .height(ACTION_HEIGHT)
        .padding([6, 11])
        .style(move |theme, status| content_tab_style(theme, status, selected))
        .into()
}

fn compact_panel_button(
    label: &str,
    panel: DesktopPanel,
    active: DesktopPanel,
) -> Element<'_, Message> {
    button(text(label).size(UI_META_SIZE))
        .on_press(Message::SelectCompactPanel(panel))
        .style(if panel == active {
            button::primary
        } else {
            button::secondary
        })
        .into()
}

fn trimmed_option(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn bounded_u16(value: f32, minimum: u16, maximum: u16) -> u16 {
    if !value.is_finite() {
        return minimum;
    }
    value.round().clamp(f32::from(minimum), f32::from(maximum)) as u16
}

async fn pick_repository_folder() -> Result<Option<String>, String> {
    tokio::task::spawn_blocking(|| {
        Ok(rfd::FileDialog::new()
            .set_title("Choose a Git repository")
            .pick_folder()
            .map(|path| path.to_string_lossy().into_owned()))
    })
    .await
    .map_err(|error| format!("folder picker failed: {error}"))?
}

fn centered_message(message: &str) -> Element<'_, Message> {
    container(text(message).style(text::secondary))
        .width(Fill)
        .height(Fill)
        .center(Fill)
        .into()
}

fn empty_hint(message: &str) -> Element<'_, Message> {
    container(text(message).size(UI_META_SIZE).style(text::secondary))
        .padding(8)
        .width(Fill)
        .into()
}

fn footer_item(label: &str) -> Element<'static, Message> {
    text(label.to_owned())
        .size(UI_META_SIZE)
        .style(text::secondary)
        .into()
}

fn chrome_surface(theme: &Theme) -> container::Style {
    let palette = theme.extended_palette();
    container::Style {
        background: Some(Background::Color(palette.background.weakest.color)),
        border: Border {
            width: 1.0,
            color: palette.background.weak.color,
            ..Border::default()
        },
        ..container::Style::default()
    }
}

fn panel_surface(theme: &Theme) -> container::Style {
    let palette = theme.extended_palette();
    container::Style {
        background: Some(Background::Color(palette.background.weakest.color)),
        ..container::Style::default()
    }
}

fn workspace_surface(theme: &Theme) -> container::Style {
    let palette = theme.extended_palette();
    container::Style {
        background: Some(Background::Color(palette.background.base.color)),
        ..container::Style::default()
    }
}

fn selected_context_style(theme: &Theme) -> container::Style {
    let palette = theme.extended_palette();
    container::Style {
        background: Some(Background::Color(palette.primary.weak.color)),
        text_color: Some(palette.primary.weak.text),
        border: Border {
            width: 1.0,
            radius: 6.0.into(),
            color: palette.primary.strong.color,
        },
        ..container::Style::default()
    }
}

fn terminal_surface(theme: &Theme, focused: bool) -> container::Style {
    let palette = theme.extended_palette();
    container::Style {
        background: Some(Background::Color(palette.background.base.color)),
        border: Border {
            width: if focused { 2.0 } else { 1.0 },
            radius: 4.0.into(),
            color: if focused {
                palette.primary.strong.color
            } else {
                palette.background.weak.color
            },
        },
        ..container::Style::default()
    }
}

fn tab_strip_surface(theme: &Theme) -> container::Style {
    let palette = theme.extended_palette();
    container::Style {
        background: Some(Background::Color(palette.background.weakest.color)),
        ..container::Style::default()
    }
}

fn modal_scrim(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(Color::from_rgba8(8, 10, 14, 0.68))),
        ..container::Style::default()
    }
}

fn modal_card(theme: &Theme) -> container::Style {
    let palette = theme.extended_palette();
    container::Style {
        background: Some(Background::Color(palette.background.base.color)),
        border: Border {
            width: 1.0,
            radius: 10.0.into(),
            color: palette.background.strong.color,
        },
        ..container::Style::default()
    }
}

fn chrome_action_style(theme: &Theme, status: Status) -> button::Style {
    let palette = theme.extended_palette();
    let background = match status {
        Status::Hovered => Some(palette.background.weak.color),
        Status::Pressed => Some(palette.background.neutral.color),
        Status::Active | Status::Disabled => None,
    };
    button::Style {
        background: background.map(Background::Color),
        text_color: palette
            .background
            .base
            .text
            .scale_alpha(if status == Status::Disabled {
                0.45
            } else {
                1.0
            }),
        border: Border {
            radius: 6.0.into(),
            ..Border::default()
        },
        ..button::Style::default()
    }
}

fn workspace_tab_style(theme: &Theme, status: Status, active: bool) -> button::Style {
    let palette = theme.extended_palette();
    let pair = if active {
        palette.background.base
    } else if status == Status::Hovered {
        palette.background.weak
    } else {
        palette.background.weakest
    };
    button::Style {
        background: Some(Background::Color(pair.color)),
        text_color: pair.text.scale_alpha(if status == Status::Disabled {
            0.45
        } else {
            1.0
        }),
        border: Border {
            radius: 7.0.into(),
            color: if active {
                palette.background.weak.color
            } else {
                pair.color
            },
            width: if active { 1.0 } else { 0.0 },
        },
        ..button::Style::default()
    }
}

fn list_item_style(theme: &Theme, status: Status, selected: bool) -> button::Style {
    let palette = theme.extended_palette();
    let pair = if selected {
        palette.primary.weak
    } else if status == Status::Hovered {
        palette.background.weak
    } else {
        palette.background.weakest
    };
    button::Style {
        background: Some(Background::Color(pair.color)),
        text_color: pair.text.scale_alpha(if status == Status::Disabled {
            0.45
        } else {
            1.0
        }),
        border: Border {
            radius: 5.0.into(),
            ..Border::default()
        },
        ..button::Style::default()
    }
}

fn content_tab_style(theme: &Theme, status: Status, selected: bool) -> button::Style {
    let palette = theme.extended_palette();
    let pair = if selected {
        palette.primary.weak
    } else if status == Status::Hovered {
        palette.background.weak
    } else {
        palette.background.base
    };
    button::Style {
        background: Some(Background::Color(pair.color)),
        text_color: pair.text,
        border: Border {
            radius: 5.0.into(),
            ..Border::default()
        },
        ..button::Style::default()
    }
}

fn session_tab_style(theme: &Theme, status: Status, selected: bool) -> button::Style {
    let palette = theme.extended_palette();
    let pair = if selected {
        palette.background.base
    } else if status == Status::Hovered {
        palette.background.weak
    } else {
        palette.background.weakest
    };
    button::Style {
        background: Some(Background::Color(pair.color)),
        text_color: pair.text,
        border: Border {
            radius: 5.0.into(),
            color: if selected {
                palette.background.weak.color
            } else {
                pair.color
            },
            width: if selected { 1.0 } else { 0.0 },
        },
        ..button::Style::default()
    }
}

fn detail(label: &str, value: String) -> Element<'_, Message> {
    row![
        text(label)
            .width(Length::Fixed(150.0))
            .style(text::secondary),
        text(value)
    ]
    .align_y(Vertical::Center)
    .into()
}

const fn status_symbol(state: SessionState) -> &'static str {
    match state {
        SessionState::Fresh => "○",
        SessionState::Starting | SessionState::Running => "◐",
        SessionState::NeedsFeedback => "!",
        SessionState::FinishedUnseen => "◆",
        SessionState::FinishedSeen => "✓",
        SessionState::Failed => "×",
        SessionState::Terminated | SessionState::Disconnected => "—",
    }
}
