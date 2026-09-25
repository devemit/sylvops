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
    Background, Border, Center, Color, Element, Fill, Font, Length, Point, Subscription, Task,
    Theme,
    alignment::Vertical,
    clipboard, keyboard,
    keyboard::{Key, key::Named},
    mouse, system, time,
    widget::{
        self, button, column, container, mouse_area, opaque, pane_grid, responsive, rich_text, row,
        rule, scrollable, sensor, slider, space, span, stack, text, text_input, vertical_slider,
    },
    window,
};
use iced::{font, font::Weight, widget::button::Status};
use sylvops_core::{
    domain::{
        AttachmentRole, DaemonSnapshot, Project, ProviderKind, Session, SessionState, Workspace,
        Worktree, WorktreeStatus, state_allows_resume,
    },
    ids::{ProjectId, SessionId, WorkspaceId, WorktreeId},
    protocol::{ClientRequest, DaemonEvent, DaemonResponse},
    provider::ProviderHealth,
    ui::{
        DesktopDensity, DesktopPanel, DesktopState, DesktopTerminalFont, DesktopTheme,
        MAX_OPEN_DESKTOP_SESSIONS, MAX_TERMINAL_FONT_SIZE, MIN_DESKTOP_HEIGHT, MIN_DESKTOP_WIDTH,
        MIN_TERMINAL_FONT_SIZE, MainTab,
    },
    ui_forms::{Form, FormKind},
};
use sylvops_daemon::runtime::RuntimePaths;
use terminal::{
    DisplayRun, TerminalState, WheelAction, display_runs as terminal_display_runs,
    encode_key as encode_terminal_key, encode_paste as encode_terminal_paste,
};

const EVENT_TICK: Duration = Duration::from_millis(16);
const SAVE_DEBOUNCE: Duration = Duration::from_millis(500);
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(75);
const SUCCESS_DURATION: Duration = Duration::from_secs(4);
const MAX_EVENTS_PER_TICK: usize = 512;
const NAV_HEIGHT: f32 = 44.0;
const FOOTER_HEIGHT: f32 = 32.0;
const SESSION_TAB_HEIGHT: f32 = 42.0;
const PANEL_HEADER_HEIGHT: f32 = 42.0;
const UI_TEXT_SIZE: f32 = 14.0;
const UI_META_SIZE: f32 = 12.0;
const FOOTER_TEXT_SIZE: f32 = 12.0;
const ACTION_HEIGHT: f32 = 34.0;
const TERMINAL_HORIZONTAL_PADDING: f32 = 24.0;
const TERMINAL_VERTICAL_PADDING: f32 = 16.0;
const TERMINAL_HEADER_HEIGHT: f32 = 26.0;
const TERMINAL_HEADER_SPACING: f32 = 4.0;
const TERMINAL_SCROLLBAR_WIDTH: f32 = 14.0;
const TERMINAL_CELL_WIDTH_RATIO: f32 = 0.6;
const TERMINAL_LINE_HEIGHT_RATIO: f32 = 1.3;
#[cfg(windows)]
const UI_FONT: Font = Font::with_name("Segoe UI");
#[cfg(target_os = "macos")]
const UI_FONT: Font = Font::with_name("SF Pro Text");
#[cfg(all(not(windows), not(target_os = "macos")))]
const UI_FONT: Font = Font::DEFAULT;
#[cfg(windows)]
const SYSTEM_TERMINAL_FONT: Font = Font::with_name("Consolas");
#[cfg(target_os = "macos")]
const SYSTEM_TERMINAL_FONT: Font = Font::with_name("Menlo");
#[cfg(all(not(windows), not(target_os = "macos")))]
const SYSTEM_TERMINAL_FONT: Font = Font::MONOSPACE;
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
                Task::batch([
                    system::theme().map(Message::SystemThemeChanged),
                    window::latest().map(Message::WindowReady),
                ]),
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
    terminal_viewport: Option<iced::Size>,
    terminal_pointer: Option<Point>,
    terminal_selection_state: TerminalSelectionState,
    inline_session_rename: Option<InlineSessionRename>,
    hovered_session_id: Option<SessionId>,
    window_mode: window::Mode,
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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum TerminalSelectionState {
    #[default]
    Idle,
    Selecting,
}

#[derive(Clone, Debug)]
struct InlineSessionRename {
    session_id: SessionId,
    value: String,
    pending: bool,
    error: Option<String>,
    input_id: widget::Id,
}

impl InlineSessionRename {
    fn new(session_id: SessionId, value: &str) -> Self {
        Self {
            session_id,
            value: value.to_owned(),
            pending: false,
            error: None,
            input_id: widget::Id::unique(),
        }
    }

    fn update(&mut self, value: String) {
        if !self.pending && value.len() <= 8 * 1024 {
            self.value = value;
            self.error = None;
        }
    }

    fn begin_submission(&mut self) -> Option<String> {
        if self.pending {
            return None;
        }
        let name = self.value.trim();
        if name.is_empty() || name.chars().count() > 200 {
            self.error = Some("Session name must contain between 1 and 200 characters.".into());
            return None;
        }
        self.pending = true;
        self.error = None;
        Some(name.to_owned())
    }

    fn fail(&mut self, message: String) {
        self.pending = false;
        self.error = Some(message);
    }
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
    BeginSessionRename(SessionId),
    SessionRenameInput(String),
    SubmitSessionRename,
    CancelSessionRename,
    HoverSession(Option<SessionId>),
    RemoveSelectedWorktree,
    FormInput(usize, String),
    SelectProvider(ProviderKind),
    ProbeProvider,
    SubmitForm,
    CancelModal,
    BrowseRepository,
    FolderPicked(Result<Option<String>, String>),
    ConfirmAction,
    Attach,
    Detach,
    TerminalPointerMoved(Point),
    TerminalSelectionStarted,
    TerminalSelectionEnded,
    TerminalPaste(Option<String>),
    ScrollTerminal(mouse::ScrollDelta),
    SetTerminalScrollback(u32),
    LatestTerminal,
    TerminalViewportResized(iced::Size),
    Stop,
    Refresh,
    ToggleSettings,
    ToggleFullscreen,
    SelectTheme(DesktopTheme),
    SelectDensity(DesktopDensity),
    SelectTerminalFont(DesktopTerminalFont),
    SetTerminalFontSize(u8),
    ResetLayout,
    SelectCompactPanel(DesktopPanel),
    WindowResized(window::Id, iced::Size),
    WindowReady(Option<window::Id>),
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
            terminal_viewport: None,
            terminal_pointer: None,
            terminal_selection_state: TerminalSelectionState::Idle,
            inline_session_rename: None,
            hovered_session_id: None,
            window_mode: window::Mode::Windowed,
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
            Message::Keyboard(event) => {
                if let Some(task) = self.handle_terminal_clipboard(&event) {
                    return task;
                }
                if self.modal.is_none()
                    && self.inline_session_rename.is_some()
                    && let keyboard::Event::KeyPressed { key, .. } = &event
                {
                    match key {
                        Key::Named(Named::Escape)
                            if self
                                .inline_session_rename
                                .as_ref()
                                .is_some_and(|rename| !rename.pending) =>
                        {
                            cancel_inline_session_rename(&mut self.inline_session_rename);
                        }
                        Key::Named(Named::Enter) => self.submit_session_rename(),
                        _ => {}
                    }
                    return Task::none();
                }
                if self.modal.is_none()
                    && !self.terminal_focus.is_focused()
                    && self.keyboard_panel == DesktopPanel::Sessions
                    && let keyboard::Event::KeyPressed { key, modifiers, .. } = &event
                    && !modifiers.control()
                    && !modifiers.alt()
                    && matches!(key.as_ref(), Key::Character(value) if value.eq_ignore_ascii_case("r"))
                    && let Some(session_id) = self.selected_session_id
                {
                    return self.begin_session_rename(session_id);
                }
                self.handle_keyboard(event);
            }
            Message::SelectWorkspace(workspace_id) => {
                self.unfocus_terminal();
                if self
                    .active_workspace()
                    .is_some_and(|workspace| workspace.id == workspace_id && workspace.is_open)
                {
                    return Task::none();
                }
                self.inline_session_rename = None;
                self.send_request(
                    Operation::OpenWorkspace(workspace_id),
                    ClientRequest::OpenWorkspace { workspace_id },
                );
            }
            Message::SelectProject(project_id) => self.select_project(project_id),
            Message::SelectWorktree(worktree_id) => self.select_worktree(worktree_id),
            Message::SelectSession(session_id) => self.select_session(session_id),
            Message::SelectOpenSession(session_id) => {
                self.unfocus_terminal();
                if self.active_session_id == Some(session_id) {
                    return Task::none();
                }
                if self
                    .inline_session_rename
                    .as_ref()
                    .is_some_and(|rename| rename.session_id != session_id)
                {
                    self.inline_session_rename = None;
                }
                self.keyboard_panel = DesktopPanel::Sessions;
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
                self.inline_session_rename = None;
                self.terminal_focus = TerminalFocus::Unfocused;
                self.modal = Some(Modal::Form(FormModal::new(Form::workspace())));
            }
            Message::NewProject => self.open_project_form(),
            Message::NewWorktree => self.open_worktree_form(),
            Message::NewSession => self.open_session_form(),
            Message::RenameProject => self.open_project_rename_form(),
            Message::RenameWorktree => self.open_worktree_rename_form(),
            Message::BeginSessionRename(session_id) => {
                return self.begin_session_rename(session_id);
            }
            Message::SessionRenameInput(value) => {
                if let Some(rename) = &mut self.inline_session_rename {
                    rename.update(value);
                }
            }
            Message::SubmitSessionRename => self.submit_session_rename(),
            Message::CancelSessionRename => {
                cancel_inline_session_rename(&mut self.inline_session_rename);
            }
            Message::HoverSession(session_id) => self.hovered_session_id = session_id,
            Message::RemoveSelectedWorktree => self.inspect_worktree_removal(),
            Message::FormInput(index, value) => self.update_form_field(index, value),
            Message::SelectProvider(kind) => self.select_provider(kind),
            Message::ProbeProvider => self.probe_selected_provider(),
            Message::SubmitForm => self.submit_form(),
            Message::CancelModal => self.modal = None,
            Message::BrowseRepository => {
                return Task::perform(pick_repository_folder(), Message::FolderPicked);
            }
            Message::FolderPicked(result) => self.apply_picked_folder(result),
            Message::ConfirmAction => self.confirm_action(),
            Message::Attach => self.attach_active(),
            Message::Detach => self.detach_active(),
            Message::TerminalPointerMoved(point) => self.move_terminal_pointer(point),
            Message::TerminalSelectionStarted => self.start_terminal_selection(),
            Message::TerminalSelectionEnded => self.finish_terminal_selection(),
            Message::TerminalPaste(contents) => self.paste_terminal(contents),
            Message::ScrollTerminal(delta) => self.scroll_terminal(delta),
            Message::SetTerminalScrollback(position) => {
                self.set_terminal_scrollback(position);
            }
            Message::LatestTerminal => self.show_latest_terminal(),
            Message::TerminalViewportResized(size) => {
                if self.terminal_viewport != Some(size) {
                    self.terminal_viewport = Some(size);
                    self.queue_terminal_resize();
                }
            }
            Message::Stop => self.open_stop_confirmation(),
            Message::Refresh => self.request_snapshot(),
            Message::ToggleSettings => {
                self.inline_session_rename = None;
                self.terminal_focus = TerminalFocus::Unfocused;
                self.modal = if matches!(self.modal, Some(Modal::Settings)) {
                    None
                } else {
                    Some(Modal::Settings)
                };
            }
            Message::ToggleFullscreen => {
                let Some(window_id) = self.window_id else {
                    self.error = Some("The application window is not ready yet.".into());
                    return Task::none();
                };
                self.window_mode = if self.window_mode == window::Mode::Fullscreen {
                    window::Mode::Windowed
                } else {
                    window::Mode::Fullscreen
                };
                return window::set_mode(window_id, self.window_mode);
            }
            Message::SelectTheme(theme) => {
                self.desktop_state.theme = theme;
                self.mark_state_dirty();
            }
            Message::SelectDensity(density) => {
                self.desktop_state.density = density;
                self.mark_state_dirty();
            }
            Message::SelectTerminalFont(font) => {
                self.desktop_state.terminal_font = font;
                self.mark_state_dirty();
                self.queue_terminal_resize();
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
                if panel != DesktopPanel::Sessions {
                    self.inline_session_rename = None;
                }
                self.keyboard_panel = panel;
                self.terminal_focus = TerminalFocus::Unfocused;
                self.desktop_state.compact_panel = panel;
                self.mark_state_dirty();
            }
            Message::WindowResized(id, size) => self.window_resized(id, size),
            Message::WindowReady(id) => self.window_id = id,
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
                self.inline_session_rename = None;
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
        let mut content = column![top, rule::horizontal(1), body]
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
        } else if let Some((message, _)) = &self.success {
            content = content.push(
                container(text(message).style(text::success))
                    .padding([6, 12])
                    .width(Fill),
            );
        }
        content = content.push(rule::horizontal(1)).push(self.footer());
        let content: Element<'_, Message> = container(content).width(Fill).height(Fill).into();
        if self.inline_session_rename.is_some() {
            mouse_area(content)
                .on_press(Message::CancelSessionRename)
                .into()
        } else {
            content
        }
    }

    fn top_bar(&self) -> Element<'_, Message> {
        let compact = self.desktop_state.window_width < 1_000;
        let fullscreen_label = fullscreen_label(self.window_mode, compact);
        let worktree_context = self.selected_worktree().map_or_else(
            || "No checkout selected".to_owned(),
            |worktree| {
                format!(
                    "Checkout: {}  ·  {}",
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
        .width(Length::Fixed(if compact { 106.0 } else { 132.0 }))
        .padding([0, 8]);
        let mut workspace_tabs = row![].spacing(0).align_y(Center);
        for (index, workspace) in self.snapshot.workspaces.iter().enumerate() {
            if index > 0 {
                workspace_tabs = workspace_tabs.push(rule::vertical(1));
            }
            let active = workspace.is_open;
            let label = workspace.name.clone();
            let action = button(
                text(label.clone())
                    .font(if active { UI_SEMIBOLD } else { UI_FONT })
                    .size(UI_TEXT_SIZE)
                    .wrapping(text::Wrapping::None),
            )
            .on_press(Message::SelectWorkspace(workspace.id))
            .height(ACTION_HEIGHT)
            .padding([6, 14])
            .style(move |theme, status| workspace_tab_style(theme, status, active));
            workspace_tabs = workspace_tabs.push(action);
        }
        if !self.snapshot.workspaces.is_empty() {
            workspace_tabs = workspace_tabs.push(rule::vertical(1));
        }
        workspace_tabs = workspace_tabs.push(
            button(
                text(if compact { "+" } else { "+ Workspace" })
                    .font(UI_MEDIUM)
                    .size(if compact { 16.0 } else { UI_META_SIZE }),
            )
            .on_press(Message::NewWorkspace)
            .height(ACTION_HEIGHT)
            .padding([5, 11])
            .style(chrome_action_style),
        );
        let workspace_tabs = scrollable(workspace_tabs)
            .direction(scrollable::Direction::Horizontal(
                scrollable::Scrollbar::hidden(),
            ))
            .width(Fill)
            .height(ACTION_HEIGHT);
        let mut actions = row![].spacing(4).align_y(Center);
        if !compact {
            actions = actions.push(
                container(text(worktree_context).font(UI_MEDIUM).size(UI_META_SIZE))
                    .height(ACTION_HEIGHT)
                    .padding([7, 11])
                    .align_y(Vertical::Center)
                    .style(selected_context_style),
            );
        }
        let actions = actions
            .push(self.refresh_button(compact))
            .push(
                button(text(fullscreen_label).font(UI_MEDIUM).size(UI_META_SIZE))
                    .on_press(Message::ToggleFullscreen)
                    .height(ACTION_HEIGHT)
                    .padding([6, 11])
                    .style(chrome_action_style),
            )
            .push(
                button(text("Settings").font(UI_MEDIUM).size(UI_META_SIZE))
                    .on_press(Message::ToggleSettings)
                    .height(ACTION_HEIGHT)
                    .padding([6, 11])
                    .style(chrome_action_style),
            );
        let navigation = row![brand, workspace_tabs, actions]
            .spacing(8)
            .align_y(Center);
        container(navigation)
            .height(NAV_HEIGHT)
            .width(Fill)
            .padding([5, 8])
            .align_y(Vertical::Center)
            .style(chrome_surface)
            .into()
    }

    fn refresh_button(&self, compact: bool) -> Element<'static, Message> {
        let can_refresh =
            matches!(self.connection, ConnectionState::Connected) && !self.snapshot_pending;
        let label = if self.snapshot_pending {
            if compact { "…" } else { "Refreshing…" }
        } else if compact {
            "↻"
        } else {
            "Refresh"
        };
        button(text(label).size(if compact { 17 } else { 12 }))
            .on_press_maybe(can_refresh.then_some(Message::Refresh))
            .height(ACTION_HEIGHT)
            .padding([5, 10])
            .style(chrome_action_style)
            .into()
    }

    fn mission_control(&self) -> Element<'_, Message> {
        if matches!(self.connection, ConnectionState::Connecting) {
            return centered_message("Connecting to the SylvOps daemon…");
        }
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
                        .style(move |theme, status| {
                            content_tab_style(theme, status, !self.narrow_main)
                        }),
                    button("Main")
                        .on_press(Message::ShowNarrowMain)
                        .style(move |theme, status| {
                            content_tab_style(theme, status, self.narrow_main)
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
        let mut items =
            column![column_heading("Repositories", Some(Message::NewProject))].spacing(4);
        let projects = self.visible_projects();
        for project in &projects {
            items = items.push(select_button(
                &project.name,
                self.selected_project_id == Some(project.id),
                Message::SelectProject(project.id),
                self.desktop_state.density,
            ));
        }
        if projects.is_empty() {
            items = items.push(if self.active_workspace().is_some() {
                empty_action(
                    "No repositories in this workspace yet.",
                    "Register repository",
                    Message::NewProject,
                )
            } else {
                empty_action(
                    "Create a workspace to begin.",
                    "Create workspace",
                    Message::NewWorkspace,
                )
            });
        }
        panel(scrollable(items), 190.0)
    }

    fn worktrees_column(&self) -> Element<'_, Message> {
        let create = self.selected_project_id.map(|_| Message::NewWorktree);
        let mut items = column![column_heading("Checkouts", create)].spacing(4);
        if let Some(project_id) = self.selected_project_id {
            let worktrees = self.worktrees_for(project_id);
            for worktree in &worktrees {
                let branch = worktree.branch.as_deref().unwrap_or("detached");
                let label = if worktree.is_root_checkout {
                    format!("{}  ·  {branch}  ·  root", worktree.name)
                } else {
                    format!("{}  ·  {branch}", worktree.name)
                };
                let selected = self.selected_worktree_id == Some(worktree.id);
                let select = select_button(
                    &label,
                    selected,
                    Message::SelectWorktree(worktree.id),
                    self.desktop_state.density,
                );
                if selected && worktree_can_delete(worktree) {
                    items = items.push(
                        row![
                            select,
                            button(text("Delete").font(UI_MEDIUM).size(UI_META_SIZE))
                                .on_press(Message::RemoveSelectedWorktree)
                                .height(match self.desktop_state.density {
                                    DesktopDensity::Comfortable => 36,
                                    DesktopDensity::Compact => 30,
                                })
                                .padding([4, 7])
                                .style(flat_danger_style),
                        ]
                        .spacing(3)
                        .align_y(Center),
                    );
                } else {
                    items = items.push(select);
                }
            }
            if worktrees.is_empty() {
                items = items.push(empty_action(
                    "No checkouts are available for this repository.",
                    "Create checkout",
                    Message::NewWorktree,
                ));
            }
        } else {
            items = items.push(empty_hint("Select a repository."));
        }
        panel(scrollable(items), 225.0)
    }

    fn sessions_column(&self) -> Element<'_, Message> {
        let create = self.selected_worktree_id.map(|_| Message::NewSession);
        let mut items = column![column_heading("Sessions", create)].spacing(4);
        if let Some(worktree_id) = self.selected_worktree_id {
            let sessions = self.sessions_for(worktree_id);
            for session in &sessions {
                let selected = self.selected_session_id == Some(session.id);
                if self
                    .inline_session_rename
                    .as_ref()
                    .is_some_and(|rename| rename.session_id == session.id)
                {
                    items = items.push(self.session_rename_row());
                } else {
                    let label = session_navigation_label(&session.display_name, session.state);
                    let hovered = self.hovered_session_id == Some(session.id);
                    let height = match self.desktop_state.density {
                        DesktopDensity::Comfortable => 36,
                        DesktopDensity::Compact => 30,
                    };
                    let row = container(
                        text(label.clone())
                            .font(if selected { UI_MEDIUM } else { UI_FONT })
                            .size(UI_TEXT_SIZE)
                            .wrapping(text::Wrapping::None)
                            .width(Fill),
                    )
                    .width(Fill)
                    .height(height)
                    .padding([5, 9])
                    .align_y(Vertical::Center)
                    .clip(true)
                    .style(move |theme| list_item_container_style(theme, selected, hovered));
                    let interaction = mouse_area(row)
                        .on_enter(Message::HoverSession(Some(session.id)))
                        .on_exit(Message::HoverSession(None))
                        .on_press(Message::SelectSession(session.id))
                        .on_double_click(Message::BeginSessionRename(session.id))
                        .interaction(mouse::Interaction::Pointer);
                    items = items.push(interaction);
                }
            }
            if sessions.is_empty() {
                items = items.push(empty_action(
                    "No sessions in this checkout yet.",
                    "Start a session",
                    Message::NewSession,
                ));
            }
        } else {
            items = items.push(empty_hint("Select a checkout."));
        }
        panel(scrollable(items), 255.0)
    }

    fn session_rename_row(&self) -> Element<'_, Message> {
        let Some(rename) = &self.inline_session_rename else {
            return space::vertical().height(0).into();
        };
        let controls = text_input("Session name", &rename.value)
            .id(rename.input_id.clone())
            .on_input_maybe((!rename.pending).then_some(Message::SessionRenameInput))
            .on_submit_maybe((!rename.pending).then_some(Message::SubmitSessionRename))
            .padding([6, 8])
            .size(UI_TEXT_SIZE);
        let mut content = column![controls].spacing(4);
        if let Some(error) = &rename.error {
            content = content.push(text(error).size(UI_META_SIZE).style(text::danger));
        }
        container(content).padding([3, 0]).width(Fill).into()
    }

    fn compact_navigator(&self) -> Element<'_, Message> {
        let tabs = row![
            compact_panel_button(
                "Repositories",
                DesktopPanel::Projects,
                self.desktop_state.compact_panel
            ),
            compact_panel_button(
                "Checkouts",
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
        let mut tabs = row![].spacing(6).align_y(Center);
        for session_id in &self.open_sessions {
            if let Some(session) = self.session(*session_id) {
                let label = session_tab_label(&session.display_name);
                let active = self.active_session_id == Some(session.id);
                tabs = tabs.push(
                    container(
                        row![
                            button(
                                text(label)
                                    .font(if active { UI_SEMIBOLD } else { UI_FONT })
                                    .size(UI_TEXT_SIZE)
                                    .wrapping(text::Wrapping::None)
                            )
                            .on_press(Message::SelectOpenSession(session.id))
                            .height(30)
                            .padding([5, 9])
                            .style(move |theme, status| { tab_label_style(theme, status, active) }),
                            button(text("×").size(15))
                                .on_press(Message::CloseSessionTab(session.id))
                                .height(30)
                                .padding([4, 8])
                                .style(tab_close_style),
                        ]
                        .spacing(0)
                        .align_y(Center),
                    )
                    .style(move |theme| session_tab_group_style(theme, active)),
                );
            }
        }
        if self.open_sessions.is_empty() {
            tabs = tabs.push(
                text("Select a session to open it")
                    .size(UI_META_SIZE)
                    .style(text::secondary),
            );
        }
        container(
            row![
                scrollable(tabs)
                    .direction(scrollable::Direction::Horizontal(
                        scrollable::Scrollbar::hidden(),
                    ))
                    .height(ACTION_HEIGHT)
                    .width(Fill),
                self.session_actions(),
            ]
            .spacing(8)
            .align_y(Center),
        )
        .height(SESSION_TAB_HEIGHT)
        .padding([4, 8])
        .width(Fill)
        .style(tab_strip_surface)
        .into()
    }

    fn session_actions(&self) -> Element<'_, Message> {
        let Some(session_id) = self.active_session_id else {
            return text("No session selected").style(text::secondary).into();
        };
        let Some(session) = self.session(session_id) else {
            return text("Session is no longer available")
                .style(text::secondary)
                .into();
        };
        let attached = self
            .terminals
            .get(&session_id)
            .is_some_and(|terminal| terminal.attached);
        let can_stop = session_can_stop(session.state);
        let session_state = session.state;
        let state_label = session_state_label(session.state);
        responsive(move |size| {
            let compact = size.width < 260.0;
            let mut actions = row![].spacing(4);
            if let Some(label) = session_terminal_action_label(attached, session_state, compact) {
                let message = if attached {
                    Message::Detach
                } else {
                    Message::Attach
                };
                let style = if attached {
                    chrome_action_style
                } else {
                    primary_action_style
                };
                actions = actions.push(
                    button(
                        text(label)
                            .font(UI_MEDIUM)
                            .size(UI_META_SIZE)
                            .wrapping(text::Wrapping::None),
                    )
                    .on_press(message)
                    .height(ACTION_HEIGHT)
                    .padding([6, 9])
                    .style(style),
                );
            }
            if can_stop {
                actions = actions.push(
                    button(text("Stop").font(UI_MEDIUM).size(UI_META_SIZE))
                        .on_press(Message::Stop)
                        .height(ACTION_HEIGHT)
                        .padding([6, 9])
                        .style(danger_action_style),
                );
            } else if !compact {
                actions = actions.push(
                    container(
                        text(format!("History retained · {state_label}"))
                            .size(UI_META_SIZE)
                            .wrapping(text::Wrapping::None)
                            .style(text::secondary),
                    )
                    .height(ACTION_HEIGHT)
                    .align_y(Vertical::Center)
                    .clip(true),
                );
            }
            actions.into()
        })
        .width(Length::Shrink)
        .height(ACTION_HEIGHT)
        .into()
    }

    fn terminal_view(&self) -> Element<'_, Message> {
        let Some(session_id) = self.active_session_id else {
            return observed_terminal_viewport(centered_message(
                "Choose a session, then open its terminal.",
            ));
        };
        let Some(terminal) = self.terminals.get(&session_id) else {
            if let Some(session) = self.session(session_id)
                && matches!(
                    session.state,
                    SessionState::Terminated | SessionState::Disconnected
                )
            {
                return observed_terminal_viewport(centered_message(
                    "This session is no longer running. Details are retained, but terminal output is not persisted.",
                ));
            }
            return observed_terminal_viewport(centered_action(
                "Session ready. Open its terminal to begin.",
                "Open terminal",
                Message::Attach,
            ));
        };
        let scrollback_rows = terminal.scrollback_rows();
        let role = if scrollback_rows > 0 {
            format!("Viewing history  ·  {scrollback_rows} lines above latest")
        } else if !terminal.attached {
            "Terminal closed  ·  select Open terminal to reconnect".to_owned()
        } else if terminal.role == AttachmentRole::Observer {
            "Read only  ·  this session is controlled in another window".to_owned()
        } else if self.terminal_focus.is_focused() {
            "Input active  ·  drag to select  ·  Ctrl+] leaves terminal".to_owned()
        } else {
            "Click to type  ·  drag to select terminal text".to_owned()
        };
        let terminal_font = terminal_font(self.desktop_state.terminal_font);
        let spans = terminal_spans(
            terminal_display_runs(terminal, self.terminal_focus.is_focused()),
            &self.theme(),
            terminal_font,
        );
        let mut terminal_header = row![
            text(role)
                .font(UI_MEDIUM)
                .size(UI_META_SIZE)
                .style(text::secondary),
            space::horizontal(),
        ]
        .align_y(Center);
        if scrollback_rows > 0 {
            terminal_header = terminal_header.push(
                button(text("Latest").font(UI_MEDIUM).size(UI_META_SIZE))
                    .on_press(Message::LatestTerminal)
                    .height(24)
                    .padding([3, 8])
                    .style(primary_action_style),
            );
        }
        let surface = container(
            column![
                container(terminal_header)
                    .height(26)
                    .width(Fill)
                    .align_y(Vertical::Center),
                container(
                    rich_text(spans)
                        .on_link_click(|()| Message::TerminalSelectionStarted)
                        .font(terminal_font)
                        .size(u32::from(self.desktop_state.terminal_font_size))
                        .line_height(TERMINAL_LINE_HEIGHT_RATIO)
                        .wrapping(text::Wrapping::None)
                        .width(Fill),
                )
                .height(Fill)
                .width(Fill),
            ]
            .spacing(4),
        )
        .padding([8, 12])
        .width(Fill)
        .height(Fill)
        .style(move |theme| terminal_surface(theme, self.terminal_focus.is_focused()));
        let viewport = observed_terminal_viewport(
            mouse_area(surface)
                .on_move(Message::TerminalPointerMoved)
                .on_press(Message::TerminalSelectionStarted)
                .on_release(Message::TerminalSelectionEnded)
                .on_scroll(Message::ScrollTerminal)
                .into(),
        );
        let scrollbar = terminal_scrollbar(terminal);
        row![viewport, scrollbar].width(Fill).height(Fill).into()
    }

    fn changes_view(&self) -> Element<'_, Message> {
        let body = self.diff.as_deref().unwrap_or(
            "Select a checkout and open Changes to load its bounded, read-only Git diff.",
        );
        container(column![
            container(
                text("Read-only Git changes · running sessions continue, but terminal input is disabled in this tab")
                    .font(UI_MEDIUM)
                    .size(UI_META_SIZE)
                    .style(text::secondary)
            )
            .height(28)
            .align_y(Vertical::Center),
            scrollable(
                text(body)
                    .font(terminal_font(self.desktop_state.terminal_font))
                    .size(u32::from(self.desktop_state.terminal_font_size)),
            )
            .height(Fill),
        ].spacing(6))
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
                .push(detail("Repository", project.name.clone()))
                .push(detail(
                    "Repository",
                    project.canonical_repository_path.clone(),
                ));
        }
        if let Some(worktree) = self.selected_worktree() {
            content = content
                .push(detail("Checkout", worktree.name.clone()))
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
                .push(detail("State", session_state_label(session.state).into()))
                .push(detail("Working directory", session.cwd.clone()))
                .push(detail(
                    "Started (Unix ms)",
                    optional_number(session.started_at, "Not started"),
                ))
                .push(detail(
                    "Ended (Unix ms)",
                    optional_number(session.ended_at, "Not ended"),
                ))
                .push(detail(
                    "Exit code",
                    optional_number(session.exit_code, "Not recorded"),
                ))
                .push(detail(
                    "Failure / recovery reason",
                    session
                        .failure_reason
                        .clone()
                        .unwrap_or_else(|| "None".into()),
                ))
                .push(detail(
                    "Terminal output",
                    "Memory-only; unavailable after a daemon restart".into(),
                ))
                .push(detail(
                    "Controller",
                    self.terminals
                        .get(&session.id)
                        .map_or_else(|| "Detached".into(), |terminal| terminal.role.to_string()),
                ))
                .push(detail("Resume", session_resume_label(session).into()));
        }
        let mut actions = row![].spacing(8);
        if self.selected_project_id.is_some() {
            actions = actions.push(button("Rename repository").on_press(Message::RenameProject));
        }
        if self.selected_worktree_id.is_some() {
            actions = actions.push(button("Rename checkout").on_press(Message::RenameWorktree));
        }
        if let Some(session) = self.active_session_id.and_then(|id| self.session(id)) {
            actions = actions.push(self.session_rename_action(session.id));
            if session_can_stop(session.state) {
                actions = actions.push(
                    button("Stop session")
                        .on_press(Message::Stop)
                        .style(flat_danger_style),
                );
            }
        }
        if self.selected_worktree().is_some_and(worktree_can_delete) {
            actions = actions.push(
                button("Delete checkout")
                    .on_press(Message::RemoveSelectedWorktree)
                    .style(flat_danger_style),
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

    fn session_rename_action(&self, session_id: SessionId) -> Element<'_, Message> {
        button(if self.inline_session_rename.is_some() {
            "Renaming…"
        } else {
            "Rename session"
        })
        .on_press_maybe(
            self.inline_session_rename
                .is_none()
                .then_some(Message::BeginSessionRename(session_id)),
        )
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
                detail("Tab / Shift+Tab", "Change navigation section".into()),
                detail("↑ / ↓", "Move through the active section".into()),
                detail("Enter", "Open the selected session".into()),
                detail("1 / 2 / 3", "Terminal / Changes / Details".into()),
                detail("N / R / D", "New / rename / stop or remove".into()),
                detail("O / G", "Open terminal / show Git changes".into()),
                detail(
                    "Wheel / Shift+PageUp",
                    "Read terminal history; Shift+End returns to latest".into(),
                ),
                detail("Ctrl+]", "Leave the focused terminal".into()),
                detail(
                    terminal_clipboard_shortcut(),
                    "Copy selected terminal text / paste from the clipboard".into(),
                ),
                text("Shortcuts are paused while a form is open. When the terminal says “Input active”, ordinary keys go to the running process.")
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
        let theme_choices = [
            DesktopTheme::System,
            DesktopTheme::Light,
            DesktopTheme::Dark,
            DesktopTheme::Nord,
            DesktopTheme::TokyoNight,
            DesktopTheme::Catppuccin,
            DesktopTheme::Dracula,
            DesktopTheme::GruvboxDark,
            DesktopTheme::SolarizedLight,
            DesktopTheme::SolarizedDark,
        ];
        let mut theme_rows = column![].spacing(8);
        for choices in theme_choices.chunks(5) {
            let mut themes = row![].spacing(8);
            for &choice in choices {
                let selected = self.desktop_state.theme == choice;
                themes = themes.push(
                    button(theme::label(choice))
                        .on_press(Message::SelectTheme(choice))
                        .style(move |theme, status| content_tab_style(theme, status, selected)),
                );
            }
            theme_rows = theme_rows.push(themes);
        }
        let mut terminal_fonts = row![].spacing(8);
        for choice in [
            DesktopTerminalFont::System,
            DesktopTerminalFont::JetBrainsMono,
            DesktopTerminalFont::CascadiaCode,
            DesktopTerminalFont::FiraCode,
        ] {
            let selected = self.desktop_state.terminal_font == choice;
            terminal_fonts = terminal_fonts.push(
                button(terminal_font_label(choice))
                    .on_press(Message::SelectTerminalFont(choice))
                    .style(move |theme, status| content_tab_style(theme, status, selected)),
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
                text("Appearance changes apply immediately and persist locally for this desktop.")
                    .style(text::secondary),
                theme_rows,
                row![
                    text("Density").width(Length::Fixed(120.0)),
                    button("Comfortable")
                        .on_press(Message::SelectDensity(DesktopDensity::Comfortable))
                        .style(move |theme, status| content_tab_style(theme, status, self.desktop_state.density == DesktopDensity::Comfortable)),
                    button("Compact")
                        .on_press(Message::SelectDensity(DesktopDensity::Compact))
                        .style(move |theme, status| content_tab_style(theme, status, self.desktop_state.density == DesktopDensity::Compact)),
                ].spacing(8).align_y(Center),
                row![
                    text("Terminal font").width(Length::Fixed(120.0)),
                    terminal_fonts,
                ].spacing(8).align_y(Center),
                row![
                    text(format!("Text size: {} px", self.desktop_state.terminal_font_size)).width(Length::Fixed(180.0)),
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
        if modal.first_run
            && let Some(guidance) = first_run_guidance(form.kind)
        {
            fields = fields.push(text(guidance).style(text::secondary));
        }
        if form.is_session() {
            fields = fields.push(Self::provider_picker(
                form,
                modal.provider_probe,
                modal.pending,
            ));
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
                group = group.push(
                    button("Choose folder…")
                        .on_press_maybe((!modal.pending).then_some(Message::BrowseRepository)),
                );
            }
            fields = fields.push(group);
        }
        if let Some(error) = &form.submission_error {
            fields = fields.push(text(error).style(text::danger));
        }
        let selected_provider_checking = modal.provider_probe.is_some_and(|kind| {
            form.provider()
                .is_some_and(|provider| provider.kind == kind)
        });
        let submit = form_submit_label(form, modal.pending, selected_provider_checking);
        container(
            column![
                text(&form.title).font(UI_SEMIBOLD).size(22),
                fields,
                row![
                    space::horizontal(),
                    button("Cancel")
                        .on_press_maybe((!modal.pending).then_some(Message::CancelModal)),
                    button(submit)
                        .on_press_maybe(
                            (!modal.pending && !selected_provider_checking)
                                .then_some(Message::SubmitForm),
                        )
                        .style(primary_action_style),
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

    fn provider_picker(
        form: &Form,
        probing: Option<ProviderKind>,
        form_pending: bool,
    ) -> Element<'_, Message> {
        let provider = form.provider();
        let provider_text = provider.map_or_else(
            || "No providers".into(),
            |item| {
                let status = if !item.available {
                    "unavailable"
                } else if item.kind == ProviderKind::Codex && !item.authenticated {
                    "available, sign-in required"
                } else {
                    "ready"
                };
                format!("{} — {status}", item.kind)
            },
        );
        let recovery = provider.map_or_else(
            || "The daemon returned no providers. Check again to retry discovery.".into(),
            provider_recovery_message,
        );
        let selected = provider.map(|provider| provider.kind);
        let mut choices = row![].spacing(8);
        for kind in [ProviderKind::Shell, ProviderKind::Codex] {
            if form.providers.iter().any(|provider| provider.kind == kind) {
                let is_selected = selected == Some(kind);
                choices = choices.push(
                    button(text(kind.to_string()).font(UI_MEDIUM))
                        .on_press_maybe((!form_pending).then_some(Message::SelectProvider(kind)))
                        .style(move |theme, status| content_tab_style(theme, status, is_selected)),
                );
            }
        }
        let checking_selected = probing.is_some() && probing == selected;
        let retry_needed = provider.is_none_or(|provider| {
            !provider.available || (provider.kind == ProviderKind::Codex && !provider.authenticated)
        });
        let recovery_row = if retry_needed {
            row![
                text(recovery).width(Fill).style(text::secondary),
                button(if checking_selected {
                    "Checking…"
                } else {
                    "Retry discovery"
                })
                .on_press_maybe(
                    (!checking_selected && !form_pending).then_some(Message::ProbeProvider),
                ),
            ]
            .spacing(8)
            .align_y(Center)
        } else {
            row![text(recovery).width(Fill).style(text::secondary)]
        };
        column![
            text("Session type").font(UI_MEDIUM).size(UI_META_SIZE),
            choices,
            row![text(provider_text).width(Fill),]
                .spacing(8)
                .align_y(Center),
            recovery_row,
        ]
        .spacing(10)
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
                "Delete clean checkout",
                format!(
                    "Delete checkout “{name}” at {canonical_path}? The checkout directory is removed without force; the Git branch is preserved."
                ),
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
                        .style(danger_action_style),
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
        let compact = self.desktop_state.window_width < 900;
        let workspace = self
            .active_workspace()
            .map_or("No workspace", |workspace| workspace.name.as_str());
        let checkout = self.selected_worktree().map_or_else(
            || "No checkout".to_owned(),
            |worktree| {
                format!(
                    "{} · {}",
                    worktree.name,
                    worktree.branch.as_deref().unwrap_or("detached")
                )
            },
        );
        let connection: Element<'_, Message> = match self.connection {
            ConnectionState::Connecting => text("● Connecting")
                .size(FOOTER_TEXT_SIZE)
                .style(text::warning)
                .into(),
            ConnectionState::Connected => text("● Connected")
                .size(FOOTER_TEXT_SIZE)
                .style(text::success)
                .into(),
            ConnectionState::Disconnected => text("● Disconnected")
                .size(FOOTER_TEXT_SIZE)
                .style(text::danger)
                .into(),
        };
        let mut status = row![
            text(format!("SylvOps {}", env!("CARGO_PKG_VERSION")))
                .font(UI_SEMIBOLD)
                .size(FOOTER_TEXT_SIZE),
            footer_separator(),
            footer_item(workspace),
        ]
        .spacing(9)
        .align_y(Center);
        if !compact {
            status = status.push(footer_separator()).push(footer_item(&checkout));
        }
        status = status.push(space::horizontal()).push(connection);
        if !compact {
            status = status
                .push(footer_separator())
                .push(footer_item("Ctrl+K shortcuts  ·  Ctrl+] leave terminal"));
        }
        container(status)
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
                    self.modal = Some(Modal::Form(FormModal::first_run(Form::workspace())));
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
                if matches!(&operation, Some(Operation::ProbeProvider(_))) {
                    if let Some(Modal::Form(modal)) = &mut self.modal {
                        modal.provider_probe = None;
                        modal.form.submission_error = Some(message);
                    } else {
                        self.error = Some(message);
                    }
                    return;
                }
                if matches!(&operation, Some(Operation::RenameSession))
                    && let Some(rename) = &mut self.inline_session_rename
                {
                    rename.fail(message);
                    return;
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
            DaemonEvent::ProviderHealthChanged { health } => {
                self.update_provider_health(health);
            }
            DaemonEvent::SessionOutput {
                session_id,
                sequence,
                bytes,
                ..
            } => {
                if let Some(terminal) = self.terminals.get_mut(&session_id)
                    && sequence > terminal.last_sequence
                {
                    terminal.process(&bytes);
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
                    terminal.process(&terminal_snapshot);
                    terminal.last_sequence = snapshot_sequence;
                    terminal.columns = columns;
                    terminal.rows = rows;
                    terminal.prepare_for_input();
                }
            }
            DaemonEvent::SessionExited { session_id, .. } => {
                if let Some(terminal) = self.terminals.get_mut(&session_id) {
                    terminal.attached = false;
                }
                if self.active_session_id == Some(session_id) {
                    self.terminal_focus = TerminalFocus::Unfocused;
                }
                self.request_snapshot();
            }
            _ => self.request_snapshot(),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn handle_response(&mut self, operation: Operation, response: DaemonResponse) {
        let first_run = matches!(
            &self.modal,
            Some(Modal::Form(FormModal {
                first_run: true,
                ..
            }))
        );
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
                self.show_success(if first_run {
                    "Session launched. Select Open terminal to begin."
                } else {
                    "Session created."
                });
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
                    .or_insert_with(|| TerminalState::new(role, rows, columns, 10_000));
                terminal.role = role;
                terminal.attached = true;
                terminal.columns = columns;
                terminal.rows = rows;
                if let Some(snapshot) = terminal_snapshot {
                    terminal.parser = vt100::Parser::new(rows, columns, 10_000);
                    terminal.process(&snapshot);
                    terminal.last_sequence = replay_through_sequence;
                }
                terminal.prepare_for_input();
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
            (Operation::ProbeProvider(kind), DaemonResponse::Provider(health))
                if health.kind == kind =>
            {
                self.update_provider_health(health);
                if let Some(Modal::Form(modal)) = &mut self.modal {
                    modal.provider_probe = None;
                    modal.form.submission_error = None;
                }
            }
            (Operation::CreateWorkspace, DaemonResponse::WorkspaceAdded { workspace, .. }) => {
                self.show_success(format!("Workspace “{}” created.", workspace.name));
                self.modal = if first_run {
                    Some(Modal::Form(FormModal::first_run(Form::repository(
                        workspace.id,
                    ))))
                } else {
                    None
                };
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
                self.show_success("Repository registered.");
                self.modal = if first_run {
                    Some(Modal::Form(FormModal::first_run(Form::session(
                        root_worktree.id,
                        self.providers.clone(),
                    ))))
                } else {
                    None
                };
                self.mark_state_dirty();
                self.request_snapshot();
            }
            (Operation::CreateWorktree, DaemonResponse::WorktreeCreated { worktree, .. }) => {
                self.selected_worktree_id = Some(worktree.id);
                self.selected_session_id = None;
                self.modal = None;
                self.show_success("Checkout created.");
                self.mark_state_dirty();
                self.request_snapshot();
            }
            (Operation::RenameProject, DaemonResponse::ProjectUpdated { .. })
            | (Operation::RenameWorktree, DaemonResponse::WorktreeUpdated { .. }) => {
                self.modal = None;
                self.show_success("Display name updated.");
                self.request_snapshot();
            }
            (Operation::RenameSession, DaemonResponse::SessionUpdated { .. }) => {
                self.inline_session_rename = None;
                self.show_success("Session name updated.");
                self.request_snapshot();
            }
            (Operation::InspectRemoval(worktree_id), DaemonResponse::WorktreeStatus(state)) => {
                if !state.clean {
                    self.error = Some(format!(
                        "Checkout is not clean: {} tracked, {} untracked, and {} ignored entries. SylvOps will not remove it.",
                        state.tracked_changes, state.untracked_files, state.ignored_files,
                    ));
                } else if state.removal_confirmation_token.is_none() {
                    self.error =
                        Some("The daemon did not authorize removal of this checkout.".into());
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
                self.show_success("Checkout deleted; its branch was preserved.");
                self.request_snapshot();
            }
            (operation, DaemonResponse::Error(failure)) => {
                if matches!(operation, Operation::ProbeProvider(_)) {
                    if let Some(Modal::Form(modal)) = &mut self.modal {
                        modal.provider_probe = None;
                        modal.form.submission_error = Some(failure.message);
                    } else {
                        self.error = Some(format!("{operation:?}: {}", failure.message));
                    }
                    return;
                }
                let form_operation = matches!(
                    operation,
                    Operation::CreateWorkspace
                        | Operation::RegisterProject
                        | Operation::CreateWorktree
                        | Operation::CreateSession(_)
                        | Operation::RenameProject
                        | Operation::RenameWorktree
                        | Operation::RenameSession
                );
                if form_operation && let Some(Modal::Form(modal)) = &mut self.modal {
                    modal.pending = false;
                    modal.form.submission_error = Some(failure.message);
                } else {
                    self.error = Some(format!("{operation:?}: {}", failure.message));
                }
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

    fn handle_terminal_clipboard(&mut self, event: &keyboard::Event) -> Option<Task<Message>> {
        if !self.terminal_focus.is_focused() || self.modal.is_some() {
            return None;
        }
        let keyboard::Event::KeyPressed { key, modifiers, .. } = event else {
            return None;
        };
        let shortcut = modifiers.command() && (cfg!(target_os = "macos") || modifiers.shift());
        if !shortcut {
            return None;
        }
        match key.as_ref() {
            Key::Character(value) if value.eq_ignore_ascii_case("c") => {
                let selected = self
                    .active_session_id
                    .and_then(|id| self.terminals.get(&id))
                    .and_then(TerminalState::selected_text);
                Some(selected.map_or_else(Task::none, clipboard::write))
            }
            Key::Character(value) if value.eq_ignore_ascii_case("v") => {
                Some(clipboard::read().map(Message::TerminalPaste))
            }
            _ => None,
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
        if self.handle_transient_keyboard(&key) {
            return;
        }
        if self.terminal_focus.is_focused() {
            let Some(session_id) = self.active_session_id else {
                self.terminal_focus = TerminalFocus::Unfocused;
                return;
            };
            let Some(terminal) = self.terminals.get_mut(&session_id) else {
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
            if modifiers.shift() {
                match key.as_ref() {
                    Key::Named(Named::PageUp) => {
                        terminal.scroll_page(1);
                        return;
                    }
                    Key::Named(Named::PageDown) => {
                        terminal.scroll_page(-1);
                        return;
                    }
                    Key::Named(Named::Home) => {
                        terminal.scroll_to_oldest();
                        return;
                    }
                    Key::Named(Named::End) => {
                        terminal.prepare_for_input();
                        return;
                    }
                    _ => {}
                }
            }
            let Some(bytes) = encode_terminal_key(&key, modifiers, text.as_deref()) else {
                return;
            };
            terminal.prepare_for_input();
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
            Key::Character(value) if is_open_terminal_shortcut(value) => self.attach_active(),
            Key::Character(value) if value.eq_ignore_ascii_case("g") => {
                self.select_main_tab(MainTab::Changes);
            }
            Key::Character("?") => self.modal = Some(Modal::Shortcuts),
            Key::Character(value) if value.eq_ignore_ascii_case("q") => self.begin_close(),
            _ => {}
        }
    }

    fn handle_transient_keyboard(&mut self, key: &Key) -> bool {
        if self.modal.is_some() {
            if matches!(key, Key::Named(Named::Escape)) {
                self.modal = None;
            }
            true
        } else if self.inline_session_rename.is_some() {
            if matches!(key, Key::Named(Named::Escape)) {
                cancel_inline_session_rename(&mut self.inline_session_rename);
            }
            true
        } else {
            false
        }
    }

    fn select_main_tab(&mut self, tab: MainTab) {
        self.unfocus_terminal();
        if self.main_tab == tab {
            return;
        }
        self.main_tab = tab;
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
        if panels[next] != DesktopPanel::Sessions {
            self.inline_session_rename = None;
        }
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
            DesktopPanel::Sessions => {}
        }
    }

    fn shortcut_delete(&mut self) {
        self.terminal_focus = TerminalFocus::Unfocused;
        match self.keyboard_panel {
            DesktopPanel::Projects => {
                self.error = Some("Repository removal is intentionally unavailable.".into());
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
        self.inline_session_rename = None;
        let Some(workspace_id) = self.active_workspace().map(|workspace| workspace.id) else {
            self.error = Some("Create a workspace before registering a repository.".into());
            self.modal = Some(Modal::Form(FormModal::new(Form::workspace())));
            return;
        };
        self.modal = Some(Modal::Form(FormModal::new(Form::repository(workspace_id))));
    }

    fn open_worktree_form(&mut self) {
        self.inline_session_rename = None;
        let Some(project_id) = self.selected_project_id else {
            self.error = Some("Select a repository before creating a checkout.".into());
            return;
        };
        self.modal = Some(Modal::Form(FormModal::new(Form::worktree(project_id))));
    }

    fn open_session_form(&mut self) {
        self.inline_session_rename = None;
        let Some(worktree_id) = self.selected_worktree_id else {
            self.error = Some("Select a checkout before starting a session.".into());
            return;
        };
        self.modal = Some(Modal::Form(FormModal::new(Form::session(
            worktree_id,
            self.providers.clone(),
        ))));
        self.probe_provider(ProviderKind::Codex);
    }

    fn open_project_rename_form(&mut self) {
        let Some(project) = self
            .selected_project_id
            .and_then(|id| self.snapshot.projects.iter().find(|item| item.id == id))
        else {
            self.error = Some("Select a repository to rename.".into());
            return;
        };
        self.modal = Some(Modal::Form(FormModal::new(Form::rename(
            "Rename repository",
            &project.name,
            FormKind::RenameProject(project.id),
        ))));
    }

    fn open_worktree_rename_form(&mut self) {
        let Some(worktree) = self.selected_worktree() else {
            self.error = Some("Select a checkout to rename.".into());
            return;
        };
        self.modal = Some(Modal::Form(FormModal::new(Form::rename(
            "Rename checkout",
            &worktree.name,
            FormKind::RenameWorktree(worktree.id),
        ))));
    }

    fn begin_session_rename(&mut self, session_id: SessionId) -> Task<Message> {
        let Some(session) = self.session(session_id) else {
            self.error = Some("Select a session to rename.".into());
            return Task::none();
        };
        let rename = InlineSessionRename::new(session.id, &session.display_name);
        self.hovered_session_id = None;
        let input_id = rename.input_id.clone();
        self.inline_session_rename = Some(rename);
        self.selected_session_id = Some(session_id);
        self.keyboard_panel = DesktopPanel::Sessions;
        self.desktop_state.compact_panel = DesktopPanel::Sessions;
        self.narrow_main = false;
        self.terminal_focus = TerminalFocus::Unfocused;
        Task::batch([
            widget::operation::focus(input_id.clone()),
            widget::operation::select_all(input_id),
        ])
    }

    fn submit_session_rename(&mut self) {
        let Some((session_id, name)) = self.inline_session_rename.as_mut().and_then(|rename| {
            rename
                .begin_submission()
                .map(|name| (rename.session_id, name))
        }) else {
            return;
        };
        if !self.bridge.request(
            Operation::RenameSession,
            ClientRequest::RenameSession { session_id, name },
        ) && let Some(rename) = &mut self.inline_session_rename
        {
            rename.fail("The desktop IPC command queue is busy. Try again.".into());
        }
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

    fn select_provider(&mut self, kind: ProviderKind) {
        if let Some(Modal::Form(modal)) = &mut self.modal
            && !modal.pending
        {
            modal.form.select_provider(kind);
            modal.form.submission_error = None;
        }
    }

    fn probe_selected_provider(&mut self) {
        let Some(kind) = self.modal.as_ref().and_then(|modal| match modal {
            Modal::Form(modal) => modal.form.provider().map(|provider| provider.kind),
            _ => None,
        }) else {
            if let Some(Modal::Form(modal)) = &mut self.modal {
                modal.form.submission_error =
                    Some("The daemon returned no providers to check.".into());
            }
            return;
        };
        self.probe_provider(kind);
    }

    fn probe_provider(&mut self, kind: ProviderKind) {
        let Some(Modal::Form(modal)) = &mut self.modal else {
            return;
        };
        if modal.pending
            || modal.provider_probe.is_some()
            || !modal
                .form
                .providers
                .iter()
                .any(|provider| provider.kind == kind)
        {
            return;
        }
        modal.provider_probe = Some(kind);
        if modal
            .form
            .provider()
            .is_some_and(|provider| provider.kind == kind)
        {
            modal.form.submission_error = None;
        }
        if !self.bridge.request(
            Operation::ProbeProvider(kind),
            ClientRequest::ProbeProvider { kind },
        ) && let Some(Modal::Form(modal)) = &mut self.modal
        {
            modal.provider_probe = None;
            modal.form.submission_error =
                Some("The desktop IPC command queue is busy. Try again.".into());
        }
    }

    fn update_provider_health(&mut self, health: ProviderHealth) {
        if let Some(provider) = self
            .providers
            .iter_mut()
            .find(|provider| provider.kind == health.kind)
        {
            provider.clone_from(&health);
        } else {
            self.providers.push(health.clone());
        }
        if let Some(Modal::Form(modal)) = &mut self.modal {
            if let Some(provider) = modal
                .form
                .providers
                .iter_mut()
                .find(|provider| provider.kind == health.kind)
            {
                provider.clone_from(&health);
            } else if modal.form.is_session() {
                modal.form.providers.push(health);
            }
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
        if !session_can_stop(session.state) {
            self.error = Some(format!(
                "Session “{}” is already {}; its history remains available.",
                session.display_name,
                session_state_label(session.state)
            ));
            return;
        }
        self.modal = Some(Modal::Confirmation(Confirmation::StopSession {
            session_name: session.display_name.clone(),
            cwd: session.cwd.clone(),
        }));
    }

    fn inspect_worktree_removal(&mut self) {
        let Some(worktree) = self.selected_worktree() else {
            self.error = Some("Select a managed checkout first.".into());
            return;
        };
        if worktree.is_root_checkout {
            self.error = Some("The root checkout cannot be deleted by SylvOps.".into());
            return;
        }
        if worktree.status != WorktreeStatus::Active {
            self.error = Some("Only an active managed checkout can be deleted.".into());
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
            self.error = Some("Select a session before opening its terminal.".into());
            return;
        };
        let Some(session) = self.session(session_id) else {
            self.error = Some("The selected session no longer exists.".into());
            return;
        };
        if !session_can_stop(session.state) && !session_can_replay(session.state) {
            self.error = Some(format!(
                "Session “{}” is {}; only its metadata is retained.",
                session.display_name,
                session_state_label(session.state)
            ));
            return;
        }
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
        let Some(terminal) = self.terminals.get_mut(&session_id) else {
            self.error = Some("Select Open terminal before typing here.".into());
            return;
        };
        if !terminal.attached {
            self.error = Some("This terminal is closed. Select Open terminal to reconnect.".into());
            return;
        }
        if terminal.role != AttachmentRole::Controller {
            self.error = Some(
                "This terminal is read-only because another client controls the session.".into(),
            );
            return;
        }
        self.terminal_focus = TerminalFocus::Focused;
        self.error = None;
    }

    fn unfocus_terminal(&mut self) {
        self.terminal_focus = TerminalFocus::Unfocused;
        self.terminal_selection_state = TerminalSelectionState::Idle;
    }

    fn move_terminal_pointer(&mut self, point: Point) {
        self.terminal_pointer = Some(point);
        if self.terminal_selection_state != TerminalSelectionState::Selecting {
            return;
        }
        let Some((row, column)) = terminal_point_to_cell(
            point,
            self.desktop_state.terminal_font_size,
            self.active_session_id
                .and_then(|id| self.terminals.get(&id))
                .map_or((0, 0), |terminal| (terminal.rows, terminal.columns)),
        ) else {
            return;
        };
        if let Some(terminal) = self
            .active_session_id
            .and_then(|id| self.terminals.get_mut(&id))
        {
            terminal.update_selection(row, column);
        }
    }

    fn start_terminal_selection(&mut self) {
        self.focus_terminal();
        let Some(point) = self.terminal_pointer else {
            return;
        };
        let dimensions = self
            .active_session_id
            .and_then(|id| self.terminals.get(&id))
            .map_or((0, 0), |terminal| (terminal.rows, terminal.columns));
        let Some((row, column)) =
            terminal_point_to_cell(point, self.desktop_state.terminal_font_size, dimensions)
        else {
            return;
        };
        if let Some(terminal) = self
            .active_session_id
            .and_then(|id| self.terminals.get_mut(&id))
        {
            terminal.begin_selection(row, column);
            self.terminal_selection_state = TerminalSelectionState::Selecting;
        }
    }

    fn finish_terminal_selection(&mut self) {
        self.terminal_selection_state = TerminalSelectionState::Idle;
        if let Some(terminal) = self
            .active_session_id
            .and_then(|id| self.terminals.get_mut(&id))
        {
            terminal.finish_selection();
        }
    }

    fn paste_terminal(&mut self, contents: Option<String>) {
        let Some(contents) = contents.filter(|contents| !contents.is_empty()) else {
            return;
        };
        let Some(session_id) = self.active_session_id else {
            return;
        };
        let Some(terminal) = self.terminals.get_mut(&session_id) else {
            return;
        };
        if !terminal.attached || terminal.role != AttachmentRole::Controller {
            return;
        }
        let bytes = encode_terminal_paste(&contents, terminal.parser.screen().bracketed_paste());
        terminal.prepare_for_input();
        self.send_request(
            Operation::Input(session_id),
            ClientRequest::SessionInput { session_id, bytes },
        );
    }

    fn scroll_terminal(&mut self, delta: mouse::ScrollDelta) {
        let Some(session_id) = self.active_session_id else {
            return;
        };
        let font_size = f32::from(self.desktop_state.terminal_font_size);
        let lines = match delta {
            mouse::ScrollDelta::Lines { y, .. } => y * 3.0,
            mouse::ScrollDelta::Pixels { y, .. } => y / (font_size + 3.0),
        };
        let pointer = self.terminal_pointer;
        let action = self.terminals.get_mut(&session_id).map(|terminal| {
            let (row, column) = pointer
                .and_then(|point| {
                    terminal_point_to_cell(
                        point,
                        self.desktop_state.terminal_font_size,
                        (terminal.rows, terminal.columns),
                    )
                })
                .unwrap_or_else(|| terminal.parser.screen().cursor_position());
            terminal.wheel_action(lines, row, column)
        });
        match action {
            Some(WheelAction::Application(bytes)) if !bytes.is_empty() => self.send_request(
                Operation::Input(session_id),
                ClientRequest::SessionInput { session_id, bytes },
            ),
            Some(WheelAction::Local) => {
                if let Some(terminal) = self.terminals.get_mut(&session_id) {
                    terminal.clear_selection();
                    terminal.scroll_lines(lines);
                }
            }
            Some(WheelAction::Application(_)) | None => {}
        }
    }

    fn set_terminal_scrollback(&mut self, position: u32) {
        let Some(session_id) = self.active_session_id else {
            return;
        };
        if let Some(terminal) = self.terminals.get_mut(&session_id) {
            terminal.set_scrollback_position(position as usize);
        }
    }

    fn show_latest_terminal(&mut self) {
        let Some(session_id) = self.active_session_id else {
            return;
        };
        if let Some(terminal) = self.terminals.get_mut(&session_id) {
            terminal.prepare_for_input();
        }
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
            terminal.resize(rows, columns);
            terminal.prepare_for_input();
        }
        self.pending_resize = Some((session_id, columns, rows, Instant::now()));
    }

    fn terminal_dimensions(&self) -> (u16, u16) {
        if let Some(viewport) = self.terminal_viewport {
            return terminal_grid_dimensions(viewport, self.desktop_state.terminal_font_size);
        }
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
        retain_inline_session_rename(
            &mut self.inline_session_rename,
            self.snapshot.sessions.iter().map(|session| session.id),
        );
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
        self.unfocus_terminal();
        if self.selected_project_id == Some(project_id) {
            return;
        }
        self.inline_session_rename = None;
        self.keyboard_panel = DesktopPanel::Projects;
        self.desktop_state.compact_panel = DesktopPanel::Projects;
        self.selected_project_id = Some(project_id);
        self.selected_worktree_id = self
            .worktrees_for(project_id)
            .first()
            .map(|worktree| worktree.id);
        self.selected_session_id = self
            .selected_worktree_id
            .and_then(|id| self.sessions_for(id).first().map(|session| session.id));
        self.activate_selected_session_context();
        self.diff = None;
        self.mark_state_dirty();
    }

    fn select_worktree(&mut self, worktree_id: WorktreeId) {
        self.unfocus_terminal();
        if self.selected_worktree_id == Some(worktree_id) {
            return;
        }
        self.inline_session_rename = None;
        self.keyboard_panel = DesktopPanel::Worktrees;
        self.desktop_state.compact_panel = DesktopPanel::Worktrees;
        self.selected_worktree_id = Some(worktree_id);
        self.selected_session_id = self
            .sessions_for(worktree_id)
            .first()
            .map(|session| session.id);
        self.activate_selected_session_context();
        self.diff = None;
        self.mark_state_dirty();
    }

    fn select_session(&mut self, session_id: SessionId) {
        self.unfocus_terminal();
        if self.selected_session_id == Some(session_id)
            && self.active_session_id == Some(session_id)
        {
            return;
        }
        if self
            .inline_session_rename
            .as_ref()
            .is_some_and(|rename| rename.session_id != session_id)
        {
            self.inline_session_rename = None;
        }
        self.keyboard_panel = DesktopPanel::Sessions;
        self.desktop_state.compact_panel = DesktopPanel::Sessions;
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

    fn activate_selected_session_context(&mut self) {
        let Some(session_id) = self.selected_session_id else {
            self.active_session_id = None;
            self.main_tab = MainTab::Details;
            return;
        };
        self.active_session_id = Some(session_id);
        if !self.open_sessions.contains(&session_id) {
            if self.open_sessions.len() == MAX_OPEN_DESKTOP_SESSIONS {
                self.open_sessions.remove(0);
            }
            self.open_sessions.push(session_id);
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

fn optional_number<T: ToString>(value: Option<T>, fallback: &str) -> String {
    value.map_or_else(|| fallback.to_owned(), |value| value.to_string())
}

const fn session_resume_label(session: &Session) -> &'static str {
    if session.external_session_id.is_some() && state_allows_resume(session.state) {
        "Available from the CLI"
    } else {
        "Unavailable"
    }
}

const fn fullscreen_label(mode: window::Mode, compact: bool) -> &'static str {
    match (mode, compact) {
        (window::Mode::Fullscreen, true) => "Window",
        (window::Mode::Fullscreen, false) => "Exit full screen",
        (_, true) => "Full",
        (_, false) => "Full screen",
    }
}

const fn terminal_clipboard_shortcut() -> &'static str {
    if cfg!(target_os = "macos") {
        "⌘C / ⌘V"
    } else {
        "Ctrl+Shift+C / V"
    }
}

const fn terminal_font_label(choice: DesktopTerminalFont) -> &'static str {
    match choice {
        DesktopTerminalFont::System => "System mono",
        DesktopTerminalFont::JetBrainsMono => "JetBrains Mono",
        DesktopTerminalFont::CascadiaCode => "Cascadia Code",
        DesktopTerminalFont::FiraCode => "Fira Code",
    }
}

const fn terminal_font(choice: DesktopTerminalFont) -> Font {
    match choice {
        DesktopTerminalFont::System => SYSTEM_TERMINAL_FONT,
        DesktopTerminalFont::JetBrainsMono => Font::with_name("JetBrains Mono"),
        DesktopTerminalFont::CascadiaCode => Font::with_name("Cascadia Code"),
        DesktopTerminalFont::FiraCode => Font::with_name("Fira Code"),
    }
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
    let label = label.to_owned();
    let content = container(
        text(label.clone())
            .font(if selected { UI_MEDIUM } else { UI_FONT })
            .size(UI_TEXT_SIZE)
            .wrapping(text::Wrapping::None)
            .width(Fill),
    )
    .width(Fill)
    .clip(true);
    let action = button(content)
        .on_press(message)
        .style(move |theme, status| list_item_style(theme, status, selected))
        .width(Fill)
        .height(height)
        .padding(padding);
    action.into()
}

const fn worktree_can_delete(worktree: &Worktree) -> bool {
    checkout_delete_available(worktree.is_root_checkout, worktree.status)
}

const fn checkout_delete_available(is_root_checkout: bool, status: WorktreeStatus) -> bool {
    !is_root_checkout && matches!(status, WorktreeStatus::Active)
}

fn tab_button(label: &str, tab: MainTab, active: MainTab) -> Element<'_, Message> {
    let selected = tab == active;
    button(
        text(label)
            .font(if selected { UI_SEMIBOLD } else { UI_FONT })
            .size(UI_TEXT_SIZE)
            .wrapping(text::Wrapping::None),
    )
    .on_press(Message::SelectMainTab(tab))
    .height(ACTION_HEIGHT)
    .padding([6, 14])
    .style(move |theme, status| content_tab_style(theme, status, selected))
    .into()
}

fn compact_panel_button(
    label: &str,
    panel: DesktopPanel,
    active: DesktopPanel,
) -> Element<'_, Message> {
    let selected = panel == active;
    button(
        text(label)
            .font(if selected { UI_SEMIBOLD } else { UI_FONT })
            .size(UI_META_SIZE)
            .wrapping(text::Wrapping::None),
    )
    .on_press(Message::SelectCompactPanel(panel))
    .height(32)
    .padding([5, 9])
    .style(move |theme, status| content_tab_style(theme, status, selected))
    .into()
}

fn trimmed_option(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn form_submit_label(form: &Form, pending: bool, selected_provider_checking: bool) -> &'static str {
    if pending {
        return "Working…";
    }
    if selected_provider_checking {
        return "Checking Codex…";
    }
    match form.kind {
        FormKind::CreateWorkspace => "Create workspace",
        FormKind::RegisterProject(_) => "Register repository",
        FormKind::CreateWorktree(_) => "Create checkout",
        FormKind::CreateSession(_) => match form.provider().map(|provider| provider.kind) {
            Some(ProviderKind::Codex) => "Start Codex",
            Some(ProviderKind::Shell) => "Open Shell",
            Some(_) | None => "Start session",
        },
        FormKind::RenameProject(_) | FormKind::RenameWorktree(_) | FormKind::RenameSession(_) => {
            "Save name"
        }
    }
}

fn first_run_guidance(kind: FormKind) -> Option<&'static str> {
    match kind {
        FormKind::CreateWorkspace => Some(
            "Step 1 of 3 · A Workspace is a local group of repositories you supervise together.",
        ),
        FormKind::RegisterProject(_) => Some(
            "Step 2 of 3 · Add an existing Git Repository. SylvOps records it as a Project and registers its current checkout as the root Worktree.",
        ),
        FormKind::CreateSession(_) => Some(
            "Step 3 of 3 · Choose a Checkout (the root or a managed Worktree), then start a Shell or Codex Session inside it.",
        ),
        FormKind::CreateWorktree(_)
        | FormKind::RenameProject(_)
        | FormKind::RenameWorktree(_)
        | FormKind::RenameSession(_) => None,
    }
}

fn provider_recovery_message(provider: &ProviderHealth) -> String {
    if !provider.available {
        let diagnostic = provider
            .diagnostic
            .as_deref()
            .map_or_else(String::new, |message| format!(" ({message})"));
        return match provider.kind {
            ProviderKind::Codex => format!(
                "Codex is unavailable{diagnostic}. Install Codex, then retry discovery. Shell remains available now."
            ),
            _ => format!(
                "{} is unavailable{diagnostic}. Fix its local configuration, then check again.",
                provider.kind
            ),
        };
    }
    if provider.kind == ProviderKind::Codex && !provider.authenticated {
        return "Sign in to Codex on this computer, then retry discovery. Shell remains available now.".into();
    }
    format!("{} is ready.", provider.kind)
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

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn terminal_grid_dimensions(viewport: iced::Size, font_size: u8) -> (u16, u16) {
    let screen_width = (viewport.width - TERMINAL_HORIZONTAL_PADDING).max(1.0);
    let screen_height = (viewport.height
        - TERMINAL_VERTICAL_PADDING
        - TERMINAL_HEADER_HEIGHT
        - TERMINAL_HEADER_SPACING)
        .max(1.0);
    let font_size = f32::from(font_size);
    let cell_width = (font_size * TERMINAL_CELL_WIDTH_RATIO).max(1.0);
    let line_height = (font_size * TERMINAL_LINE_HEIGHT_RATIO).max(1.0);
    let columns = (screen_width / cell_width).floor().clamp(1.0, 500.0) as u16;
    let rows = (screen_height / line_height).floor().clamp(1.0, 200.0) as u16;
    (columns, rows)
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn terminal_point_to_cell(
    point: Point,
    font_size: u8,
    (rows, columns): (u16, u16),
) -> Option<(u16, u16)> {
    if rows == 0 || columns == 0 {
        return None;
    }
    let x = point.x - TERMINAL_HORIZONTAL_PADDING / 2.0;
    let y = point.y
        - TERMINAL_VERTICAL_PADDING / 2.0
        - TERMINAL_HEADER_HEIGHT
        - TERMINAL_HEADER_SPACING;
    if x < 0.0 || y < 0.0 {
        return None;
    }
    let font_size = f32::from(font_size);
    let column = (x / (font_size * TERMINAL_CELL_WIDTH_RATIO).max(1.0)).floor() as u16;
    let row = (y / (font_size * TERMINAL_LINE_HEIGHT_RATIO).max(1.0)).floor() as u16;
    (row < rows && column < columns).then_some((row, column))
}

fn terminal_spans(
    runs: Vec<DisplayRun>,
    theme: &Theme,
    terminal_font: Font,
) -> Vec<text::Span<'static, (), Font>> {
    let palette = theme.extended_palette();
    let default_foreground = palette.background.base.text;
    let default_background = palette.background.base.color;

    runs.into_iter()
        .map(|run| {
            let mut foreground = terminal_color(run.style.foreground, default_foreground);
            let mut background = terminal_color(run.style.background, default_background);
            if run.style.inverse {
                std::mem::swap(&mut foreground, &mut background);
            }
            if run.style.dim {
                foreground = foreground.scale_alpha(0.62);
            }
            if run.style.selected {
                foreground = palette.primary.weak.text;
                background = palette.primary.weak.color;
            }
            if run.style.cursor {
                foreground = palette.primary.strong.text;
                background = palette.primary.strong.color;
            }
            let mut font = terminal_font;
            if run.style.bold {
                font.weight = Weight::Bold;
            }
            if run.style.italic {
                font.style = font::Style::Italic;
            }
            let mut span: text::Span<'static, (), Font> = span(run.text)
                .font(font)
                .color(foreground)
                .underline(run.style.underline);
            if background != default_background {
                span = span.background(background);
            }
            span
        })
        .collect()
}

fn terminal_color(color: vt100::Color, default: Color) -> Color {
    match color {
        vt100::Color::Default => default,
        vt100::Color::Rgb(red, green, blue) => Color::from_rgb8(red, green, blue),
        vt100::Color::Idx(index) => indexed_terminal_color(index),
    }
}

fn indexed_terminal_color(index: u8) -> Color {
    const ANSI: [(u8, u8, u8); 16] = [
        (30, 30, 30),
        (241, 76, 76),
        (35, 209, 139),
        (229, 229, 16),
        (59, 142, 234),
        (214, 112, 214),
        (41, 184, 219),
        (229, 229, 229),
        (102, 102, 102),
        (241, 76, 76),
        (35, 209, 139),
        (245, 245, 67),
        (59, 142, 234),
        (214, 112, 214),
        (41, 184, 219),
        (255, 255, 255),
    ];
    let (red, green, blue) = match index {
        0..=15 => ANSI[usize::from(index)],
        16..=231 => {
            let offset = index - 16;
            let levels = [0, 95, 135, 175, 215, 255];
            (
                levels[usize::from(offset / 36)],
                levels[usize::from((offset % 36) / 6)],
                levels[usize::from(offset % 6)],
            )
        }
        232..=255 => {
            let value = 8 + (index - 232) * 10;
            (value, value, value)
        }
    };
    Color::from_rgb8(red, green, blue)
}

fn observed_terminal_viewport(content: Element<'_, Message>) -> Element<'_, Message> {
    sensor(content)
        .on_resize(Message::TerminalViewportResized)
        .into()
}

fn terminal_scrollbar(terminal: &TerminalState) -> Element<'_, Message> {
    let extent = terminal.scrollback_extent();
    if extent == 0 {
        return container(space::vertical())
            .width(TERMINAL_SCROLLBAR_WIDTH)
            .height(Fill)
            .into();
    }
    let maximum = u32::try_from(extent).unwrap_or(u32::MAX);
    let position = u32::try_from(terminal.scrollback_rows())
        .unwrap_or(u32::MAX)
        .min(maximum);
    container(
        vertical_slider(0..=maximum, position, Message::SetTerminalScrollback)
            .default(0_u32)
            .width(TERMINAL_SCROLLBAR_WIDTH)
            .height(Fill),
    )
    .width(TERMINAL_SCROLLBAR_WIDTH)
    .height(Fill)
    .into()
}

fn centered_message(message: &str) -> Element<'_, Message> {
    container(text(message).style(text::secondary))
        .width(Fill)
        .height(Fill)
        .center(Fill)
        .into()
}

fn centered_action(
    message: &'static str,
    label: &'static str,
    action: Message,
) -> Element<'static, Message> {
    container(
        column![
            text(message).style(text::secondary),
            button(label).on_press(action).style(primary_action_style),
        ]
        .spacing(12)
        .align_x(Center),
    )
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

fn empty_action(
    message: &'static str,
    label: &'static str,
    action: Message,
) -> Element<'static, Message> {
    container(
        column![
            text(message).size(UI_META_SIZE).style(text::secondary),
            button(label).on_press(action).style(chrome_action_style),
        ]
        .spacing(8),
    )
    .padding(8)
    .width(Fill)
    .into()
}

fn footer_item(label: &str) -> Element<'static, Message> {
    text(label.to_owned())
        .font(UI_FONT)
        .size(FOOTER_TEXT_SIZE)
        .style(text::secondary)
        .into()
}

fn footer_separator() -> Element<'static, Message> {
    text("/")
        .size(FOOTER_TEXT_SIZE)
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
    let pair = palette.primary.weak;
    container::Style {
        background: Some(Background::Color(pair.color)),
        text_color: Some(contrast_safe_text(pair.color, pair.text)),
        border: Border {
            width: 1.0,
            radius: 4.0.into(),
            color: palette.primary.base.color,
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
    let pair = match status {
        Status::Hovered => palette.background.weak,
        Status::Pressed => palette.background.neutral,
        Status::Active | Status::Disabled => palette.background.base,
    };
    button::Style {
        background: matches!(status, Status::Hovered | Status::Pressed)
            .then_some(Background::Color(pair.color)),
        text_color: pair.text.scale_alpha(if status == Status::Disabled {
            0.45
        } else {
            1.0
        }),
        border: Border {
            radius: 4.0.into(),
            ..Border::default()
        },
        ..button::Style::default()
    }
}

fn primary_action_style(theme: &Theme, status: Status) -> button::Style {
    let palette = theme.extended_palette();
    let pair = if status == Status::Pressed {
        palette.primary.base
    } else {
        palette.primary.weak
    };
    button::Style {
        background: Some(Background::Color(pair.color)),
        text_color: contrast_safe_text(pair.color, pair.text).scale_alpha(
            if status == Status::Disabled {
                0.45
            } else {
                1.0
            },
        ),
        border: Border {
            radius: 4.0.into(),
            color: palette.primary.base.color,
            width: if status == Status::Hovered { 2.0 } else { 1.0 },
        },
        ..button::Style::default()
    }
}

fn danger_action_style(theme: &Theme, status: Status) -> button::Style {
    let palette = theme.extended_palette();
    let pair = if status == Status::Pressed {
        palette.danger.base
    } else {
        palette.danger.weak
    };
    button::Style {
        background: Some(Background::Color(pair.color)),
        text_color: contrast_safe_text(pair.color, pair.text).scale_alpha(
            if status == Status::Disabled {
                0.45
            } else {
                1.0
            },
        ),
        border: Border {
            radius: 4.0.into(),
            color: palette.danger.base.color,
            width: if status == Status::Hovered { 2.0 } else { 1.0 },
        },
        ..button::Style::default()
    }
}

fn flat_danger_style(theme: &Theme, status: Status) -> button::Style {
    let palette = theme.extended_palette();
    let pair = match status {
        Status::Hovered | Status::Pressed => palette.danger.weak,
        Status::Active | Status::Disabled => palette.background.base,
    };
    button::Style {
        background: matches!(status, Status::Hovered | Status::Pressed)
            .then_some(Background::Color(pair.color)),
        text_color: contrast_safe_text(pair.color, pair.text).scale_alpha(
            if status == Status::Disabled {
                0.45
            } else {
                1.0
            },
        ),
        border: Border {
            radius: 4.0.into(),
            ..Border::default()
        },
        ..button::Style::default()
    }
}

fn contrast_safe_text(background: Color, preferred: Color) -> Color {
    if background.relative_contrast(preferred) >= 4.5 {
        preferred
    } else if background.relative_contrast(Color::WHITE)
        >= background.relative_contrast(Color::BLACK)
    {
        Color::WHITE
    } else {
        Color::BLACK
    }
}

fn workspace_tab_style(theme: &Theme, status: Status, active: bool) -> button::Style {
    let palette = theme.extended_palette();
    let pair = if active {
        palette.primary.weak
    } else {
        match status {
            Status::Hovered => palette.background.weak,
            Status::Pressed => palette.background.neutral,
            Status::Active | Status::Disabled => palette.background.base,
        }
    };
    button::Style {
        background: (active || matches!(status, Status::Hovered | Status::Pressed))
            .then_some(Background::Color(pair.color)),
        text_color: contrast_safe_text(pair.color, pair.text).scale_alpha(
            if status == Status::Disabled {
                0.45
            } else {
                1.0
            },
        ),
        border: Border {
            radius: 0.0.into(),
            color: if active {
                palette.primary.base.color
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
    } else {
        match status {
            Status::Hovered => palette.background.weak,
            Status::Pressed => palette.background.neutral,
            Status::Active | Status::Disabled => palette.background.weakest,
        }
    };
    button::Style {
        background: (selected || matches!(status, Status::Hovered | Status::Pressed))
            .then_some(Background::Color(pair.color)),
        text_color: contrast_safe_text(pair.color, pair.text).scale_alpha(
            if status == Status::Disabled {
                0.45
            } else {
                1.0
            },
        ),
        border: Border {
            radius: 4.0.into(),
            color: if selected {
                palette.primary.base.color
            } else {
                pair.color
            },
            width: if selected { 1.0 } else { 0.0 },
        },
        ..button::Style::default()
    }
}

fn list_item_container_style(theme: &Theme, selected: bool, hovered: bool) -> container::Style {
    let status = if hovered {
        Status::Hovered
    } else {
        Status::Active
    };
    let style = list_item_style(theme, status, selected);
    container::Style {
        background: style.background,
        text_color: Some(style.text_color),
        border: style.border,
        ..container::Style::default()
    }
}

fn content_tab_style(theme: &Theme, status: Status, selected: bool) -> button::Style {
    let palette = theme.extended_palette();
    let pair = if selected {
        palette.primary.weak
    } else {
        match status {
            Status::Hovered => palette.background.weak,
            Status::Pressed => palette.background.neutral,
            Status::Active | Status::Disabled => palette.background.base,
        }
    };
    button::Style {
        background: (selected || matches!(status, Status::Hovered | Status::Pressed))
            .then_some(Background::Color(pair.color)),
        text_color: contrast_safe_text(pair.color, pair.text).scale_alpha(if selected {
            1.0
        } else {
            0.82
        }),
        border: Border {
            radius: 4.0.into(),
            color: if selected {
                palette.primary.base.color
            } else {
                pair.color
            },
            width: if selected { 1.0 } else { 0.0 },
        },
        ..button::Style::default()
    }
}

fn session_tab_group_style(theme: &Theme, selected: bool) -> container::Style {
    let palette = theme.extended_palette();
    let pair = if selected {
        palette.primary.weak
    } else {
        palette.background.weakest
    };
    container::Style {
        background: Some(Background::Color(pair.color)),
        text_color: Some(contrast_safe_text(pair.color, pair.text)),
        border: Border {
            radius: 4.0.into(),
            color: if selected {
                palette.primary.base.color
            } else {
                palette.background.weak.color
            },
            width: 1.0,
        },
        ..container::Style::default()
    }
}

fn tab_label_style(theme: &Theme, status: Status, selected: bool) -> button::Style {
    let palette = theme.extended_palette();
    let pair = if selected {
        palette.primary.weak
    } else {
        match status {
            Status::Hovered => palette.background.weak,
            Status::Pressed => palette.background.neutral,
            Status::Active | Status::Disabled => palette.background.base,
        }
    };
    button::Style {
        background: (!selected && !matches!(status, Status::Active | Status::Disabled))
            .then_some(Background::Color(pair.color)),
        text_color: contrast_safe_text(pair.color, pair.text),
        border: Border {
            radius: 4.0.into(),
            ..Border::default()
        },
        ..button::Style::default()
    }
}

fn tab_close_style(theme: &Theme, status: Status) -> button::Style {
    let palette = theme.extended_palette();
    let pair = match status {
        Status::Hovered => palette.background.weak,
        Status::Pressed => palette.background.neutral,
        Status::Active | Status::Disabled => palette.background.base,
    };
    button::Style {
        background: match status {
            Status::Hovered | Status::Pressed => Some(Background::Color(pair.color)),
            Status::Active | Status::Disabled => None,
        },
        text_color: if status == Status::Active {
            pair.text.scale_alpha(0.68)
        } else {
            pair.text
        },
        border: Border {
            radius: 4.0.into(),
            color: if matches!(status, Status::Hovered | Status::Pressed) {
                palette.danger.strong.color
            } else {
                pair.color
            },
            width: if matches!(status, Status::Hovered | Status::Pressed) {
                1.0
            } else {
                0.0
            },
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

fn session_navigation_label(name: &str, state: SessionState) -> String {
    format!("{name} · {}", session_state_label(state))
}

fn session_tab_label(name: &str) -> String {
    name.to_owned()
}

fn cancel_inline_session_rename(rename: &mut Option<InlineSessionRename>) {
    if rename.as_ref().is_some_and(|rename| !rename.pending) {
        *rename = None;
    }
}

fn retain_inline_session_rename(
    rename: &mut Option<InlineSessionRename>,
    session_ids: impl IntoIterator<Item = SessionId>,
) {
    let Some(session_id) = rename.as_ref().map(|rename| rename.session_id) else {
        return;
    };
    if !session_ids.into_iter().any(|id| id == session_id) {
        *rename = None;
    }
}

const fn session_state_label(state: SessionState) -> &'static str {
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

const fn session_can_stop(state: SessionState) -> bool {
    matches!(
        state,
        SessionState::Fresh
            | SessionState::Starting
            | SessionState::Running
            | SessionState::NeedsFeedback
    )
}

const fn session_can_replay(state: SessionState) -> bool {
    matches!(
        state,
        SessionState::FinishedUnseen | SessionState::FinishedSeen | SessionState::Failed
    )
}

const fn session_terminal_action_label(
    attached: bool,
    state: SessionState,
    compact: bool,
) -> Option<&'static str> {
    if attached {
        Some(if compact { "Leave" } else { "Leave terminal" })
    } else if session_can_stop(state) {
        Some(if compact { "Open" } else { "Open terminal" })
    } else if session_can_replay(state) {
        Some("View output")
    } else {
        None
    }
}

fn is_open_terminal_shortcut(value: &str) -> bool {
    value.eq_ignore_ascii_case("o") || value.eq_ignore_ascii_case("a")
}

#[cfg(test)]
mod tests {
    use super::*;
    use sylvops_core::provider::ProviderCapabilities;

    fn provider_health(kind: ProviderKind, available: bool, authenticated: bool) -> ProviderHealth {
        ProviderHealth {
            kind,
            available,
            authenticated,
            executable_path: None,
            version: None,
            diagnostic: (!available).then(|| "not found".into()),
            capabilities: ProviderCapabilities::default(),
            checked_at: 0,
        }
    }

    fn assert_button_contrast(
        theme_name: &str,
        style_name: &str,
        theme: &Theme,
        style: &button::Style,
    ) {
        let background = match style.background {
            Some(Background::Color(color)) => color,
            Some(Background::Gradient(_)) => panic!("button gradients are not expected"),
            None => theme.extended_palette().background.base.color,
        };
        let contrast = background.relative_contrast(style.text_color);
        assert!(
            contrast >= 4.5,
            "{theme_name} {style_name} contrast was {contrast:.2}"
        );
    }

    fn assert_session_tab_contrast(theme_name: &str, theme: &Theme) {
        let session_group = session_tab_group_style(theme, true);
        let Some(Background::Color(group_background)) = session_group.background else {
            panic!("selected session tabs need a solid background");
        };
        for status in [Status::Active, Status::Hovered, Status::Pressed] {
            let session_label = tab_label_style(theme, status, true);
            assert!(session_label.background.is_none());
            assert!(
                group_background.relative_contrast(session_label.text_color) >= 4.5,
                "{theme_name} selected session-tab text must remain readable"
            );
        }
    }

    #[test]
    fn terminal_view_has_no_nested_scroll_viewport() {
        let source = include_str!("lib.rs");
        let terminal_view = source
            .split_once("    fn terminal_view(&self)")
            .and_then(|(_, tail)| tail.split_once("    fn changes_view(&self)"))
            .map(|(body, _)| body)
            .expect("terminal view source");

        assert!(
            !terminal_view.contains("scrollable("),
            "the VT screen owns its viewport; a nested desktop scroll area breaks terminal input and resizing"
        );
        assert!(
            terminal_view.contains(".on_scroll("),
            "the fixed VT viewport must still route wheel input to terminal-owned scrollback"
        );
    }

    #[test]
    fn navigator_reselection_clears_terminal_focus_before_returning() {
        let source = include_str!("lib.rs");
        for function in ["select_project", "select_worktree", "select_session"] {
            let body = source
                .split_once(&format!("    fn {function}"))
                .and_then(|(_, tail)| tail.split_once("\n    fn "))
                .map(|(body, _)| body)
                .expect("navigator selection function");
            let focus = body
                .find("self.unfocus_terminal()")
                .expect("navigator selection clears terminal focus");
            let early_return = body.find("return;").unwrap_or(usize::MAX);
            assert!(
                focus < early_return,
                "{function} must clear focus even when the selection is unchanged"
            );
        }
    }

    #[test]
    fn session_rename_uses_double_click_without_a_pencil_control() {
        let source = include_str!("lib.rs");
        let sessions = source
            .split_once("    fn sessions_column(&self)")
            .and_then(|(_, tail)| tail.split_once("    fn session_rename_row(&self)"))
            .map(|(body, _)| body)
            .expect("sessions column source");

        assert!(sessions.contains("on_double_click"));
        assert!(!sessions.contains('✎'));
    }

    #[test]
    fn navigator_labels_are_single_line_clipped_and_have_no_tooltips() {
        let source = include_str!("lib.rs");
        let select_button = source
            .split_once("fn select_button(")
            .and_then(|(_, tail)| tail.split_once("\nfn tab_button"))
            .map(|(body, _)| body)
            .expect("select button source");

        assert!(select_button.contains("Wrapping::None"));
        assert!(select_button.contains(".clip(true)"));
        assert!(!select_button.contains("tooltip("));
    }

    #[test]
    fn inline_rename_uses_keyboard_and_click_away_without_buttons() {
        let source = include_str!("lib.rs");
        let rename = source
            .split_once("    fn session_rename_row(&self)")
            .and_then(|(_, tail)| tail.split_once("    fn compact_navigator(&self)"))
            .map(|(body, _)| body)
            .expect("session rename source");

        assert!(rename.contains("on_submit_maybe"));
        assert!(!rename.contains("Save"));
        assert!(!rename.contains("Cancel"));
        assert!(source.contains(".on_press(Message::CancelSessionRename)"));
    }

    #[test]
    fn lifecycle_actions_live_with_the_active_session_tabs() {
        let source = include_str!("lib.rs");
        let workspace = source
            .split_once("    fn workspace_view(&self)")
            .and_then(|(_, tail)| tail.split_once("    fn session_actions(&self)"))
            .map(|(body, _)| body)
            .expect("workspace header source");

        let session_tabs = workspace
            .split_once("    fn session_tabs(&self)")
            .map(|(_, body)| body)
            .expect("session tabs source");
        assert!(session_tabs.contains("self.session_actions()"));
    }

    #[test]
    fn measured_terminal_viewport_keeps_the_grid_inside_the_visible_screen() {
        let viewport = iced::Size::new(600.0, 400.0);
        let font_size = 14;
        let (columns, rows) = terminal_grid_dimensions(viewport, font_size);
        let screen_width = viewport.width - TERMINAL_HORIZONTAL_PADDING;
        let screen_height = viewport.height
            - TERMINAL_VERTICAL_PADDING
            - TERMINAL_HEADER_HEIGHT
            - TERMINAL_HEADER_SPACING;
        let cell_width = f32::from(font_size) * TERMINAL_CELL_WIDTH_RATIO;
        let line_height = f32::from(font_size) * TERMINAL_LINE_HEIGHT_RATIO;

        assert!(f32::from(columns) * cell_width <= screen_width);
        assert!(f32::from(columns + 1) * cell_width > screen_width);
        assert!(f32::from(rows) * line_height <= screen_height);
        assert!(f32::from(rows + 1) * line_height > screen_height);
    }

    #[test]
    fn active_navigation_tabs_have_a_clear_persistent_accent() {
        let theme = theme::resolve(DesktopTheme::Dark, iced::theme::Mode::Dark);
        let active = content_tab_style(&theme, Status::Active, true);
        let inactive = content_tab_style(&theme, Status::Active, false);

        assert!(active.border.width >= 1.0);
        assert!(active.border.width > inactive.border.width);
        assert_ne!(active.text_color, inactive.text_color);
        let Some(Background::Color(background)) = active.background else {
            panic!("active tabs need a solid background");
        };
        assert!(
            background.relative_contrast(active.text_color) >= 4.5,
            "selected tab text must remain readable against its fill"
        );
    }

    #[test]
    fn custom_button_styles_remain_readable_and_have_distinct_hover_states() {
        let choices = [
            ("light", DesktopTheme::Light, iced::theme::Mode::Light),
            ("dark", DesktopTheme::Dark, iced::theme::Mode::Dark),
            ("nord", DesktopTheme::Nord, iced::theme::Mode::Dark),
            (
                "tokyo night",
                DesktopTheme::TokyoNight,
                iced::theme::Mode::Dark,
            ),
            (
                "catppuccin",
                DesktopTheme::Catppuccin,
                iced::theme::Mode::Dark,
            ),
            ("dracula", DesktopTheme::Dracula, iced::theme::Mode::Dark),
            (
                "gruvbox dark",
                DesktopTheme::GruvboxDark,
                iced::theme::Mode::Dark,
            ),
            (
                "solarized light",
                DesktopTheme::SolarizedLight,
                iced::theme::Mode::Light,
            ),
            (
                "solarized dark",
                DesktopTheme::SolarizedDark,
                iced::theme::Mode::Dark,
            ),
        ];

        for (theme_name, choice, mode) in choices {
            let theme = theme::resolve(choice, mode);
            let styles = [
                ("chrome active", chrome_action_style(&theme, Status::Active)),
                ("chrome hover", chrome_action_style(&theme, Status::Hovered)),
                (
                    "chrome pressed",
                    chrome_action_style(&theme, Status::Pressed),
                ),
                (
                    "primary active",
                    primary_action_style(&theme, Status::Active),
                ),
                (
                    "primary hover",
                    primary_action_style(&theme, Status::Hovered),
                ),
                ("danger active", danger_action_style(&theme, Status::Active)),
                ("danger hover", danger_action_style(&theme, Status::Hovered)),
                ("flat danger", flat_danger_style(&theme, Status::Hovered)),
                (
                    "workspace selected",
                    workspace_tab_style(&theme, Status::Active, true),
                ),
                (
                    "workspace hover",
                    workspace_tab_style(&theme, Status::Hovered, false),
                ),
                (
                    "list selected",
                    list_item_style(&theme, Status::Active, true),
                ),
                (
                    "list hover",
                    list_item_style(&theme, Status::Hovered, false),
                ),
                (
                    "content selected",
                    content_tab_style(&theme, Status::Active, true),
                ),
                (
                    "content hover",
                    content_tab_style(&theme, Status::Hovered, false),
                ),
                ("tab active", tab_label_style(&theme, Status::Active, false)),
                ("tab hover", tab_label_style(&theme, Status::Hovered, false)),
                ("close active", tab_close_style(&theme, Status::Active)),
                ("close hover", tab_close_style(&theme, Status::Hovered)),
            ];
            for (name, style) in styles {
                assert_button_contrast(theme_name, name, &theme, &style);
            }
            assert_ne!(
                chrome_action_style(&theme, Status::Active).background,
                chrome_action_style(&theme, Status::Hovered).background,
                "hover must be visible without changing text contrast"
            );
            assert_session_tab_contrast(theme_name, &theme);
        }
    }

    #[test]
    fn session_labels_put_state_after_the_name_without_a_prefix() {
        assert_eq!(
            session_navigation_label("Build API", SessionState::Disconnected),
            "Build API · Disconnected"
        );
        assert_eq!(session_tab_label("Build API"), "Build API");
    }

    #[test]
    fn inline_session_rename_retains_input_and_errors_until_resolved() {
        let session_id = SessionId::new();
        let mut rename = InlineSessionRename::new(session_id, "Old name");
        rename.update("  New name  ".into());

        assert_eq!(rename.begin_submission().as_deref(), Some("New name"));
        assert!(rename.pending);

        rename.fail("name already exists".into());
        assert!(!rename.pending);
        assert_eq!(rename.value, "  New name  ");
        assert_eq!(rename.error.as_deref(), Some("name already exists"));
    }

    #[test]
    fn inline_session_rename_cancels_only_before_submission() {
        let session_id = SessionId::new();
        let mut rename = Some(InlineSessionRename::new(session_id, "Name"));
        cancel_inline_session_rename(&mut rename);
        assert!(rename.is_none());

        let mut pending = InlineSessionRename::new(session_id, "Name");
        assert_eq!(pending.begin_submission().as_deref(), Some("Name"));
        let mut rename = Some(pending);
        cancel_inline_session_rename(&mut rename);
        assert!(
            rename.is_some(),
            "an in-flight rename cannot be cancelled locally"
        );
    }

    #[test]
    fn stale_inline_session_rename_is_discarded_after_refresh() {
        let session_id = SessionId::new();
        let mut rename = Some(InlineSessionRename::new(session_id, "Name"));
        retain_inline_session_rename(&mut rename, [session_id]);
        assert!(rename.is_some());

        retain_inline_session_rename(&mut rename, [SessionId::new()]);
        assert!(rename.is_none());
    }

    #[test]
    fn footer_uses_a_readable_status_text_size() {
        const {
            assert!(FOOTER_TEXT_SIZE >= 12.0);
        }
    }

    #[test]
    fn inactive_session_actions_are_safe_and_explicit() {
        assert!(session_can_stop(SessionState::Running));
        assert!(!session_can_stop(SessionState::Terminated));
        assert!(session_can_replay(SessionState::FinishedSeen));
        assert!(session_can_replay(SessionState::Failed));
        assert!(!session_can_replay(SessionState::Disconnected));
        assert!(!session_can_replay(SessionState::Terminated));
    }

    #[test]
    fn terminal_actions_use_short_single_line_labels_when_space_is_tight() {
        assert_eq!(
            session_terminal_action_label(false, SessionState::Running, false),
            Some("Open terminal")
        );
        assert_eq!(
            session_terminal_action_label(false, SessionState::Running, true),
            Some("Open")
        );
        assert_eq!(
            session_terminal_action_label(true, SessionState::Running, false),
            Some("Leave terminal")
        );
        assert_eq!(
            session_terminal_action_label(true, SessionState::Running, true),
            Some("Leave")
        );
    }

    #[test]
    fn checkout_delete_is_only_available_for_active_managed_checkouts() {
        assert!(checkout_delete_available(false, WorktreeStatus::Active));
        assert!(!checkout_delete_available(true, WorktreeStatus::Active));
        for status in [
            WorktreeStatus::Creating,
            WorktreeStatus::Removing,
            WorktreeStatus::Removed,
            WorktreeStatus::Missing,
            WorktreeStatus::Invalid,
        ] {
            assert!(!checkout_delete_available(false, status));
        }
    }

    #[test]
    fn keyboard_selection_wraps_in_both_directions() {
        let items = [10, 20, 30];
        assert_eq!(next_selection(&items, Some(30), 1), Some(10));
        assert_eq!(next_selection(&items, Some(10), -1), Some(30));
        assert_eq!(next_selection::<u8>(&[], None, 1), None);
    }

    #[test]
    fn first_run_copy_explains_the_hierarchy_in_order() {
        assert!(
            first_run_guidance(FormKind::CreateWorkspace)
                .is_some_and(|message| message.contains("Workspace"))
        );
        assert!(
            first_run_guidance(FormKind::RegisterProject(WorkspaceId::new()))
                .is_some_and(|message| message.contains("Project") && message.contains("Worktree"))
        );
        assert!(
            first_run_guidance(FormKind::CreateSession(WorktreeId::new()))
                .is_some_and(|message| message.contains("Worktree") && message.contains("Session"))
        );
    }

    #[test]
    fn provider_recovery_copy_offers_retry_and_shell_fallback() {
        let unavailable =
            provider_recovery_message(&provider_health(ProviderKind::Codex, false, false));
        assert!(unavailable.contains("Install Codex"));
        assert!(unavailable.contains("Shell remains available"));

        let unauthenticated =
            provider_recovery_message(&provider_health(ProviderKind::Codex, true, false));
        assert!(unauthenticated.contains("Sign in to Codex"));
        assert!(unauthenticated.contains("retry discovery"));
    }

    #[test]
    fn forms_use_specific_primary_action_labels() {
        assert_eq!(
            form_submit_label(&Form::workspace(), false, false),
            "Create workspace"
        );
        assert_eq!(
            form_submit_label(&Form::repository(WorkspaceId::new()), false, false),
            "Register repository"
        );
        assert_eq!(
            form_submit_label(&Form::worktree(ProjectId::new()), false, false),
            "Create checkout"
        );
        assert_eq!(
            form_submit_label(
                &Form::session(
                    WorktreeId::new(),
                    vec![provider_health(ProviderKind::Shell, true, true)],
                ),
                false,
                false,
            ),
            "Open Shell"
        );
    }
}
