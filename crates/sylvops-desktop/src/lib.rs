//! Native, mouse-first `SylvOps` client backed exclusively by authenticated daemon IPC.

mod bridge;
mod forms;
mod icons;
mod presentation;
mod state;
mod terminal;
mod theme;

use std::{
    collections::{HashMap, HashSet},
    fmt,
    io::Cursor,
    time::{Duration, Instant},
};

use bridge::{Bridge, BridgeEvent, Operation};
use chrono::{DateTime, Local, Locale};
use forms::{
    Confirmation, DataRemovalConfirmation, FirstRunStepState, FormModal, Modal, first_run_steps,
};
use iced::{
    Background, Border, Center, Color, Element, Fill, Font, Length, Point, Subscription, Task,
    Theme,
    alignment::Vertical,
    clipboard, keyboard,
    keyboard::{Key, key::Named},
    mouse, system, time,
    widget::{
        self, button, column, container, mouse_area, opaque, pane_grid, pick_list, responsive,
        rich_text, row, rule, scrollable, sensor, space, span, stack, text, text_input,
        vertical_slider,
    },
    window,
};
use iced::{font, font::Weight, widget::button::Status};
use icons::{LineIcon, line_icon};
use presentation::{
    ActiveTerminalAction, ButtonIntent, ButtonTokens, ControlState, DensityMetrics,
    DesktopPresentation, InteractionState as PresentationInteraction, NavigatorAction,
    NavigatorAttention, NavigatorMode, NavigatorNodeId, NavigatorRow, PresentationInput,
    PresentationLayout, PresentationTheme, SystemAppearance, TerminalPalette, Viewport,
    WorkspaceNavigation, button_visual, human_readable_path,
    session_state_can_replay as session_can_replay, session_state_can_stop as session_can_stop,
    session_state_label,
};
use sylvops_core::{
    domain::{
        AttachmentRole, DaemonSnapshot, Project, ProviderKind, Session, SessionState, Workspace,
        Worktree, WorktreeStatus,
    },
    ids::{ProjectId, SessionId, WorkspaceId, WorktreeId},
    protocol::{ClientRequest, DaemonEvent, DaemonResponse},
    provider::{ProviderHealth, session_can_resume},
    ui::{
        DesktopDensity, DesktopPanel, DesktopState, DesktopTerminalCursor, DesktopTerminalFont,
        DesktopTheme, MAX_OPEN_DESKTOP_SESSIONS, MAX_TERMINAL_FONT_SIZE, MIN_DESKTOP_HEIGHT,
        MIN_DESKTOP_WIDTH, MIN_TERMINAL_FONT_SIZE, MainTab,
    },
    ui_forms::{Form, FormKind},
    upgrade::{InstallDisposition, UpgradeStatus},
};
use sylvops_daemon::runtime::RuntimePaths;
use terminal::{
    DisplayRun, TerminalState, WheelAction, display_runs as terminal_display_runs,
    encode_key as encode_terminal_key, encode_paste as encode_terminal_paste,
};

const TIMER_TICK: Duration = Duration::from_millis(16);
const SAVE_DEBOUNCE: Duration = Duration::from_millis(500);
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(75);
const SUCCESS_DURATION: Duration = Duration::from_secs(4);
const MAX_EVENTS_PER_WAKE: usize = 512;
const UI_TEXT_SIZE: f32 = 14.0;
const UI_META_SIZE: f32 = 12.0;
const FOOTER_TEXT_SIZE: f32 = 12.0;
const TERMINAL_HORIZONTAL_PADDING: f32 = 24.0;
const TERMINAL_VERTICAL_PADDING: f32 = 16.0;
const TERMINAL_HEADER_HEIGHT: f32 = 26.0;
const TERMINAL_HEADER_SPACING: f32 = 4.0;
const TERMINAL_SCROLLBAR_WIDTH: f32 = 14.0;
const TERMINAL_CELL_WIDTH_RATIO: f32 = 0.6;
const TERMINAL_LINE_HEIGHT_RATIO: f32 = 1.3;
const MAX_WINDOW_ICON_SIDE: u32 = 256;
const DENSITY_CHOICES: [DesktopDensity; 2] = [DesktopDensity::Comfortable, DesktopDensity::Compact];
const TERMINAL_FONT_CHOICES: [DesktopTerminalFont; 4] = [
    DesktopTerminalFont::System,
    DesktopTerminalFont::JetBrainsMono,
    DesktopTerminalFont::CascadiaCode,
    DesktopTerminalFont::FiraCode,
];
const TERMINAL_CURSOR_CHOICES: [DesktopTerminalCursor; 2] =
    [DesktopTerminalCursor::Block, DesktopTerminalCursor::Line];
const TERMINAL_FONT_SIZE_CHOICES: [u8; 13] = [10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22];
const FEATURED_THEME_COUNT: usize = 4;
const CHECK_AGAIN_LABEL: &str = "Check again";
const STOP_SESSION_LABEL: &str = "Stop session";
const DELETE_CHECKOUT_LABEL: &str = "Delete checkout";
const MAX_TECHNICAL_COPY_CHARS: usize = 4_096;
const MAX_USER_DIAGNOSTIC_CHARS: usize = 1_024;
const JETBRAINS_MONO_REGULAR: &[u8] = include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf");
const JETBRAINS_MONO_MEDIUM: &[u8] = include_bytes!("../assets/fonts/JetBrainsMono-Medium.ttf");
const JETBRAINS_MONO_SEMIBOLD: &[u8] = include_bytes!("../assets/fonts/JetBrainsMono-SemiBold.ttf");
const UI_FONT: Font = Font::with_name("JetBrains Mono");
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

#[cfg(test)]
fn load_bundled_ui_fonts_for_rendering() {
    let mut font_system = iced::advanced::graphics::text::font_system()
        .write()
        .expect("global font system lock");
    for bytes in [
        JETBRAINS_MONO_REGULAR,
        JETBRAINS_MONO_MEDIUM,
        JETBRAINS_MONO_SEMIBOLD,
    ] {
        font_system.load_font(std::borrow::Cow::Borrowed(bytes));
    }
}

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
    .font(JETBRAINS_MONO_REGULAR)
    .font(JETBRAINS_MONO_MEDIUM)
    .font(JETBRAINS_MONO_SEMIBOLD)
    .default_font(UI_FONT)
    .antialiasing(true)
    .window(desktop_window_settings())
    .run()
}

fn desktop_window_settings() -> window::Settings {
    let icon_directory = ico::IconDir::read(Cursor::new(include_bytes!(
        "../../../packaging/icons/sylvops.ico"
    )))
    .expect("the embedded SylvOps window icon must be a valid ICO file");
    let icon_entry = icon_directory
        .entries()
        .iter()
        .filter(|entry| {
            (1..=MAX_WINDOW_ICON_SIDE).contains(&entry.width())
                && (1..=MAX_WINDOW_ICON_SIDE).contains(&entry.height())
        })
        .max_by_key(|entry| u64::from(entry.width()) * u64::from(entry.height()))
        .expect("the embedded SylvOps window icon must contain a bounded image");
    let icon_image = icon_entry
        .decode()
        .expect("the embedded SylvOps window icon image must be valid");
    let icon = window::icon::from_rgba(
        icon_image.rgba_data().to_vec(),
        icon_image.width(),
        icon_image.height(),
    )
    .expect("the embedded SylvOps window icon must have valid RGBA dimensions");

    let settings = window::Settings {
        size: iced::Size::new(1_440.0, 900.0),
        min_size: Some(iced::Size::new(
            f32::from(MIN_DESKTOP_WIDTH),
            f32::from(MIN_DESKTOP_HEIGHT),
        )),
        exit_on_close_request: false,
        icon: Some(icon),
        ..window::Settings::default()
    };
    #[cfg(target_os = "linux")]
    let settings = {
        let mut settings = settings;
        sylvops_core::LINUX_DESKTOP_ID.clone_into(&mut settings.platform_specific.application_id);
        settings
    };
    settings
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
    navigator_visibility: NavigatorVisibility,
    keyboard_panel: DesktopPanel,
    hovered_navigator_node: Option<NavigatorNodeId>,
    pending_navigator_reveal: Option<DesktopPanel>,
    terminal_focus: TerminalFocus,
    terminal_viewport: Option<iced::Size>,
    terminal_pointer: Option<Point>,
    terminal_selection_state: TerminalSelectionState,
    inline_navigator_rename: Option<InlineNavigatorRename>,
    resume_pending: HashSet<SessionId>,
    window_mode: window::Mode,
    update_status: UpgradeStatus,
    install_request: InstallRequestState,
    modal_focus: ModalFocus,
    disclosures: DisclosureState,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct DisclosureState {
    classic_themes: bool,
    technical_details: bool,
    settings_theme_focus: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum InstallRequestState {
    #[default]
    Idle,
    Pending,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum ModalFocus {
    #[default]
    Idle,
    Pending,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DesktopPane {
    Projects,
    Worktrees,
    Sessions,
    Main,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum NavigatorVisibility {
    #[default]
    Shown,
    Collapsed,
}

#[derive(Clone, Debug)]
struct InlineNavigatorRename {
    target: NavigatorNodeId,
    value: String,
    pending: bool,
    error: Option<String>,
    input_id: widget::Id,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct WorkspaceChoice {
    id: WorkspaceId,
    name: String,
}

impl fmt::Display for WorkspaceChoice {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.name)
    }
}

impl InlineNavigatorRename {
    fn new(target: NavigatorNodeId, value: &str) -> Self {
        Self {
            target,
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
            self.error = Some("Name must contain between 1 and 200 characters.".into());
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
    BridgeReady,
    Keyboard(keyboard::Event),
    SelectWorkspace(WorkspaceId),
    SelectProject(ProjectId),
    SelectWorktree(WorktreeId),
    SelectSession(SessionId),
    RunNavigatorAction(NavigatorAction),
    HoverNavigatorNode(Option<NavigatorNodeId>),
    SelectOpenSession(SessionId),
    CloseSessionTab(SessionId),
    SelectMainTab(MainTab),
    NewWorkspace,
    NewProject,
    NewWorktree,
    NewSession,
    BeginNavigatorRename(NavigatorNodeId),
    NavigatorRenameInput(String),
    SubmitNavigatorRename,
    CancelNavigatorRename,
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
    Resume(SessionId),
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
    ToggleClassicThemes,
    ToggleTechnicalDetails,
    CopyTechnicalValue(String),
    ToggleFullscreen,
    SelectTheme(DesktopTheme),
    SelectDensity(DesktopDensity),
    SelectTerminalFont(DesktopTerminalFont),
    SelectTerminalCursor(DesktopTerminalCursor),
    SetTerminalFontSize(u8),
    ResetLayout,
    SelectCompactPanel(DesktopPanel),
    ToggleNavigator,
    WindowResized(window::Id, iced::Size),
    WindowReady(Option<window::Id>),
    SystemThemeChanged(iced::theme::Mode),
    CloseRequested(window::Id),
    PaneResized(pane_grid::ResizeEvent),
    ShowNarrowNavigator,
    ShowNarrowMain,
    ClearError,
    CheckForUpdate,
    DownloadUpdate,
    InstallUpdate,
    TogglePeriodicUpdateChecks,
    OpenDataRemoval,
    DataRemovalConfirmationInput(String),
    SubmitDataRemoval,
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
            navigator_visibility: NavigatorVisibility::Shown,
            keyboard_panel: DesktopPanel::Projects,
            hovered_navigator_node: None,
            pending_navigator_reveal: None,
            terminal_focus: TerminalFocus::Unfocused,
            terminal_viewport: None,
            terminal_pointer: None,
            terminal_selection_state: TerminalSelectionState::Idle,
            inline_navigator_rename: None,
            resume_pending: HashSet::new(),
            window_mode: window::Mode::Windowed,
            update_status: UpgradeStatus::Idle,
            install_request: InstallRequestState::Idle,
            modal_focus: ModalFocus::Idle,
            disclosures: DisclosureState::default(),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Tick => {
                self.flush_timers();
                if self.modal_focus == ModalFocus::Pending {
                    self.modal_focus = ModalFocus::Idle;
                    return widget::operation::focus_next();
                }
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
            Message::BridgeReady => self.process_bridge_events(),
            Message::Keyboard(event) => {
                if let Some(task) = self.handle_terminal_clipboard(&event) {
                    return task;
                }
                if self.modal.is_none()
                    && self.inline_navigator_rename.is_some()
                    && let keyboard::Event::KeyPressed { key, .. } = &event
                {
                    match key {
                        Key::Named(Named::Escape)
                            if self
                                .inline_navigator_rename
                                .as_ref()
                                .is_some_and(|rename| !rename.pending) =>
                        {
                            cancel_inline_navigator_rename(&mut self.inline_navigator_rename);
                        }
                        Key::Named(Named::Enter) => self.submit_navigator_rename(),
                        _ => {}
                    }
                    return Task::none();
                }
                if self.modal.is_none()
                    && !self.terminal_focus.is_focused()
                    && let keyboard::Event::KeyPressed { key, modifiers, .. } = &event
                    && !modifiers.control()
                    && !modifiers.alt()
                    && matches!(key.as_ref(), Key::Character(value) if value.eq_ignore_ascii_case("r"))
                {
                    return self.begin_selected_navigator_rename();
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
                self.inline_navigator_rename = None;
                self.send_request(
                    Operation::OpenWorkspace(workspace_id),
                    ClientRequest::OpenWorkspace { workspace_id },
                );
            }
            Message::SelectProject(project_id) => self.select_project(project_id),
            Message::SelectWorktree(worktree_id) => self.select_worktree(worktree_id),
            Message::SelectSession(session_id) => self.select_session(session_id),
            Message::RunNavigatorAction(action) => {
                return self.run_navigator_action(action);
            }
            Message::HoverNavigatorNode(node) => self.hovered_navigator_node = node,
            Message::SelectOpenSession(session_id) => {
                self.unfocus_terminal();
                if self.active_session_id == Some(session_id) {
                    return Task::none();
                }
                if self
                    .inline_navigator_rename
                    .as_ref()
                    .is_some_and(|rename| rename.target != NavigatorNodeId::Session(session_id))
                {
                    self.inline_navigator_rename = None;
                }
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
                self.inline_navigator_rename = None;
                self.terminal_focus = TerminalFocus::Unfocused;
                self.modal = Some(Modal::Form(if self.snapshot.workspaces.is_empty() {
                    FormModal::first_run(Form::workspace())
                } else {
                    FormModal::new(Form::workspace())
                }));
                self.modal_focus = ModalFocus::Pending;
            }
            Message::NewProject => self.open_project_form(),
            Message::NewWorktree => self.open_worktree_form(),
            Message::NewSession => self.open_session_form(),
            Message::BeginNavigatorRename(target) => {
                return self.begin_navigator_rename(target);
            }
            Message::NavigatorRenameInput(value) => {
                if let Some(rename) = &mut self.inline_navigator_rename {
                    rename.update(value);
                }
            }
            Message::SubmitNavigatorRename => self.submit_navigator_rename(),
            Message::CancelNavigatorRename => {
                cancel_inline_navigator_rename(&mut self.inline_navigator_rename);
            }
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
            Message::Resume(session_id) => self.resume_session(session_id),
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
                if matches!(self.modal, Some(Modal::Settings)) {
                    self.modal = None;
                } else {
                    self.open_settings();
                }
            }
            Message::ToggleClassicThemes => {
                self.disclosures.classic_themes = !self.disclosures.classic_themes;
                self.disclosures.settings_theme_focus = self
                    .disclosures
                    .settings_theme_focus
                    .min(theme_gallery_len(self.disclosures.classic_themes).saturating_sub(1));
            }
            Message::ToggleTechnicalDetails => {
                self.disclosures.technical_details = !self.disclosures.technical_details;
            }
            Message::CopyTechnicalValue(value) => {
                self.success = Some(("Copied technical detail.".into(), Instant::now()));
                return clipboard::write(bounded_technical_copy(&value));
            }
            Message::CheckForUpdate => {
                self.send_request(Operation::CheckForUpdate, ClientRequest::CheckForUpdate);
            }
            Message::DownloadUpdate => {
                self.send_request(Operation::DownloadUpdate, ClientRequest::DownloadUpdate);
            }
            Message::InstallUpdate => self.send_install_request(Vec::new()),
            Message::TogglePeriodicUpdateChecks => {
                self.desktop_state.periodic_update_checks =
                    !self.desktop_state.periodic_update_checks;
                self.mark_state_dirty();
            }
            Message::OpenDataRemoval => self.open_data_removal_confirmation(),
            Message::DataRemovalConfirmationInput(value) => {
                if let Some(Modal::DataRemoval(confirmation)) = &mut self.modal {
                    confirmation.update(value);
                }
            }
            Message::SubmitDataRemoval => self.submit_data_removal(),
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
                self.disclosures.settings_theme_focus = theme_gallery_index(theme);
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
            Message::SelectTerminalCursor(cursor) => {
                self.desktop_state.terminal_cursor = cursor;
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
                self.inline_navigator_rename = None;
                self.terminal_focus = TerminalFocus::Unfocused;
                self.keyboard_panel = panel;
                self.desktop_state.compact_panel = panel;
                self.pending_navigator_reveal = Some(panel);
                self.mark_state_dirty();
            }
            Message::ToggleNavigator => {
                self.inline_navigator_rename = None;
                self.terminal_focus = TerminalFocus::Unfocused;
                self.navigator_visibility = match self.navigator_visibility {
                    NavigatorVisibility::Shown => NavigatorVisibility::Collapsed,
                    NavigatorVisibility::Collapsed => NavigatorVisibility::Shown,
                };
                self.queue_terminal_resize();
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
                self.pending_navigator_reveal = Some(self.keyboard_panel);
            }
            Message::ShowNarrowMain => {
                self.inline_navigator_rename = None;
                self.terminal_focus = TerminalFocus::Unfocused;
                self.narrow_main = true;
            }
            Message::ClearError => self.error = None,
        }
        self.take_navigator_reveal_task()
    }

    fn theme(&self) -> Theme {
        theme::resolve(self.presentation().theme)
    }

    fn presentation(&self) -> DesktopPresentation {
        DesktopPresentation::build(&PresentationInput {
            daemon: &self.snapshot,
            preferences: &self.desktop_state,
            viewport: Viewport::new(
                self.desktop_state.window_width,
                self.desktop_state.window_height,
            ),
            system_appearance: match self.system_theme {
                iced::theme::Mode::Light => SystemAppearance::Light,
                iced::theme::Mode::None | iced::theme::Mode::Dark => SystemAppearance::Dark,
            },
            interaction: PresentationInteraction {
                selected_project_id: self.selected_project_id,
                selected_worktree_id: self.selected_worktree_id,
                selected_session_id: self.selected_session_id,
                active_session_id: self.active_session_id,
                open_session_ids: self.open_sessions.clone(),
                active_session_attached: self.active_session_id.is_some_and(|session_id| {
                    self.terminals
                        .get(&session_id)
                        .is_some_and(|terminal| terminal.attached)
                }),
                resumable_session_ids: self
                    .snapshot
                    .sessions
                    .iter()
                    .filter(|session| {
                        session_can_resume(session, &self.snapshot.sessions, &self.providers)
                    })
                    .map(|session| session.id)
                    .collect(),
                main_tab: self.main_tab,
            },
        })
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
                .padding(16)
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
        let mut content = column![top, rule::horizontal(1)].width(Fill).height(Fill);
        if let Some(error) = &self.error {
            let presentation_theme = self.presentation().theme;
            content = content.push(
                container(
                    row![
                        text("Action failed").font(UI_SEMIBOLD).style(text::danger),
                        text(error).style(text::danger),
                        space::horizontal(),
                        button("Dismiss").on_press(Message::ClearError).style(
                            move |_theme, status| {
                                button_intent_style(
                                    presentation_theme,
                                    ButtonIntent::Secondary,
                                    status,
                                )
                            }
                        )
                    ]
                    .spacing(10)
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
        content = content
            .push(body)
            .push(rule::horizontal(1))
            .push(self.footer());
        let content: Element<'_, Message> = container(content).width(Fill).height(Fill).into();
        if self.inline_navigator_rename.is_some() {
            mouse_area(content)
                .on_press(Message::CancelNavigatorRename)
                .into()
        } else {
            content
        }
    }

    fn top_bar(&self) -> Element<'_, Message> {
        let presentation = self.presentation();
        let density = presentation.density;
        let compact = self.desktop_state.window_width < 1_000;
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
                line_icon(LineIcon::Brand, 16),
                text("SylvOps").font(UI_SEMIBOLD).size(15)
            ]
            .spacing(7)
            .align_y(Center),
        )
        .width(Length::Fixed(if compact { 106.0 } else { 132.0 }))
        .padding([0, 8]);
        let workspace_navigation = self.workspace_navigation(&presentation, compact);
        let mut actions = row![].spacing(density.region_spacing).align_y(Center);
        if !compact {
            actions = actions.push(
                container(text(worktree_context).font(UI_MEDIUM).size(UI_META_SIZE))
                    .height(density.control_height)
                    .padding([7, 11])
                    .align_y(Vertical::Center)
                    .style(selected_context_style),
            );
        }
        let actions = actions
            .push(self.refresh_button(compact))
            .push(
                button(line_icon(LineIcon::Fullscreen, 16))
                    .on_press(Message::ToggleFullscreen)
                    .width(density.control_height)
                    .height(density.control_height)
                    .padding(7)
                    .style(chrome_action_style),
            )
            .push(
                button(icon_text_label(
                    LineIcon::Settings,
                    "Settings",
                    UI_META_SIZE,
                ))
                .on_press(Message::ToggleSettings)
                .height(density.control_height)
                .padding([6, 11])
                .style(chrome_action_style),
            );
        let navigation = row![brand, workspace_navigation, actions]
            .spacing(density.region_spacing)
            .align_y(Center);
        container(navigation)
            .height(density.navigation_height)
            .width(Fill)
            .padding([5, 8])
            .align_y(Vertical::Center)
            .style(chrome_surface)
            .into()
    }

    fn workspace_navigation(
        &self,
        presentation: &DesktopPresentation,
        _compact: bool,
    ) -> Element<'static, Message> {
        let density = presentation.density;
        if presentation.workspace_navigation == WorkspaceNavigation::LabeledSwitcher {
            let choices: Vec<_> = self
                .snapshot
                .workspaces
                .iter()
                .map(|workspace| WorkspaceChoice {
                    id: workspace.id,
                    name: workspace.name.clone(),
                })
                .collect();
            let selected = self.active_workspace().map(|workspace| WorkspaceChoice {
                id: workspace.id,
                name: workspace.name.clone(),
            });
            return row![
                text("Workspace").font(UI_MEDIUM).size(UI_META_SIZE),
                pick_list(choices, selected, |choice| Message::SelectWorkspace(
                    choice.id
                ))
                .placeholder("Choose workspace")
                .width(Fill),
                container(row![
                    space::horizontal().width(10),
                    button(line_icon(LineIcon::Add, 16))
                        .on_press(Message::NewWorkspace)
                        .width(density.control_height)
                        .height(density.control_height)
                        .padding(7)
                        .style(borderless_icon_style),
                ]),
            ]
            .spacing(5)
            .align_y(Center)
            .width(Fill)
            .into();
        }

        let mut workspace_tabs = row![].spacing(0).align_y(Center);
        for (index, workspace) in self.snapshot.workspaces.iter().enumerate() {
            if index > 0 {
                workspace_tabs = workspace_tabs.push(rule::vertical(1));
            }
            let active = workspace.is_open;
            let action = button(
                centered_button_label(
                    workspace.name.clone(),
                    UI_TEXT_SIZE,
                    if active { UI_SEMIBOLD } else { UI_FONT },
                )
                .wrapping(text::Wrapping::None),
            )
            .on_press(Message::SelectWorkspace(workspace.id))
            .height(density.control_height)
            .padding([6, 14])
            .style(move |theme, status| workspace_tab_style(theme, status, active));
            workspace_tabs = workspace_tabs.push(action);
        }
        if !self.snapshot.workspaces.is_empty() {
            workspace_tabs = workspace_tabs.push(rule::vertical(1));
        }
        workspace_tabs = workspace_tabs.push(space::horizontal().width(10));
        workspace_tabs = workspace_tabs.push(
            button(line_icon(LineIcon::Add, 16))
                .on_press(Message::NewWorkspace)
                .width(density.control_height)
                .height(density.control_height)
                .padding(7)
                .style(borderless_icon_style),
        );
        scrollable(workspace_tabs)
            .direction(scrollable::Direction::Horizontal(
                scrollable::Scrollbar::hidden(),
            ))
            .width(Fill)
            .height(density.control_height)
            .into()
    }

    fn refresh_button(&self, _compact: bool) -> Element<'static, Message> {
        let control_height = self.presentation().density.control_height;
        let can_refresh =
            matches!(self.connection, ConnectionState::Connected) && !self.snapshot_pending;
        button(line_icon(LineIcon::Refresh, 15))
            .on_press_maybe(can_refresh.then_some(Message::Refresh))
            .width(control_height)
            .height(control_height)
            .padding(7)
            .style(chrome_action_style)
            .into()
    }

    fn mission_control(&self) -> Element<'_, Message> {
        if matches!(self.connection, ConnectionState::Connecting) {
            return centered_message("Connecting to the SylvOps daemon…");
        }
        match self.presentation().navigator.mode {
            NavigatorMode::Columns => pane_grid(&self.panes, |_pane, kind, _maximized| {
                let content = match kind {
                    DesktopPane::Projects => self.repositories_column(),
                    DesktopPane::Worktrees => self.checkouts_column(),
                    DesktopPane::Sessions => self.sessions_column(),
                    DesktopPane::Main => self.workspace_view(),
                };
                pane_grid::Content::new(content)
            })
            .spacing(1)
            .min_size(150)
            .on_resize(8, Message::PaneResized)
            .into(),
            NavigatorMode::Tabs if self.navigator_visibility == NavigatorVisibility::Collapsed => {
                row![
                    container(
                        button(line_icon(LineIcon::Collapse, 16))
                            .on_press(Message::ToggleNavigator)
                            .width(32)
                            .height(32)
                            .padding(7)
                            .style(chrome_action_style),
                    )
                    .padding([5, 4]),
                    rule::vertical(1),
                    self.workspace_view(),
                ]
                .width(Fill)
                .height(Fill)
                .into()
            }
            NavigatorMode::Tabs => row![
                self.compact_navigator(),
                rule::vertical(1),
                self.workspace_view(),
            ]
            .width(Fill)
            .height(Fill)
            .into(),
            NavigatorMode::Drawer => {
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

    fn repositories_column(&self) -> Element<'_, Message> {
        let presentation = self.presentation();
        let density = presentation.density;
        let mut items = column![].spacing(2);
        for row in &presentation.navigator.repositories {
            if self
                .inline_navigator_rename
                .as_ref()
                .is_some_and(|rename| rename.target == row.id)
            {
                items = items.push(self.navigator_rename_row());
            } else {
                items = items.push(self.navigator_row(row));
            }
        }
        if presentation.navigator.repositories.is_empty() {
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
        navigator_panel(
            "Repositories",
            Some((LineIcon::Add, Message::NewProject)),
            selected_navigator_actions(&presentation.navigator.repositories),
            density,
            items,
            DesktopPanel::Projects,
        )
    }

    fn checkouts_column(&self) -> Element<'_, Message> {
        let presentation = self.presentation();
        let density = presentation.density;
        let create = presentation
            .selection
            .project_id
            .map(|_| (LineIcon::Add, Message::NewWorktree));
        let mut items = column![].spacing(2);
        for row in &presentation.navigator.checkouts {
            if self
                .inline_navigator_rename
                .as_ref()
                .is_some_and(|rename| rename.target == row.id)
            {
                items = items.push(self.navigator_rename_row());
            } else {
                items = items.push(self.navigator_row(row));
            }
        }
        if presentation.selection.project_id.is_none() {
            items = items.push(empty_hint("Select a repository first."));
        } else if presentation.navigator.checkouts.is_empty() {
            items = items.push(empty_action(
                "No checkouts for this repository.",
                "Create checkout",
                Message::NewWorktree,
            ));
        }
        navigator_panel(
            "Checkouts",
            create,
            selected_navigator_actions(&presentation.navigator.checkouts),
            density,
            items,
            DesktopPanel::Worktrees,
        )
    }

    fn sessions_column(&self) -> Element<'_, Message> {
        let presentation = self.presentation();
        let density = presentation.density;
        let create = presentation
            .selection
            .worktree_id
            .map(|_| (LineIcon::Add, Message::NewSession));
        let mut items = column![].spacing(2);
        for row in &presentation.navigator.sessions {
            if self
                .inline_navigator_rename
                .as_ref()
                .is_some_and(|rename| rename.target == row.id)
            {
                items = items.push(self.navigator_rename_row());
            } else {
                items = items.push(self.navigator_row(row));
            }
        }
        if presentation.selection.worktree_id.is_none() {
            items = items.push(empty_hint("Select a checkout first."));
        } else if presentation.navigator.sessions.is_empty() {
            items = items.push(empty_action(
                "No sessions in this checkout.",
                "Start session",
                Message::NewSession,
            ));
        }
        navigator_panel(
            "Sessions",
            create,
            selected_navigator_actions(&presentation.navigator.sessions),
            density,
            items,
            DesktopPanel::Sessions,
        )
    }

    fn compact_navigator(&self) -> Element<'_, Message> {
        let presentation = self.presentation();
        let density = presentation.density;
        let tabs = row![
            compact_panel_button(
                "Repositories",
                DesktopPanel::Projects,
                self.desktop_state.compact_panel,
                density
            ),
            compact_panel_button(
                "Checkouts",
                DesktopPanel::Worktrees,
                self.desktop_state.compact_panel,
                density
            ),
            compact_panel_button(
                "Sessions",
                DesktopPanel::Sessions,
                self.desktop_state.compact_panel,
                density
            ),
            button(line_icon(LineIcon::Collapse, 15))
                .on_press_maybe(
                    (presentation.layout == PresentationLayout::Compact)
                        .then_some(Message::ToggleNavigator),
                )
                .width(density.control_height)
                .height(density.control_height)
                .padding(7)
                .style(chrome_action_style),
        ]
        .spacing(3);
        let panel = match self.desktop_state.compact_panel {
            DesktopPanel::Projects => self.repositories_column(),
            DesktopPanel::Worktrees => self.checkouts_column(),
            DesktopPanel::Sessions => self.sessions_column(),
        };
        container(column![container(tabs).padding([4, 5]), panel])
            .width(if presentation.layout == PresentationLayout::Compact {
                Length::Fixed(300.0)
            } else {
                Fill
            })
            .height(Fill)
            .into()
    }

    fn navigator_row(&self, row: &NavigatorRow) -> Element<'static, Message> {
        let density = self.presentation().density;
        let is_selected = row.selected;
        let is_hovered = self.hovered_navigator_node == Some(row.id);
        let name = text(row.label.clone())
            .font(if is_selected { UI_SEMIBOLD } else { UI_MEDIUM })
            .size(UI_TEXT_SIZE)
            .wrapping(text::Wrapping::None);
        let labels: Element<'static, Message> = {
            let mut title = row![name].spacing(5).align_y(Center);
            if !matches!(row.id, NavigatorNodeId::Session(_)) {
                if let Some(badge) = &row.badge {
                    title = title.push(
                        container(text(badge.clone()).font(UI_MEDIUM).size(10))
                            .padding([1, 4])
                            .style(navigator_badge_style),
                    );
                }
                if let Some(attention) = row.attention {
                    title = title.push(
                        row![
                            line_icon(navigator_attention_icon(attention), 13),
                            text(navigator_attention_label(attention))
                                .size(10)
                                .style(text::secondary),
                        ]
                        .spacing(3)
                        .align_y(Center),
                    );
                }
            }
            title = title.push(space::horizontal());
            column![
                title,
                text(row.detail.clone())
                    .size(UI_META_SIZE)
                    .style(text::secondary)
                    .wrapping(text::Wrapping::None),
            ]
            .spacing(1)
            .width(Fill)
            .into()
        };
        let content = container(row![labels].spacing(3).align_y(Center))
            .width(Fill)
            .height(density.control_height + 14)
            .padding([4, 6])
            .clip(true)
            .style(move |theme| navigator_row_container_style(theme, is_selected, is_hovered));
        let select = match row.id {
            NavigatorNodeId::Repository(id) => Message::SelectProject(id),
            NavigatorNodeId::Checkout(id) => Message::SelectWorktree(id),
            NavigatorNodeId::Session(id) => Message::SelectSession(id),
        };
        let double_click = Message::BeginNavigatorRename(row.id);
        mouse_area(content)
            .on_enter(Message::HoverNavigatorNode(Some(row.id)))
            .on_exit(Message::HoverNavigatorNode(None))
            .on_press(select)
            .on_double_click(double_click)
            .interaction(mouse::Interaction::Pointer)
            .into()
    }

    fn navigator_rename_row(&self) -> Element<'_, Message> {
        let Some(rename) = &self.inline_navigator_rename else {
            return space::vertical().height(0).into();
        };
        let controls = text_input("Name", &rename.value)
            .id(rename.input_id.clone())
            .on_input_maybe((!rename.pending).then_some(Message::NavigatorRenameInput))
            .on_submit_maybe((!rename.pending).then_some(Message::SubmitNavigatorRename))
            .padding([6, 8])
            .size(UI_TEXT_SIZE);
        let mut content = column![controls].spacing(4);
        if let Some(error) = &rename.error {
            content = content.push(text(error).size(UI_META_SIZE).style(text::danger));
        }
        container(content).padding([3, 0]).width(Fill).into()
    }

    fn workspace_view(&self) -> Element<'_, Message> {
        let presentation = self.presentation();
        let density = presentation.density;
        let main_tab = presentation.selection.main_tab;
        let tabs = self.session_tabs();
        let views = container(
            row![
                tab_button("Terminal", MainTab::Terminal, main_tab, density),
                tab_button("Changes", MainTab::Changes, main_tab, density),
                tab_button("Details", MainTab::Details, main_tab, density),
            ]
            .spacing(density.region_spacing)
            .align_y(Center),
        )
        .height(density.tab_height)
        .padding([4, 8])
        .width(Fill)
        .style(tab_strip_surface);
        let content = match main_tab {
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
        let presentation = self.presentation();
        let density = presentation.density;
        let mut tabs = row![].spacing(density.region_spacing).align_y(Center);
        for session in &presentation.active_session.tabs {
            let label = session_tab_label(&session.label);
            let active = session.active;
            let session_id = session.id;
            tabs = tabs.push(
                container(
                    row![
                        button(
                            centered_button_label(
                                label,
                                UI_TEXT_SIZE,
                                if active { UI_SEMIBOLD } else { UI_FONT },
                            )
                            .wrapping(text::Wrapping::None)
                        )
                        .on_press(Message::SelectOpenSession(session_id))
                        .height(density.row_height)
                        .padding([5, 9])
                        .style(move |theme, status| { tab_label_style(theme, status, active) }),
                        button(line_icon(LineIcon::Close, 15))
                            .on_press(Message::CloseSessionTab(session_id))
                            .height(density.row_height)
                            .padding([4, 8])
                            .style(tab_close_style),
                    ]
                    .spacing(0)
                    .align_y(Center),
                )
                .style(move |theme| session_tab_group_style(theme, active)),
            );
        }
        if presentation.active_session.tabs.is_empty() {
            tabs = tabs.push(
                text("Select a session to open it")
                    .size(UI_META_SIZE)
                    .style(text::secondary),
            );
        }
        let context = presentation.active_session.context.as_ref().map_or_else(
            || "No active session".to_owned(),
            |context| {
                format!(
                    "Repository: {}  /  Checkout: {}  ·  {}  ·  {}",
                    context.repository, context.checkout, context.branch, context.state
                )
            },
        );
        container(column![
            scrollable(tabs)
                .direction(scrollable::Direction::Horizontal(
                    scrollable::Scrollbar::hidden(),
                ))
                .height(density.control_height)
                .width(Fill),
            row![
                container(text(context).font(UI_MEDIUM).size(UI_META_SIZE))
                    .height(density.control_height)
                    .align_y(Vertical::Center)
                    .clip(true)
                    .width(Fill),
                self.session_actions(),
            ]
            .spacing(density.region_spacing)
            .align_y(Center),
        ])
        .padding([4, 8])
        .width(Fill)
        .style(tab_strip_surface)
        .into()
    }

    fn session_actions(&self) -> Element<'_, Message> {
        let presentation = self.presentation();
        let density = presentation.density;
        let Some(context) = presentation.active_session.context.as_ref() else {
            return text("No session selected").style(text::secondary).into();
        };
        let session_id = context.session_id;
        let terminal_action = context.terminal_action;
        let can_stop = context.can_stop;
        let can_resume = context.can_resume;
        let resume_pending = self.resume_pending.contains(&session_id);
        let state_label = context.state.clone();
        let presentation_theme = presentation.theme;
        responsive(move |size| {
            let compact = size.width < 260.0;
            let mut actions = row![].spacing(4);
            if let Some(action) = terminal_action {
                let message = match action {
                    ActiveTerminalAction::Leave => Message::Detach,
                    ActiveTerminalAction::Open | ActiveTerminalAction::ViewOutput => {
                        Message::Attach
                    }
                };
                let intent = match action {
                    ActiveTerminalAction::Leave => ButtonIntent::Quiet,
                    ActiveTerminalAction::Open | ActiveTerminalAction::ViewOutput => {
                        ButtonIntent::Primary
                    }
                };
                actions = actions.push(
                    button(
                        centered_button_label(
                            active_terminal_action_label(action, compact),
                            UI_META_SIZE,
                            UI_MEDIUM,
                        )
                        .wrapping(text::Wrapping::None),
                    )
                    .on_press(message)
                    .height(density.control_height)
                    .padding([6, 9])
                    .style(move |_theme, status| {
                        button_intent_style(presentation_theme, intent, status)
                    }),
                );
            }
            if can_stop {
                actions = actions.push(
                    button(centered_button_label(
                        STOP_SESSION_LABEL,
                        UI_META_SIZE,
                        UI_MEDIUM,
                    ))
                    .on_press(Message::Stop)
                    .height(density.control_height)
                    .padding([6, 9])
                    .style(move |_theme, status| {
                        button_intent_style(presentation_theme, ButtonIntent::Danger, status)
                    }),
                );
            } else if can_resume {
                actions = actions.push(
                    button(centered_button_label(
                        if resume_pending {
                            "Resuming…"
                        } else {
                            "Resume"
                        },
                        UI_META_SIZE,
                        UI_MEDIUM,
                    ))
                    .on_press_maybe((!resume_pending).then_some(Message::Resume(session_id)))
                    .height(density.control_height)
                    .padding([6, 9])
                    .style(move |_theme, status| {
                        button_intent_style(presentation_theme, ButtonIntent::Primary, status)
                    }),
                );
            } else if !compact {
                actions = actions.push(
                    container(
                        text(format!("Session record retained · {state_label}"))
                            .size(UI_META_SIZE)
                            .wrapping(text::Wrapping::None)
                            .style(text::secondary),
                    )
                    .height(density.control_height)
                    .align_y(Vertical::Center)
                    .clip(true),
                );
            }
            actions.into()
        })
        .width(Length::Shrink)
        .height(density.control_height)
        .into()
    }

    fn terminal_view(&self) -> Element<'_, Message> {
        let presentation = self.presentation();
        let Some(session_id) = presentation.selection.active_session_id else {
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
            return observed_terminal_viewport(open_terminal_action(presentation.density));
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
        let terminal_palette = presentation.theme.terminal;
        let spans = terminal_spans(
            terminal_display_runs(
                terminal,
                self.terminal_focus.is_focused(),
                self.desktop_state.terminal_cursor,
            ),
            terminal_palette,
            terminal_font,
            self.desktop_state.terminal_cursor,
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
        .style(move |theme| {
            terminal_surface(theme, self.terminal_focus.is_focused(), terminal_palette)
        });
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

    #[allow(clippy::too_many_lines)]
    fn details_view(&self) -> Element<'_, Message> {
        let presentation = self.presentation();
        let selection = presentation.details_selection;
        let mut content = column![text("Selection details").font(UI_SEMIBOLD).size(24)].spacing(12);
        let mut technical = column![].spacing(8);
        let mut has_technical_details = false;
        if let Some(project) = selection
            .project
            .and_then(|id| self.snapshot.projects.iter().find(|item| item.id == id))
        {
            has_technical_details = true;
            content = content
                .push(detail("Repository", project.name.clone()))
                .push(detail(
                    "Path",
                    human_readable_path(&project.canonical_repository_path),
                ))
                .push(detail(
                    "Last activity",
                    optional_human_timestamp(Some(project.last_activity_at), "Not recorded"),
                ));
            technical = technical
                .push(technical_detail("Repository ID", project.id.to_string()))
                .push(technical_detail(
                    "Workspace ID",
                    project.workspace_id.to_string(),
                ))
                .push(technical_detail(
                    "Canonical repository path",
                    project.canonical_repository_path.clone(),
                ))
                .push(technical_detail(
                    "Created (raw ms)",
                    project.created_at.to_string(),
                ))
                .push(technical_detail(
                    "Last activity (raw ms)",
                    project.last_activity_at.to_string(),
                ));
        }
        if let Some(worktree) = selection
            .worktree
            .and_then(|id| self.snapshot.worktrees.iter().find(|item| item.id == id))
        {
            has_technical_details = true;
            content = content
                .push(detail("Checkout", worktree.name.clone()))
                .push(detail(
                    "Branch",
                    worktree.branch.clone().unwrap_or_else(|| "Detached".into()),
                ))
                .push(detail(
                    "Path",
                    human_readable_path(&worktree.canonical_path),
                ))
                .push(detail(
                    "Created",
                    optional_human_timestamp(Some(worktree.created_at), "Not recorded"),
                ));
            technical = technical
                .push(technical_detail("Checkout ID", worktree.id.to_string()))
                .push(technical_detail(
                    "Project ID",
                    worktree.project_id.to_string(),
                ))
                .push(technical_detail(
                    "Canonical checkout path",
                    worktree.canonical_path.clone(),
                ))
                .push(technical_detail(
                    "Base commit",
                    worktree.base_commit.clone(),
                ))
                .push(technical_detail(
                    "Created (raw ms)",
                    worktree.created_at.to_string(),
                ));
        }
        if let Some(session) = selection.session.and_then(|id| self.session(id)) {
            has_technical_details = true;
            content = content
                .push(rule::horizontal(1))
                .push(text(&session.display_name).font(UI_SEMIBOLD).size(18))
                .push(detail(
                    "Provider",
                    session.provider_kind.display_name().to_owned(),
                ))
                .push(detail("State", session_state_label(session.state).into()))
                .push(detail(
                    "Working directory",
                    human_readable_path(&session.cwd),
                ))
                .push(detail(
                    "Started",
                    optional_human_timestamp(session.started_at, "Not started"),
                ))
                .push(detail(
                    "Ended",
                    optional_human_timestamp(session.ended_at, "Not ended"),
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
                    "Session record",
                    "Retained after the process ends until user data is removed.".into(),
                ))
                .push(detail(
                    "Terminal output",
                    "Available while the daemon is running; not persisted across daemon restart."
                        .into(),
                ))
                .push(detail(
                    "Resume",
                    session_resume_label(session, &self.snapshot.sessions, &self.providers).into(),
                ));
            technical = technical
                .push(technical_detail("Session ID", session.id.to_string()))
                .push(technical_detail(
                    "Checkout ID",
                    session.worktree_id.to_string(),
                ))
                .push(technical_detail(
                    "Canonical working directory",
                    session.cwd.clone(),
                ))
                .push(technical_detail(
                    "Process ID",
                    optional_number(session.process_id, "Not running"),
                ))
                .push(technical_detail(
                    "Provider session ID",
                    session
                        .external_session_id
                        .clone()
                        .unwrap_or_else(|| "Not recorded".into()),
                ))
                .push(technical_detail(
                    "Started (raw ms)",
                    optional_number(session.started_at, "Not started"),
                ))
                .push(technical_detail(
                    "Ended (raw ms)",
                    optional_number(session.ended_at, "Not ended"),
                ))
                .push(technical_detail(
                    "Last output sequence",
                    session.last_seen_output_sequence.to_string(),
                ))
                .push(technical_detail(
                    "Terminal controller",
                    self.terminals
                        .get(&session.id)
                        .map_or_else(|| "Detached".into(), |terminal| terminal.role.to_string()),
                ))
                .push(technical_detail(
                    "Transport",
                    "Authenticated local IPC with daemon-owned PTY".into(),
                ));
        }
        let mut actions = row![].spacing(8);
        if let Some(session) = selection.session.and_then(|id| self.session(id))
            && self.active_session_id == Some(session.id)
            && session_can_stop(session.state)
        {
            actions = actions.push(
                button(STOP_SESSION_LABEL)
                    .on_press(Message::Stop)
                    .style(flat_danger_style),
            );
        }
        if selection
            .worktree
            .and_then(|id| self.snapshot.worktrees.iter().find(|item| item.id == id))
            .is_some_and(worktree_can_delete)
            && selection.session.is_none()
        {
            actions = actions.push(
                button(DELETE_CHECKOUT_LABEL)
                    .on_press(Message::RemoveSelectedWorktree)
                    .style(flat_danger_style),
            );
        }
        content = content.push(rule::horizontal(1)).push(actions);
        if has_technical_details {
            content = content.push(rule::horizontal(1)).push(
                button(if self.disclosures.technical_details {
                    "Technical details — hide"
                } else {
                    "Technical details — show"
                })
                .on_press(Message::ToggleTechnicalDetails)
                .style(chrome_action_style),
            );
            if self.disclosures.technical_details {
                content = content.push(technical);
            }
        }
        container(scrollable(content).height(Fill))
            .padding(20)
            .width(Fill)
            .height(Fill)
            .style(workspace_surface)
            .into()
    }

    fn modal_view<'a>(&'a self, modal: &'a Modal) -> Element<'a, Message> {
        match modal {
            Modal::Settings => self.settings_view(),
            Modal::Shortcuts => Self::shortcuts_view(self.presentation().density),
            Modal::Form(form) => Self::form_view(form, self.presentation().density),
            Modal::Confirmation(confirmation) => Self::confirmation_view(confirmation),
            Modal::DataRemoval(confirmation) => Self::data_removal_view(confirmation),
        }
    }

    fn shortcuts_view(density: DensityMetrics) -> Element<'static, Message> {
        container(
            column![
                row![
                    text("Keyboard shortcuts").font(UI_SEMIBOLD).size(22),
                    space::horizontal(),
                    button(centered_button_label("Done", UI_META_SIZE, UI_MEDIUM))
                        .on_press(Message::CancelModal)
                        .height(density.control_height)
                        .padding([6, 12])
                ]
                .align_y(Center),
                detail("Tab / Shift+Tab", "Change navigation section".into()),
                detail("↑ / ↓", "Move through the active section".into()),
                detail("Enter", "Open the selected session".into()),
                detail("1 / 2 / 3", "Terminal / Changes / Details".into()),
                detail("N / R / D", "New / rename / stop or remove".into()),
                detail("Ctrl+,", "Open Settings".into()),
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
        let presentation = self.presentation();
        let density = presentation.density;
        let control_width = Length::Fixed(260.0);
        let appearance_controls = column![
            setting_row(
                "Density",
                pick_list(
                    DENSITY_CHOICES,
                    Some(self.desktop_state.density),
                    Message::SelectDensity,
                )
                .width(control_width),
            ),
            setting_row(
                "Terminal font",
                pick_list(
                    TERMINAL_FONT_CHOICES,
                    Some(self.desktop_state.terminal_font),
                    Message::SelectTerminalFont,
                )
                .width(control_width),
            ),
            setting_row(
                "Font size (px)",
                pick_list(
                    TERMINAL_FONT_SIZE_CHOICES,
                    Some(self.desktop_state.terminal_font_size),
                    Message::SetTerminalFontSize,
                )
                .width(control_width),
            ),
            setting_row(
                "Cursor",
                pick_list(
                    TERMINAL_CURSOR_CHOICES,
                    Some(self.desktop_state.terminal_cursor),
                    Message::SelectTerminalCursor,
                )
                .width(control_width),
            ),
        ]
        .spacing(density.region_spacing);
        let appearance = self.appearance_theme_gallery();
        let update_settings = self.update_settings_view();
        let advanced_settings = self.advanced_settings_view();
        let settings = column![
            row![
                text("Settings").font(UI_SEMIBOLD).size(24),
                space::horizontal(),
                button(line_icon(LineIcon::Close, 16))
                    .on_press(Message::ToggleSettings)
                    .width(density.control_height)
                    .height(density.control_height)
                    .padding(7)
                    .style(chrome_action_style)
            ]
            .align_y(Center),
            text("Tab and arrow keys move through themes · Enter applies · Esc closes")
                .size(UI_META_SIZE)
                .style(text::secondary),
            rule::horizontal(1),
            scrollable(
                column![
                    text("Essentials").font(UI_SEMIBOLD).size(16),
                    appearance,
                    appearance_controls,
                    rule::horizontal(1),
                    update_settings,
                    rule::horizontal(1),
                    advanced_settings,
                ]
                .spacing(16)
                .padding([8, 6]),
            )
            .height(Fill),
        ]
        .spacing(12);
        container(settings)
            .padding(22)
            .width(Fill)
            .max_width(640)
            .height(Fill)
            .style(modal_card)
            .into()
    }

    fn appearance_theme_gallery(&self) -> Element<'_, Message> {
        let density = self.presentation().density;
        let mut featured = row![].spacing(density.region_spacing);
        for choice in DesktopTheme::ALL.into_iter().take(FEATURED_THEME_COUNT) {
            featured = featured.push(self.theme_preview_card(choice));
        }
        featured.into()
    }

    fn advanced_settings_view(&self) -> Element<'_, Message> {
        let density = self.presentation().density;
        let disclosure = if self.disclosures.classic_themes {
            "Advanced — hide"
        } else {
            "Advanced — show"
        };
        let mut advanced = column![
            button(disclosure)
                .on_press(Message::ToggleClassicThemes)
                .height(density.control_height)
                .style(chrome_action_style),
        ]
        .spacing(10);
        if self.disclosures.classic_themes {
            let mut classic = row![].spacing(density.region_spacing);
            for choice in DesktopTheme::ALL.into_iter().skip(FEATURED_THEME_COUNT) {
                let selected = self.desktop_state.theme == choice;
                let focused = self.disclosures.settings_theme_focus == theme_gallery_index(choice);
                classic = classic.push(
                    button(centered_button_label(
                        choice.to_string(),
                        UI_META_SIZE,
                        UI_MEDIUM,
                    ))
                    .on_press(Message::SelectTheme(choice))
                    .height(density.control_height)
                    .padding([5, 9])
                    .style(move |theme, status| {
                        theme_preview_style(theme, status, selected, focused)
                    }),
                );
            }
            advanced = advanced
                .push(text("Classic themes").font(UI_SEMIBOLD).size(UI_META_SIZE))
                .push(
                    scrollable(classic)
                        .direction(scrollable::Direction::Horizontal(
                            scrollable::Scrollbar::default(),
                        ))
                        .height(density.control_height + 12),
                )
                .push(
                    button("Reset layout")
                        .on_press(Message::ResetLayout)
                        .style(chrome_action_style),
                )
                .push(text("Safety and data").font(UI_SEMIBOLD).size(UI_META_SIZE))
                .push(
                    button("Remove SylvOps user data…")
                        .on_press(Message::OpenDataRemoval)
                        .style(danger_action_style),
                );
        }
        advanced.into()
    }

    fn theme_preview_card(&self, choice: DesktopTheme) -> Element<'static, Message> {
        let system_appearance = match self.system_theme {
            iced::theme::Mode::Light => SystemAppearance::Light,
            iced::theme::Mode::None | iced::theme::Mode::Dark => SystemAppearance::Dark,
        };
        let preview = PresentationTheme::resolve(choice, system_appearance);
        let selected = self.desktop_state.theme == choice;
        let focused = self.disclosures.settings_theme_focus == theme_gallery_index(choice);
        let subtitle = if choice == DesktopTheme::System {
            format!("Uses {}", preview.label)
        } else {
            "Signature theme".to_owned()
        };
        button(
            column![
                row![
                    theme_swatch(preview.tokens.canvas),
                    theme_swatch(preview.tokens.surface),
                    theme_swatch(preview.tokens.interaction),
                ]
                .spacing(3),
                text(choice.to_string()).font(UI_MEDIUM).size(UI_META_SIZE),
                text(subtitle).size(11).style(text::secondary),
            ]
            .spacing(5),
        )
        .on_press(Message::SelectTheme(choice))
        .width(Length::Fixed(126.0))
        .height(74)
        .padding([8, 9])
        .style(move |theme, status| theme_preview_style(theme, status, selected, focused))
        .into()
    }

    fn update_settings_view(&self) -> Element<'_, Message> {
        let update_details: Element<'_, Message> = match &self.update_status {
            UpgradeStatus::UpToDate { version } => {
                text(format!("SylvOps {version} is up to date."))
                    .style(text::secondary)
                    .into()
            }
            UpgradeStatus::Available { release } => column![
                text(format!(
                    "SylvOps {} is available ({} download).",
                    release.target_version,
                    human_byte_size(release.byte_length)
                )),
                text(&release.release_notes).style(text::secondary),
            ]
            .spacing(8)
            .into(),
            UpgradeStatus::Downloading { release } => text(format!(
                "Downloading and verifying SylvOps {}…",
                release.target_version
            ))
            .into(),
            UpgradeStatus::Staged { release } => text(format!(
                "SylvOps {} is verified and ready.",
                release.target_version
            ))
            .into(),
            UpgradeStatus::Installing { release } => {
                text(format!("Installing SylvOps {}…", release.target_version)).into()
            }
            UpgradeStatus::Installed { version } => {
                text(format!("SylvOps {version} installed successfully.")).into()
            }
            UpgradeStatus::RolledBack { version } => text(format!(
                "SylvOps {version} did not pass health checks; the previous version was restored."
            ))
            .style(text::danger)
            .into(),
            UpgradeStatus::Failed { message } => text(bounded_redacted_diagnostic(message))
                .style(text::danger)
                .into(),
            UpgradeStatus::Idle => text("No update check has run in this desktop session.")
                .style(text::secondary)
                .into(),
        };
        let periodic_label = if self.desktop_state.periodic_update_checks {
            "Periodic checks: On"
        } else {
            "Periodic checks: Off"
        };
        let update_action: Element<'_, Message> = match &self.update_status {
            UpgradeStatus::Available { .. } => button("Download verified upgrade")
                .on_press(Message::DownloadUpdate)
                .style(primary_action_style)
                .into(),
            UpgradeStatus::Staged { .. } => button("Install update")
                .on_press(Message::InstallUpdate)
                .style(primary_action_style)
                .into(),
            UpgradeStatus::Downloading { .. } => button("Downloading…").into(),
            UpgradeStatus::Installing { .. } => button("Installing…").into(),
            _ => button(CHECK_AGAIN_LABEL)
                .on_press(Message::CheckForUpdate)
                .style(primary_action_style)
                .into(),
        };
        column![
            text("Updates").font(UI_SEMIBOLD).size(16),
            update_details,
            row![
                update_action,
                button(periodic_label)
                    .on_press(Message::TogglePeriodicUpdateChecks)
                    .style(chrome_action_style),
            ]
            .spacing(8),
        ]
        .spacing(8)
        .into()
    }

    fn form_view(modal: &FormModal, density: DensityMetrics) -> Element<'_, Message> {
        let form = &modal.form;
        let mut fields = column![].spacing(10);
        if modal.first_run {
            fields = fields.push(first_run_checklist(form.kind));
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
        let mut submit_button = button(submit)
            .on_press_maybe(
                (!modal.pending && !selected_provider_checking).then_some(Message::SubmitForm),
            )
            .style(primary_action_style);
        if form.is_session() {
            submit_button = submit_button.height(density.control_height);
        }
        container(
            column![
                text(&form.title).font(UI_SEMIBOLD).size(22),
                fields,
                row![
                    space::horizontal(),
                    button("Cancel")
                        .on_press_maybe((!modal.pending).then_some(Message::CancelModal)),
                    submit_button,
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
                } else if !item.can_start_interactive_session() {
                    "available, setup required"
                } else {
                    "ready"
                };
                format!("{} — {status}", item.kind.display_name())
            },
        );
        let recovery = provider.map_or_else(
            || "The daemon returned no providers. Check again to repeat discovery.".into(),
            provider_recovery_message,
        );
        let selected = provider.map(|provider| provider.kind);
        let mut choices = row![].spacing(8);
        for provider in &form.providers {
            let kind = provider.kind;
            let is_selected = selected == Some(kind);
            choices = choices.push(
                button(text(kind.display_name()).font(UI_MEDIUM))
                    .on_press_maybe((!form_pending).then_some(Message::SelectProvider(kind)))
                    .style(move |theme, status| content_tab_style(theme, status, is_selected)),
            );
        }
        let checking_selected = probing.is_some() && probing == selected;
        let retry_needed =
            provider.is_none_or(|provider| !provider.can_start_interactive_session());
        let recovery_row = if retry_needed {
            row![
                text(recovery).width(Fill).style(text::secondary),
                button(if checking_selected {
                    "Checking…"
                } else {
                    CHECK_AGAIN_LABEL
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
            text("Keyboard: Alt+← / Alt+→ cycles providers · Alt+R runs Check again")
                .size(UI_META_SIZE)
                .style(text::secondary),
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
                STOP_SESSION_LABEL,
                format!(
                    "Stop “{session_name}” in {cwd} and everything it started? Its Session record remains available."
                ),
            ),
            Confirmation::RemoveWorktree {
                name,
                canonical_path,
                ..
            } => (
                DELETE_CHECKOUT_LABEL,
                format!(
                    "Delete checkout “{name}” at {canonical_path}? The checkout directory is removed without force; the Git branch is preserved."
                ),
            ),
            Confirmation::InstallUpdate {
                version,
                active_sessions,
            } => (
                "Stop sessions and install update",
                format!(
                    "Install SylvOps {version}? The following active sessions and everything they started will stop: {}.",
                    active_sessions
                        .iter()
                        .map(|session| session.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ),
        };
        container(
            column![
                text(title).font(UI_SEMIBOLD).size(22),
                text(explanation),
                text(format!("Press Enter to {title}; press Escape to cancel."))
                    .size(UI_META_SIZE)
                    .style(text::secondary),
                row![
                    space::horizontal(),
                    button("Cancel").on_press(Message::CancelModal),
                    button(title)
                        .on_press(Message::ConfirmAction)
                        .style(focused_danger_action_style),
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

    fn data_removal_view(confirmation: &DataRemovalConfirmation) -> Element<'_, Message> {
        container(
            column![
                text("Remove SylvOps user data").font(UI_SEMIBOLD).size(22),
                text("Permanently remove configuration, preferences, logs, and session history. Repositories, worktrees, and Git branches are preserved."),
                text("All sessions must be stopped. Type DELETE SYLVOPS USER DATA to continue.")
                    .style(text::secondary),
                text_input(
                    "DELETE SYLVOPS USER DATA",
                    &confirmation.confirmation,
                )
                .on_input_maybe(
                    (!confirmation.pending).then_some(Message::DataRemovalConfirmationInput),
                )
                .on_submit_maybe(confirmation.can_submit().then_some(Message::SubmitDataRemoval)),
                row![
                    space::horizontal(),
                    button("Cancel")
                        .on_press_maybe((!confirmation.pending).then_some(Message::CancelModal)),
                    button(if confirmation.pending {
                        "Removing…"
                    } else {
                        "Remove user data"
                    })
                    .on_press_maybe(
                        confirmation
                            .can_submit()
                            .then_some(Message::SubmitDataRemoval),
                    )
                    .style(danger_action_style),
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

    fn footer(&self) -> Element<'_, Message> {
        let presentation = self.presentation();
        let density = presentation.density;
        let width = self.desktop_state.window_width;
        let workspace = self
            .active_workspace()
            .map_or_else(|| "None".to_owned(), |workspace| workspace.name.clone());
        let repository = presentation
            .selection
            .project_id
            .and_then(|id| {
                self.snapshot
                    .projects
                    .iter()
                    .find(|project| project.id == id)
            })
            .map_or_else(|| "None".to_owned(), |project| project.name.clone());
        let checkout = self.selected_worktree().map_or_else(
            || "None".to_owned(),
            |worktree| {
                format!(
                    "{} · {}",
                    worktree.name,
                    worktree.branch.as_deref().unwrap_or("detached")
                )
            },
        );
        let session = presentation
            .selection
            .active_session_id
            .and_then(|id| self.session(id))
            .map_or_else(
                || "None".to_owned(),
                |session| {
                    format!(
                        "{} · {}",
                        session.display_name,
                        session_state_label(session.state)
                    )
                },
            );
        let mut status = row![
            text(application_version_text())
                .font(UI_SEMIBOLD)
                .size(FOOTER_TEXT_SIZE)
                .wrapping(text::Wrapping::None),
            footer_connection(self.connection),
        ]
        .spacing(density.region_spacing)
        .align_y(Center);
        if width >= 720 {
            status = status
                .push(footer_separator())
                .push(footer_context("Workspace", &workspace));
        }
        if width >= 980 {
            status = status
                .push(footer_separator())
                .push(footer_context("Repository", &repository))
                .push(footer_separator())
                .push(footer_context("Checkout", &checkout));
        }
        if width >= 1_180 {
            status = status
                .push(footer_separator())
                .push(footer_context("Session", &session));
        }
        let mut trailing = row![footer_context(
            "View",
            main_tab_label(presentation.selection.main_tab),
        )]
        .spacing(density.region_spacing)
        .align_y(Center);
        if width >= 1_180 {
            trailing = trailing
                .push(footer_separator())
                .push(footer_item("Ctrl+K shortcuts"));
        }
        let contexts = scrollable(status)
            .direction(scrollable::Direction::Horizontal(
                scrollable::Scrollbar::hidden(),
            ))
            .width(Fill)
            .height(density.footer_height);
        container(
            row![contexts, trailing]
                .spacing(density.region_spacing)
                .align_y(Center),
        )
        .height(density.footer_height)
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
        for event in self.bridge.drain(MAX_EVENTS_PER_WAKE) {
            self.handle_bridge_event(event);
        }
    }

    #[allow(clippy::too_many_lines)]
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
                self.send_request(Operation::GetUpdateStatus, ClientRequest::GetUpdateStatus);
                self.restore_selection();
                if self.snapshot.workspaces.is_empty() {
                    self.modal = Some(Modal::Form(FormModal::first_run(Form::workspace())));
                    self.modal_focus = ModalFocus::Pending;
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
                let message = bounded_redacted_diagnostic(&message);
                if let Some(Operation::Resume(session_id)) = operation {
                    self.resume_pending.remove(&session_id);
                }
                if matches!(operation, Some(Operation::RefreshSnapshot)) {
                    self.snapshot_pending = false;
                }
                if matches!(operation, Some(Operation::SaveDesktopState)) {
                    self.state_save_pending = false;
                }
                if matches!(operation, Some(Operation::PrepareDataRemoval)) {
                    if let Some(Modal::DataRemoval(confirmation)) = &mut self.modal {
                        confirmation.pending = false;
                    }
                    self.error = Some(contextual_error(operation, &message));
                    return;
                }
                if matches!(&operation, Some(Operation::ProbeProvider(_))) {
                    if let Some(Modal::Form(modal)) = &mut self.modal {
                        modal.provider_probe = None;
                        modal.form.submission_error = Some(message);
                    } else {
                        self.error = Some(contextual_error(operation, &message));
                    }
                    return;
                }
                if matches!(
                    &operation,
                    Some(
                        Operation::RenameProject
                            | Operation::RenameWorktree
                            | Operation::RenameSession
                    )
                ) && let Some(rename) = &mut self.inline_navigator_rename
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
                    self.error = Some(contextual_error(operation, &message));
                }
            }
            BridgeEvent::DataRemovalFinished(result) => match result {
                Ok(()) => {
                    self.connection = ConnectionState::Disconnected;
                    self.modal = None;
                    self.show_success(
                        "SylvOps user data was removed; repositories, worktrees, and branches were preserved.",
                    );
                    self.closing_since = Some(Instant::now());
                }
                Err(message) => {
                    let message = bounded_redacted_diagnostic(&message);
                    self.connection = ConnectionState::Disconnected;
                    self.modal = None;
                    self.error = Some(format!(
                        "User-data removal did not complete: {message}. Run the data-removal command again to retry."
                    ));
                }
            },
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
            DaemonEvent::UpgradeProgress { status } => {
                if matches!(status, UpgradeStatus::Installing { .. })
                    && self.install_request == InstallRequestState::Idle
                {
                    self.closing_since = Some(Instant::now());
                }
                self.update_status = status;
            }
            _ => self.request_snapshot(),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn handle_response(&mut self, operation: Operation, response: DaemonResponse) {
        if matches!(operation, Operation::InstallUpdate) {
            self.install_request = InstallRequestState::Idle;
            if matches!(self.update_status, UpgradeStatus::Installing { .. }) {
                self.closing_since = Some(Instant::now());
            }
        }
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
            (
                Operation::Resume(source_session_id),
                DaemonResponse::SessionResumed { session, .. },
            ) => {
                self.resume_pending.remove(&source_session_id);
                let session_id = session.id;
                if let Some(existing) = self
                    .snapshot
                    .sessions
                    .iter_mut()
                    .find(|existing| existing.id == session_id)
                {
                    *existing = session;
                } else {
                    self.snapshot.sessions.push(session);
                }
                self.select_session_context(session_id);
                self.select_session(session_id);
                self.main_tab = MainTab::Terminal;
                self.show_success("Session resumed into a new history record.");
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
            (Operation::Resize, DaemonResponse::Acknowledged)
            | (Operation::PrepareDataRemoval, DaemonResponse::DataRemovalPrepared) => {}
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
                if first_run {
                    self.modal_focus = ModalFocus::Pending;
                }
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
                if first_run {
                    self.modal_focus = ModalFocus::Pending;
                }
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
                self.inline_navigator_rename = None;
                self.show_success("Display name updated.");
                self.request_snapshot();
            }
            (Operation::RenameSession, DaemonResponse::SessionUpdated { .. }) => {
                self.inline_navigator_rename = None;
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
            (Operation::CheckForUpdate, DaemonResponse::UpdateAvailable(release)) => {
                self.update_status = UpgradeStatus::Available {
                    release: release.clone(),
                };
                self.show_success(format!("SylvOps {} is available.", release.target_version));
            }
            (Operation::CheckForUpdate, DaemonResponse::UpdateNotAvailable { version }) => {
                self.update_status = UpgradeStatus::UpToDate {
                    version: version.clone(),
                };
                self.show_success(format!("SylvOps {version} is up to date."));
            }
            (Operation::GetUpdateStatus, DaemonResponse::UpdateStatus(status)) => {
                self.update_status = status;
            }
            (Operation::DownloadUpdate, DaemonResponse::UpdateStaged(release)) => {
                self.update_status = UpgradeStatus::Staged {
                    release: release.clone(),
                };
                self.show_success(format!(
                    "SylvOps {} downloaded and verified.",
                    release.target_version
                ));
            }
            (
                Operation::InstallUpdate,
                DaemonResponse::UpdateInstall(InstallDisposition::Blocked { active_sessions }),
            ) => {
                let version = match &self.update_status {
                    UpgradeStatus::Staged { release } => release.target_version.clone(),
                    _ => "the staged version".into(),
                };
                self.modal = Some(Modal::Confirmation(Confirmation::InstallUpdate {
                    version,
                    active_sessions,
                }));
            }
            (
                Operation::InstallUpdate,
                DaemonResponse::UpdateInstall(InstallDisposition::Prepared { version }),
            ) => {
                let release = match &self.update_status {
                    UpgradeStatus::Staged { release } => release.clone(),
                    _ => return,
                };
                self.update_status = UpgradeStatus::Installing { release };
                self.show_success(format!(
                    "SylvOps {version} is installing; the desktop will relaunch after health checks."
                ));
                self.closing_since = Some(Instant::now());
            }
            (
                Operation::InstallUpdate,
                DaemonResponse::UpdateInstall(InstallDisposition::Installed),
            ) => self.show_success("Application update installed and health-checked."),
            (
                Operation::InstallUpdate,
                DaemonResponse::UpdateInstall(InstallDisposition::RolledBack),
            ) => {
                self.error = Some(
                    "The updated application failed its health check; the previous version was restored."
                        .into(),
                );
            }
            (operation, DaemonResponse::Error(failure)) => {
                let message = bounded_redacted_diagnostic(&failure.message);
                if let Operation::Resume(session_id) = operation {
                    self.resume_pending.remove(&session_id);
                }
                if matches!(operation, Operation::PrepareDataRemoval) {
                    if let Some(Modal::DataRemoval(confirmation)) = &mut self.modal {
                        confirmation.pending = false;
                    }
                    self.error = Some(contextual_error(Some(operation), &message));
                    return;
                }
                if matches!(operation, Operation::ProbeProvider(_)) {
                    if let Some(Modal::Form(modal)) = &mut self.modal {
                        modal.provider_probe = None;
                        modal.form.submission_error = Some(message);
                    } else {
                        self.error = Some(contextual_error(Some(operation), &message));
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
                    modal.form.submission_error = Some(message);
                } else {
                    self.error = Some(contextual_error(Some(operation), &message));
                }
            }
            (_operation, _response) => {
                self.error = Some(
                    "The background service returned an unexpected response. Check again or restart SylvOps."
                        .into(),
                );
            }
        }
    }

    fn send_request(&mut self, operation: Operation, request: ClientRequest) {
        if !self.bridge.request(operation, request) {
            self.error = Some("The background service is busy. Try again.".into());
        }
    }

    fn send_install_request(&mut self, confirmed_active_sessions: Vec<SessionId>) {
        self.install_request = if self.bridge.request(
            Operation::InstallUpdate,
            ClientRequest::InstallUpdate {
                confirmed_active_sessions,
                requesting_process_id: std::process::id(),
            },
        ) {
            InstallRequestState::Pending
        } else {
            InstallRequestState::Idle
        };
        if self.install_request == InstallRequestState::Idle {
            self.error = Some("The background service is busy. Try again.".into());
        }
    }

    fn resume_session(&mut self, session_id: SessionId) {
        let Some(request) = self.begin_resume_request(session_id) else {
            return;
        };
        if !self.bridge.request(Operation::Resume(session_id), request) {
            self.resume_pending.remove(&session_id);
            self.error = Some("The background service is busy. Try again.".into());
        }
    }

    fn begin_resume_request(&mut self, session_id: SessionId) -> Option<ClientRequest> {
        let Some(session) = self.session(session_id) else {
            self.error =
                Some("The selected session is no longer available. Refresh and try again.".into());
            return None;
        };
        if !session_can_resume(session, &self.snapshot.sessions, &self.providers) {
            self.error = Some(
                "This session is not eligible for resume. Refresh to load its latest state.".into(),
            );
            return None;
        }
        if self.resume_pending.contains(&session_id) {
            return None;
        }
        let (columns, rows) = self.terminal_dimensions();
        self.resume_pending.insert(session_id);
        Some(ClientRequest::ResumeSession {
            session_id,
            columns,
            rows,
        })
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

    #[allow(clippy::too_many_lines)]
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
        if self.handle_global_shortcut(&key, modifiers) {
            return;
        }
        if self.handle_transient_keyboard(&key, modifiers) {
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
            Key::Named(Named::Delete) => self.shortcut_delete(),
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

    fn handle_transient_keyboard(&mut self, key: &Key, modifiers: keyboard::Modifiers) -> bool {
        if matches!(self.modal, Some(Modal::Settings)) {
            match key.as_ref() {
                Key::Named(Named::Escape) => self.modal = None,
                Key::Named(Named::Tab | Named::ArrowRight | Named::ArrowDown) => {
                    let count = theme_gallery_len(self.disclosures.classic_themes);
                    if modifiers.shift() {
                        self.disclosures.settings_theme_focus = self
                            .disclosures
                            .settings_theme_focus
                            .checked_sub(1)
                            .unwrap_or(count.saturating_sub(1));
                    } else {
                        self.disclosures.settings_theme_focus =
                            (self.disclosures.settings_theme_focus + 1) % count;
                    }
                }
                Key::Named(Named::ArrowLeft | Named::ArrowUp) => {
                    let count = theme_gallery_len(self.disclosures.classic_themes);
                    self.disclosures.settings_theme_focus = self
                        .disclosures
                        .settings_theme_focus
                        .checked_sub(1)
                        .unwrap_or(count.saturating_sub(1));
                }
                Key::Named(Named::Enter | Named::Space) => {
                    if let Some(theme) = theme_gallery_choice(
                        self.disclosures.settings_theme_focus,
                        self.disclosures.classic_themes,
                    ) {
                        self.desktop_state.theme = theme;
                        self.mark_state_dirty();
                    }
                }
                Key::Character(value) if value.eq_ignore_ascii_case("c") => {
                    self.disclosures.classic_themes = !self.disclosures.classic_themes;
                    let count = theme_gallery_len(self.disclosures.classic_themes);
                    self.disclosures.settings_theme_focus = self
                        .disclosures
                        .settings_theme_focus
                        .min(count.saturating_sub(1));
                }
                Key::Character(value) if value.eq_ignore_ascii_case("u") => {
                    match &self.update_status {
                        UpgradeStatus::Available { .. } => self
                            .send_request(Operation::DownloadUpdate, ClientRequest::DownloadUpdate),
                        UpgradeStatus::Staged { .. } => self.send_install_request(Vec::new()),
                        _ => self
                            .send_request(Operation::CheckForUpdate, ClientRequest::CheckForUpdate),
                    }
                }
                Key::Character(value) if value.eq_ignore_ascii_case("p") => {
                    self.desktop_state.periodic_update_checks =
                        !self.desktop_state.periodic_update_checks;
                    self.mark_state_dirty();
                }
                Key::Character(value) if value.eq_ignore_ascii_case("r") => {
                    state::reset_layout(&mut self.desktop_state);
                    for (index, split) in self.pane_splits.iter().copied().enumerate() {
                        self.panes.resize(
                            split,
                            f32::from(self.desktop_state.panel_ratios[index]) / 1000.0,
                        );
                    }
                    self.mark_state_dirty();
                }
                Key::Character(value) if value.eq_ignore_ascii_case("d") => {
                    self.open_data_removal_confirmation();
                }
                _ => {}
            }
            true
        } else if matches!(self.modal, Some(Modal::Confirmation(_))) {
            match key {
                Key::Named(Named::Escape) => self.modal = None,
                Key::Named(Named::Enter) => self.confirm_action(),
                _ => {}
            }
            true
        } else if self.handle_session_form_shortcut(key, modifiers) {
            true
        } else if self.modal.is_some() {
            if matches!(key, Key::Named(Named::Escape)) {
                self.modal = None;
            }
            true
        } else if self.inline_navigator_rename.is_some() {
            if matches!(key, Key::Named(Named::Escape)) {
                cancel_inline_navigator_rename(&mut self.inline_navigator_rename);
            }
            true
        } else {
            false
        }
    }

    fn handle_global_shortcut(&mut self, key: &Key, modifiers: keyboard::Modifiers) -> bool {
        if self.modal.is_none() && is_open_settings_shortcut(key, modifiers) {
            self.open_settings();
            true
        } else {
            false
        }
    }

    fn handle_session_form_shortcut(&mut self, key: &Key, modifiers: keyboard::Modifiers) -> bool {
        if !matches!(self.modal, Some(Modal::Form(ref modal)) if modal.form.is_session()) {
            return false;
        }
        if matches!(key, Key::Named(Named::Escape)) {
            self.modal = None;
            return true;
        }
        if !modifiers.alt() {
            return true;
        }
        match key.as_ref() {
            Key::Named(Named::ArrowLeft) => {
                self.select_adjacent_provider(-1);
            }
            Key::Named(Named::ArrowRight) => {
                self.select_adjacent_provider(1);
            }
            Key::Character(value) if value.eq_ignore_ascii_case("s") => {
                self.select_provider(ProviderKind::Shell);
            }
            Key::Character(value) if value.eq_ignore_ascii_case("c") => {
                self.select_provider(ProviderKind::Codex);
            }
            Key::Character(value) if value.eq_ignore_ascii_case("l") => {
                self.select_provider(ProviderKind::Claude);
            }
            Key::Character(value) if value.eq_ignore_ascii_case("r") => {
                self.probe_selected_provider();
            }
            _ => {}
        }
        true
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

    fn open_settings(&mut self) {
        self.inline_navigator_rename = None;
        self.terminal_focus = TerminalFocus::Unfocused;
        self.disclosures.classic_themes = false;
        self.disclosures.settings_theme_focus =
            theme_gallery_index(self.desktop_state.theme).min(FEATURED_THEME_COUNT - 1);
        self.modal = Some(Modal::Settings);
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
        self.inline_navigator_rename = None;
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
        let _ = self.begin_selected_navigator_rename();
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

    fn run_navigator_action(&mut self, action: NavigatorAction) -> Task<Message> {
        match action {
            NavigatorAction::DeleteCheckout(worktree_id) => {
                self.select_worktree(worktree_id);
                self.inspect_worktree_removal();
            }
        }
        Task::none()
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
        self.inline_navigator_rename = None;
        let Some(workspace_id) = self.active_workspace().map(|workspace| workspace.id) else {
            self.error = Some("Create a workspace before registering a repository.".into());
            self.modal = Some(Modal::Form(FormModal::first_run(Form::workspace())));
            self.modal_focus = ModalFocus::Pending;
            return;
        };
        let form = Form::repository(workspace_id);
        self.modal = Some(Modal::Form(
            if self
                .snapshot
                .projects
                .iter()
                .all(|project| project.workspace_id != workspace_id)
            {
                FormModal::first_run(form)
            } else {
                FormModal::new(form)
            },
        ));
        self.modal_focus = ModalFocus::Pending;
    }

    fn open_worktree_form(&mut self) {
        self.inline_navigator_rename = None;
        let Some(project_id) = self.selected_project_id else {
            self.error = Some("Select a repository before creating a checkout.".into());
            return;
        };
        self.modal = Some(Modal::Form(FormModal::new(Form::worktree(project_id))));
        self.modal_focus = ModalFocus::Pending;
    }

    fn open_session_form(&mut self) {
        self.inline_navigator_rename = None;
        let Some(worktree_id) = self.selected_worktree_id else {
            self.error = Some("Select a checkout before starting a session.".into());
            return;
        };
        let form = Form::session(worktree_id, self.providers.clone());
        self.modal = Some(Modal::Form(if self.snapshot.sessions.is_empty() {
            FormModal::first_run(form)
        } else {
            FormModal::new(form)
        }));
        self.modal_focus = ModalFocus::Pending;
    }

    fn begin_selected_navigator_rename(&mut self) -> Task<Message> {
        let target = match self.keyboard_panel {
            DesktopPanel::Projects => self.selected_project_id.map(NavigatorNodeId::Repository),
            DesktopPanel::Worktrees => self.selected_worktree_id.map(NavigatorNodeId::Checkout),
            DesktopPanel::Sessions => self.selected_session_id.map(NavigatorNodeId::Session),
        };
        target.map_or_else(Task::none, |target| self.begin_navigator_rename(target))
    }

    fn begin_navigator_rename(&mut self, target: NavigatorNodeId) -> Task<Message> {
        let value = match target {
            NavigatorNodeId::Repository(id) => self
                .snapshot
                .projects
                .iter()
                .find(|item| item.id == id)
                .map(|item| item.name.clone()),
            NavigatorNodeId::Checkout(id) => self
                .snapshot
                .worktrees
                .iter()
                .find(|item| item.id == id)
                .map(|item| item.name.clone()),
            NavigatorNodeId::Session(id) => self.session(id).map(|item| item.display_name.clone()),
        };
        let Some(value) = value else {
            self.error = Some("Select an item to rename.".into());
            return Task::none();
        };
        match target {
            NavigatorNodeId::Repository(id) => self.select_project(id),
            NavigatorNodeId::Checkout(id) => self.select_worktree(id),
            NavigatorNodeId::Session(id) => self.select_session(id),
        }
        let rename = InlineNavigatorRename::new(target, &value);
        let input_id = rename.input_id.clone();
        self.inline_navigator_rename = Some(rename);
        self.narrow_main = false;
        self.terminal_focus = TerminalFocus::Unfocused;
        Task::batch([
            self.take_navigator_reveal_task(),
            widget::operation::focus(input_id.clone()),
            widget::operation::select_all(input_id),
        ])
    }

    fn submit_navigator_rename(&mut self) {
        let Some((target, name)) = self
            .inline_navigator_rename
            .as_mut()
            .and_then(|rename| rename.begin_submission().map(|name| (rename.target, name)))
        else {
            return;
        };
        let sent = match target {
            NavigatorNodeId::Repository(project_id) => self.bridge.request(
                Operation::RenameProject,
                ClientRequest::RenameProject { project_id, name },
            ),
            NavigatorNodeId::Checkout(worktree_id) => self.bridge.request(
                Operation::RenameWorktree,
                ClientRequest::RenameWorktree { worktree_id, name },
            ),
            NavigatorNodeId::Session(session_id) => self.bridge.request(
                Operation::RenameSession,
                ClientRequest::RenameSession { session_id, name },
            ),
        };
        if !sent && let Some(rename) = &mut self.inline_navigator_rename {
            rename.fail("The background service is busy. Try again.".into());
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

    fn select_adjacent_provider(&mut self, delta: isize) {
        if let Some(Modal::Form(modal)) = &mut self.modal
            && !modal.pending
        {
            modal.form.select_next_provider(delta);
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
            modal.form.submission_error = Some("The background service is busy. Try again.".into());
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
                let (columns, rows) = self.terminal_dimensions();
                let Some(request) = form.session_request(columns, rows) else {
                    return;
                };
                self.send_request(Operation::CreateSession(worktree_id), request);
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

    fn open_data_removal_confirmation(&mut self) {
        self.modal = Some(Modal::DataRemoval(DataRemovalConfirmation::default()));
        self.modal_focus = ModalFocus::Pending;
    }

    fn submit_data_removal(&mut self) {
        let Some(Modal::DataRemoval(confirmation)) = &mut self.modal else {
            return;
        };
        if !confirmation.can_submit() {
            return;
        }
        let phrase = confirmation.confirmation.clone();
        if self.bridge.remove_user_data(phrase) {
            confirmation.begin_submission();
        } else {
            self.error = Some("The desktop command queue is full. Try again.".into());
        }
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
            Confirmation::InstallUpdate {
                active_sessions, ..
            } => {
                self.send_install_request(
                    active_sessions
                        .into_iter()
                        .map(|session| session.id)
                        .collect(),
                );
                self.modal = None;
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
        self.keyboard_panel = state.compact_panel;
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
        let main_width = match self.presentation().layout {
            PresentationLayout::Wide => {
                let mut remaining = u32::from(self.desktop_state.window_width);
                for ratio in self.desktop_state.panel_ratios {
                    let pane = remaining.saturating_mul(u32::from(ratio)) / 1000;
                    remaining = remaining.saturating_sub(pane);
                }
                u16::try_from(remaining).unwrap_or(u16::MAX)
            }
            PresentationLayout::Compact => self.desktop_state.window_width.saturating_sub(
                if self.navigator_visibility == NavigatorVisibility::Collapsed {
                    42
                } else {
                    305
                },
            ),
            PresentationLayout::Narrow => self.desktop_state.window_width,
        };
        let font = u16::from(self.desktop_state.terminal_font_size);
        let columns = (main_width / font.saturating_mul(3).saturating_div(5).max(1)).clamp(1, 500);
        let rows = (self.desktop_state.window_height.saturating_sub(150) / font.saturating_add(3))
            .clamp(1, 200);
        (columns, rows)
    }

    fn restore_selection(&mut self) {
        retain_inline_navigator_rename(&mut self.inline_navigator_rename, &self.snapshot);
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
        self.pending_navigator_reveal = Some(DesktopPanel::Projects);
        if self.selected_project_id == Some(project_id) {
            self.keyboard_panel = DesktopPanel::Projects;
            self.desktop_state.compact_panel = DesktopPanel::Projects;
            return;
        }
        self.inline_navigator_rename = None;
        self.keyboard_panel = DesktopPanel::Projects;
        self.desktop_state.compact_panel = DesktopPanel::Projects;
        self.selected_project_id = Some(project_id);
        self.selected_worktree_id = self.worktrees_for(project_id).first().map(|item| item.id);
        self.selected_session_id = self
            .selected_worktree_id
            .and_then(|id| self.sessions_for(id).first().map(|item| item.id));
        self.diff = None;
        self.mark_state_dirty();
    }

    fn select_worktree(&mut self, worktree_id: WorktreeId) {
        self.unfocus_terminal();
        self.pending_navigator_reveal = Some(DesktopPanel::Worktrees);
        if self.selected_worktree_id == Some(worktree_id) {
            self.keyboard_panel = DesktopPanel::Worktrees;
            self.desktop_state.compact_panel = DesktopPanel::Worktrees;
            return;
        }
        self.inline_navigator_rename = None;
        self.keyboard_panel = DesktopPanel::Worktrees;
        self.desktop_state.compact_panel = DesktopPanel::Worktrees;
        self.selected_worktree_id = Some(worktree_id);
        self.selected_project_id = self
            .snapshot
            .worktrees
            .iter()
            .find(|item| item.id == worktree_id)
            .map(|item| item.project_id);
        self.selected_session_id = self.sessions_for(worktree_id).first().map(|item| item.id);
        self.diff = None;
        self.mark_state_dirty();
    }

    fn select_session(&mut self, session_id: SessionId) {
        self.unfocus_terminal();
        self.pending_navigator_reveal = Some(DesktopPanel::Sessions);
        if self.selected_session_id == Some(session_id)
            && self.active_session_id == Some(session_id)
        {
            self.keyboard_panel = DesktopPanel::Sessions;
            self.desktop_state.compact_panel = DesktopPanel::Sessions;
            return;
        }
        if self
            .inline_navigator_rename
            .as_ref()
            .is_some_and(|rename| rename.target != NavigatorNodeId::Session(session_id))
        {
            self.inline_navigator_rename = None;
        }
        self.select_session_context(session_id);
        self.keyboard_panel = DesktopPanel::Sessions;
        self.desktop_state.compact_panel = DesktopPanel::Sessions;
        self.active_session_id = Some(session_id);
        if !self.open_sessions.contains(&session_id) {
            if self.open_sessions.len() == MAX_OPEN_DESKTOP_SESSIONS {
                self.open_sessions.remove(0);
            }
            self.open_sessions.push(session_id);
        }
        self.mark_state_dirty();
        if self.presentation().layout == PresentationLayout::Narrow {
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

    fn take_navigator_reveal_task(&mut self) -> Task<Message> {
        let Some(panel) = self.pending_navigator_reveal.take() else {
            return Task::none();
        };
        let presentation = self.presentation();
        let rows = match panel {
            DesktopPanel::Projects => &presentation.navigator.repositories,
            DesktopPanel::Worktrees => &presentation.navigator.checkouts,
            DesktopPanel::Sessions => &presentation.navigator.sessions,
        };
        let Some(index) = rows.iter().position(|row| row.selected) else {
            return Task::none();
        };
        widget::operation::snap_to(
            navigator_scroll_id(panel),
            widget::operation::RelativeOffset {
                x: None,
                y: Some(navigator_reveal_offset(index, rows.len())),
            },
        )
    }

    fn active_workspace(&self) -> Option<&Workspace> {
        let workspace_id = self.presentation().selection.workspace_id?;
        self.snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)
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
        self.presentation().selection.worktree_id.and_then(|id| {
            self.snapshot
                .worktrees
                .iter()
                .find(|worktree| worktree.id == id)
        })
    }
}

fn subscription(app: &DesktopApp) -> Subscription<Message> {
    Subscription::batch([
        app.bridge.subscription().map(|()| Message::BridgeReady),
        time::every(TIMER_TICK).map(|_| Message::Tick),
        keyboard::listen().map(Message::Keyboard),
        window::resize_events().map(|(id, size)| Message::WindowResized(id, size)),
        window::close_requests().map(Message::CloseRequested),
        system::theme_changes().map(Message::SystemThemeChanged),
    ])
}

fn optional_number<T: ToString>(value: Option<T>, fallback: &str) -> String {
    value.map_or_else(|| fallback.to_owned(), |value| value.to_string())
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

fn human_readable_timestamp(timestamp_millis: i64) -> Option<String> {
    let locale = sys_locale::get_locale().unwrap_or_else(|| "en-US".into());
    human_readable_timestamp_for_locale(timestamp_millis, &locale)
}

fn human_readable_timestamp_for_locale(timestamp_millis: i64, locale: &str) -> Option<String> {
    let locale = chrono_locale(locale);
    DateTime::from_timestamp_millis(timestamp_millis).map(|timestamp| {
        timestamp
            .with_timezone(&Local)
            .format_localized("%x %X %Z", locale)
            .to_string()
    })
}

fn chrono_locale(locale: &str) -> Locale {
    locale
        .split('.')
        .next()
        .unwrap_or(locale)
        .replace('-', "_")
        .parse()
        .unwrap_or(Locale::en_US)
}

fn optional_human_timestamp(value: Option<i64>, fallback: &str) -> String {
    value
        .and_then(human_readable_timestamp)
        .unwrap_or_else(|| fallback.to_owned())
}

fn bounded_technical_copy(value: &str) -> String {
    value.chars().take(MAX_TECHNICAL_COPY_CHARS).collect()
}

fn bounded_redacted_diagnostic(message: &str) -> String {
    let normalized: String = message
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect();
    let lowercase = normalized.to_ascii_lowercase();
    if [
        "password",
        "token",
        "secret",
        "authorization",
        "bearer",
        "api_key",
        "api-key",
        "apikey",
    ]
    .iter()
    .any(|marker| lowercase.contains(marker))
    {
        return "Diagnostic contained sensitive data and was [redacted].".into();
    }
    normalized
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(MAX_USER_DIAGNOSTIC_CHARS)
        .collect()
}

fn contextual_error(operation: Option<Operation>, message: &str) -> String {
    let context = operation.map_or("Action", operation_error_context);
    bounded_redacted_diagnostic(&format!("{context}: {message}"))
}

const fn operation_error_context(operation: Operation) -> &'static str {
    match operation {
        Operation::RefreshSnapshot => "Refresh",
        Operation::ProbeProvider(_) => "Provider check",
        Operation::CreateWorkspace => "Create workspace",
        Operation::OpenWorkspace(_) => "Open workspace",
        Operation::RegisterProject => "Add repository",
        Operation::CreateWorktree => "Create checkout",
        Operation::CreateSession(_) => "Start session",
        Operation::Resume(_) => "Resume session",
        Operation::RenameProject => "Rename repository",
        Operation::RenameWorktree => "Rename checkout",
        Operation::RenameSession => "Rename session",
        Operation::InspectRemoval(_) | Operation::RemoveWorktree => "Delete checkout",
        Operation::Attach(_) => "Open terminal",
        Operation::Detach(_) => "Leave terminal",
        Operation::Stop(_) => "Stop session",
        Operation::Resize => "Resize terminal",
        Operation::LoadDiff(_) => "Load changes",
        Operation::Input(_) => "Terminal input",
        Operation::SaveDesktopState => "Save desktop preferences",
        Operation::CheckForUpdate => "Check for updates",
        Operation::GetUpdateStatus => "Load update status",
        Operation::DownloadUpdate => "Download update",
        Operation::InstallUpdate => "Install update",
        Operation::PrepareDataRemoval => "Remove SylvOps user data",
    }
}

fn human_byte_size(bytes: u64) -> String {
    const KIB: u64 = 1_024;
    const MIB: u64 = KIB * 1_024;
    const GIB: u64 = MIB * 1_024;
    let (unit, suffix) = if bytes >= GIB {
        (GIB, "GiB")
    } else if bytes >= MIB {
        (MIB, "MiB")
    } else if bytes >= KIB {
        (KIB, "KiB")
    } else {
        return format!("{bytes} B");
    };
    let tenths = bytes.saturating_mul(10) / unit;
    format!("{}.{:01} {suffix}", tenths / 10, tenths % 10)
}

fn session_resume_label(
    session: &Session,
    sessions: &[Session],
    providers: &[ProviderHealth],
) -> &'static str {
    if session_can_resume(session, sessions, providers) {
        "Available"
    } else {
        "Unavailable"
    }
}

const fn terminal_clipboard_shortcut() -> &'static str {
    if cfg!(target_os = "macos") {
        "⌘C / ⌘V"
    } else {
        "Ctrl+Shift+C / V"
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

fn selected_navigator_actions(rows: &[NavigatorRow]) -> &[NavigatorAction] {
    rows.iter()
        .find(|row| row.selected)
        .map_or(&[], |row| row.actions.as_slice())
}

fn navigator_panel<'a>(
    title: &'static str,
    heading_action: Option<(LineIcon, Message)>,
    actions: &[NavigatorAction],
    density: DensityMetrics,
    items: impl Into<Element<'a, Message>>,
    panel: DesktopPanel,
) -> Element<'a, Message> {
    container(
        column![
            navigator_heading(title, heading_action, density),
            navigator_action_strip(actions, density),
            scrollable(items)
                .id(navigator_scroll_id(panel))
                .height(Fill),
        ]
        .width(Fill)
        .height(Fill),
    )
    .width(Fill)
    .height(Fill)
    .padding([4, 5])
    .style(panel_surface)
    .into()
}

fn navigator_scroll_id(panel: DesktopPanel) -> widget::Id {
    widget::Id::new(match panel {
        DesktopPanel::Projects => "navigator-repositories",
        DesktopPanel::Worktrees => "navigator-checkouts",
        DesktopPanel::Sessions => "navigator-sessions",
    })
}

fn navigator_reveal_offset(index: usize, row_count: usize) -> f32 {
    let last = row_count.saturating_sub(1);
    if last == 0 {
        return 0.0;
    }
    let index = u16::try_from(index.min(last)).unwrap_or(u16::MAX);
    let last = u16::try_from(last).unwrap_or(u16::MAX);
    f32::from(index) / f32::from(last)
}

fn navigator_heading(
    title: &'static str,
    action: Option<(LineIcon, Message)>,
    density: DensityMetrics,
) -> Element<'static, Message> {
    let mut title_row =
        row![text(title).font(UI_SEMIBOLD).size(15), space::horizontal(),].align_y(Center);
    if let Some((icon, message)) = action {
        title_row = title_row.push(
            button(line_icon(icon, 16))
                .on_press(message)
                .width(density.control_height)
                .height(density.control_height)
                .padding(7)
                .style(borderless_icon_style),
        );
    }
    container(title_row)
        .height(density.panel_header_height)
        .padding([5, 7])
        .width(Fill)
        .align_y(Vertical::Center)
        .into()
}

fn navigator_action_strip(
    actions: &[NavigatorAction],
    density: DensityMetrics,
) -> Element<'static, Message> {
    let mut controls = row![].spacing(4).align_y(Center);
    for action in actions.iter().copied() {
        controls = controls.push(match action {
            NavigatorAction::DeleteCheckout(_) => button(icon_text_label(
                LineIcon::Delete,
                DELETE_CHECKOUT_LABEL,
                UI_META_SIZE,
            ))
            .on_press(Message::RunNavigatorAction(action))
            .height(density.control_height)
            .padding([3, 7])
            .style(flat_danger_style),
        });
    }
    container(
        scrollable(controls)
            .direction(scrollable::Direction::Horizontal(
                scrollable::Scrollbar::hidden(),
            ))
            .width(Fill)
            .height(density.control_height),
    )
    .width(Fill)
    .height(density.control_height + 8)
    .padding([4, 7])
    .into()
}

fn compact_panel_button(
    label: &'static str,
    panel: DesktopPanel,
    active: DesktopPanel,
    density: DensityMetrics,
) -> Element<'static, Message> {
    button(centered_button_label(label, UI_META_SIZE, UI_MEDIUM))
        .on_press(Message::SelectCompactPanel(panel))
        .height(density.control_height)
        .padding([5, 8])
        .style(move |theme, status| content_tab_style(theme, status, panel == active))
        .into()
}

fn icon_text_label(
    icon: LineIcon,
    label: &'static str,
    size: f32,
) -> iced::widget::Row<'static, Message> {
    row![line_icon(icon, 14), text(label).font(UI_MEDIUM).size(size)]
        .spacing(5)
        .align_y(Center)
}

const fn worktree_can_delete(worktree: &Worktree) -> bool {
    checkout_delete_available(worktree.is_root_checkout, worktree.status)
}

const fn checkout_delete_available(is_root_checkout: bool, status: WorktreeStatus) -> bool {
    !is_root_checkout && matches!(status, WorktreeStatus::Active)
}

const fn navigator_attention_icon(attention: NavigatorAttention) -> LineIcon {
    match attention {
        NavigatorAttention::Finished => LineIcon::Finished,
        NavigatorAttention::NeedsFeedback => LineIcon::NeedsFeedback,
        NavigatorAttention::Failed => LineIcon::Failed,
    }
}

const fn navigator_attention_label(attention: NavigatorAttention) -> &'static str {
    match attention {
        NavigatorAttention::Finished => "Finished unseen",
        NavigatorAttention::NeedsFeedback => "Needs feedback",
        NavigatorAttention::Failed => "Failed",
    }
}

fn tab_button(
    label: &str,
    tab: MainTab,
    active: MainTab,
    density: DensityMetrics,
) -> Element<'_, Message> {
    let selected = tab == active;
    button(
        centered_button_label(
            label.to_owned(),
            UI_TEXT_SIZE,
            if selected { UI_SEMIBOLD } else { UI_FONT },
        )
        .wrapping(text::Wrapping::None),
    )
    .on_press(Message::SelectMainTab(tab))
    .height(density.control_height)
    .padding([6, 14])
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
        return "Checking provider…";
    }
    match form.kind {
        FormKind::CreateWorkspace => "Create workspace",
        FormKind::RegisterProject(_) => "Register repository",
        FormKind::CreateWorktree(_) => "Create checkout",
        FormKind::CreateSession(_) => match form.provider().map(|provider| provider.kind) {
            Some(ProviderKind::Codex) => "Start Codex",
            Some(ProviderKind::Claude) => "Start Claude Code",
            Some(ProviderKind::Shell) => "Start shell",
            Some(_) | None => "Start session",
        },
        FormKind::RenameProject(_) | FormKind::RenameWorktree(_) | FormKind::RenameSession(_) => {
            "Save name"
        }
    }
}

fn first_run_checklist(kind: FormKind) -> Element<'static, Message> {
    let Some(steps) = first_run_steps(kind) else {
        return space::vertical().height(0).into();
    };
    let mut checklist = column![
        text("Get started").font(UI_SEMIBOLD).size(16),
        text("Create a workspace, add a repository, then start a session")
            .size(UI_META_SIZE)
            .style(text::secondary),
    ]
    .spacing(7);
    for (index, step) in steps.into_iter().enumerate() {
        let status = match step.state {
            FirstRunStepState::Complete => "Done",
            FirstRunStepState::Current => "Current",
            FirstRunStepState::Upcoming => "Next",
        };
        let mut content = column![
            row![
                text(format!("{}. {}", index + 1, step.title)).font(UI_MEDIUM),
                space::horizontal(),
                text(status).size(UI_META_SIZE).style(text::secondary),
            ]
            .align_y(Center),
        ];
        if step.state == FirstRunStepState::Current {
            content = content.push(
                text(step.description)
                    .size(UI_META_SIZE)
                    .style(text::secondary),
            );
        }
        let is_current = step.state == FirstRunStepState::Current;
        checklist = checklist.push(
            container(content.spacing(4))
                .width(Fill)
                .padding([7, 9])
                .style(move |theme| {
                    if is_current {
                        selected_context_style(theme)
                    } else {
                        container::Style::default()
                    }
                }),
        );
    }
    checklist.into()
}

fn provider_recovery_message(provider: &ProviderHealth) -> String {
    if let Some(error) = provider.session_start_error() {
        let error = bounded_redacted_diagnostic(&error);
        let fallback = if provider.kind == ProviderKind::Shell {
            ""
        } else {
            " Shell remains available now."
        };
        return format!("{error} Check again after resolving it.{fallback}");
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
    palette: TerminalPalette,
    terminal_font: Font,
    cursor_style: DesktopTerminalCursor,
) -> Vec<text::Span<'static, (), Font>> {
    let default_background = theme::color(palette.background);

    runs.into_iter()
        .map(|run| {
            let (foreground, background) = palette.cell_colors(
                run.style.foreground,
                run.style.background,
                run.style.inverse,
            );
            let mut foreground = theme::color(foreground);
            let mut background = theme::color(background);
            if run.style.dim {
                foreground = foreground.scale_alpha(0.62);
            }
            if run.style.selected {
                foreground = theme::color(palette.selection_foreground);
                background = theme::color(palette.selection_background);
            }
            if run.style.cursor {
                let (cursor_foreground, cursor_background) =
                    terminal_cursor_colors(palette, cursor_style);
                foreground = cursor_foreground;
                if let Some(cursor_background) = cursor_background {
                    background = cursor_background;
                }
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

fn terminal_cursor_colors(
    palette: TerminalPalette,
    cursor_style: DesktopTerminalCursor,
) -> (Color, Option<Color>) {
    match cursor_style {
        DesktopTerminalCursor::Block => {
            let cursor = theme::color(palette.cursor);
            (cursor, Some(cursor))
        }
        DesktopTerminalCursor::Line => (theme::color(palette.cursor), None),
    }
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

fn open_terminal_action(density: DensityMetrics) -> Element<'static, Message> {
    container(
        column![
            text("Session ready. Open its terminal to begin.").style(text::secondary),
            button(centered_button_label(
                "Open terminal",
                UI_TEXT_SIZE,
                UI_MEDIUM
            ))
            .on_press(Message::Attach)
            .height(density.control_height)
            .style(primary_action_style),
        ]
        .spacing(12)
        .align_x(Center),
    )
    .width(Fill)
    .height(Fill)
    .center(Fill)
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
            button(centered_button_label(label, UI_META_SIZE, UI_MEDIUM))
                .on_press(action)
                .style(chrome_action_style),
        ]
        .spacing(8),
    )
    .padding(8)
    .width(Fill)
    .into()
}

fn empty_hint(message: &str) -> Element<'_, Message> {
    container(text(message).size(UI_META_SIZE).style(text::secondary))
        .padding(8)
        .width(Fill)
        .into()
}

fn centered_button_label(label: impl Into<String>, size: f32, font: Font) -> widget::Text<'static> {
    text(label.into())
        .font(font)
        .size(size)
        .height(Fill)
        .align_x(Center)
        .align_y(Vertical::Center)
}

fn application_version_text() -> String {
    format_application_version(
        option_env!("PREVIEW_TAG"),
        option_env!("RELEASE_TAG"),
        env!("CARGO_PKG_VERSION"),
    )
}

fn format_application_version(
    preview_tag: Option<&str>,
    release_tag: Option<&str>,
    package_version: &str,
) -> String {
    let version = preview_tag.or(release_tag).unwrap_or(package_version);
    if version.starts_with('v') {
        format!("SylvOps {version}")
    } else {
        format!("SylvOps v{version}")
    }
}

const fn main_tab_label(tab: MainTab) -> &'static str {
    match tab {
        MainTab::Terminal => "Terminal",
        MainTab::Changes => "Changes",
        MainTab::Details => "Details",
    }
}

fn footer_item(label: &str) -> Element<'static, Message> {
    text(label.to_owned())
        .font(UI_FONT)
        .size(FOOTER_TEXT_SIZE)
        .style(text::secondary)
        .wrapping(text::Wrapping::None)
        .into()
}

fn footer_context(label: &'static str, value: &str) -> Element<'static, Message> {
    text(format!("{label}: {value}"))
        .font(UI_MEDIUM)
        .size(FOOTER_TEXT_SIZE)
        .wrapping(text::Wrapping::None)
        .into()
}

fn footer_connection(connection: ConnectionState) -> Element<'static, Message> {
    let (icon, label) = match connection {
        ConnectionState::Connecting => (LineIcon::Working, "Connecting"),
        ConnectionState::Connected => (LineIcon::Finished, "Connected"),
        ConnectionState::Disconnected => (LineIcon::Disconnected, "Offline"),
    };
    container(
        row![
            line_icon(icon, 12),
            text(label)
                .font(UI_MEDIUM)
                .size(FOOTER_TEXT_SIZE)
                .wrapping(text::Wrapping::None),
        ]
        .spacing(4)
        .align_y(Center),
    )
    .padding([3, 7])
    .style(move |theme| footer_connection_style(theme, connection))
    .into()
}

fn footer_separator() -> Element<'static, Message> {
    container(rule::vertical(1)).height(16).into()
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

fn terminal_surface(
    theme: &Theme,
    focused: bool,
    terminal_palette: TerminalPalette,
) -> container::Style {
    let palette = theme.extended_palette();
    container::Style {
        background: Some(Background::Color(theme::color(terminal_palette.background))),
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

fn button_intent_style(
    presentation: PresentationTheme,
    intent: ButtonIntent,
    status: Status,
) -> button::Style {
    button_visual_style(presentation.button(intent, control_state(status)), status)
}

fn iced_button_intent_style(theme: &Theme, intent: ButtonIntent, status: Status) -> button::Style {
    button_visual_style(
        button_visual(iced_button_tokens(theme), intent, control_state(status)),
        status,
    )
}

const fn control_state(status: Status) -> ControlState {
    match status {
        Status::Active => ControlState::Rest,
        Status::Hovered => ControlState::Hovered,
        Status::Pressed => ControlState::Pressed,
        Status::Disabled => ControlState::Disabled,
    }
}

fn iced_button_tokens(theme: &Theme) -> ButtonTokens {
    let palette = theme.extended_palette();
    ButtonTokens {
        canvas: theme::rgb(palette.background.base.color),
        surface: theme::rgb(palette.background.weakest.color),
        surface_raised: theme::rgb(palette.background.weaker.color),
        surface_sunken: theme::rgb(palette.background.weak.color),
        border: theme::rgb(palette.background.strong.color),
        border_strong: theme::rgb(palette.background.stronger.color),
        text: theme::rgb(palette.background.base.text),
        text_muted: theme::rgb(palette.secondary.base.color),
        interaction: theme::rgb(palette.primary.base.color),
        interaction_text: theme::rgb(palette.primary.base.text),
        danger: theme::rgb(palette.danger.base.color),
        danger_surface: theme::rgb(palette.danger.weak.color),
    }
}

fn button_visual_style(visual: presentation::ButtonVisual, status: Status) -> button::Style {
    button::Style {
        background: Some(Background::Color(theme::color(visual.background))),
        text_color: theme::color(visual.foreground),
        border: Border {
            color: theme::color(visual.border),
            width: if matches!(status, Status::Hovered | Status::Pressed) {
                2.0
            } else {
                1.0
            },
            radius: 6.0.into(),
        },
        ..button::Style::default()
    }
}

fn chrome_action_style(theme: &Theme, status: Status) -> button::Style {
    iced_button_intent_style(theme, ButtonIntent::Secondary, status)
}

fn borderless_icon_style(theme: &Theme, status: Status) -> button::Style {
    let mut style = iced_button_intent_style(theme, ButtonIntent::Quiet, status);
    style.border.width = 0.0;
    if matches!(status, Status::Active | Status::Disabled) {
        style.background = None;
    }
    style
}

fn primary_action_style(theme: &Theme, status: Status) -> button::Style {
    iced_button_intent_style(theme, ButtonIntent::Primary, status)
}

fn danger_action_style(theme: &Theme, status: Status) -> button::Style {
    iced_button_intent_style(theme, ButtonIntent::Danger, status)
}

fn focused_danger_action_style(theme: &Theme, status: Status) -> button::Style {
    let mut style = danger_action_style(theme, status);
    style.border.width = 2.0;
    style.border.color = theme.extended_palette().danger.strong.color;
    style
}

fn flat_danger_style(theme: &Theme, status: Status) -> button::Style {
    danger_action_style(theme, status)
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
        palette.background.weak
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

fn navigator_row_container_style(theme: &Theme, selected: bool, hovered: bool) -> container::Style {
    list_item_container_style(theme, selected, hovered)
}

fn navigator_badge_style(theme: &Theme) -> container::Style {
    let palette = theme.extended_palette();
    container::Style {
        background: Some(Background::Color(palette.background.weak.color)),
        text_color: Some(palette.background.weak.text),
        border: Border {
            width: 1.0,
            radius: 4.0.into(),
            color: palette.background.strong.color,
        },
        ..container::Style::default()
    }
}

fn footer_connection_style(theme: &Theme, connection: ConnectionState) -> container::Style {
    let palette = theme.extended_palette();
    let (pair, border) = match connection {
        ConnectionState::Connecting => (palette.warning.weak, palette.warning.strong.color),
        ConnectionState::Connected => (palette.success.weak, palette.success.strong.color),
        ConnectionState::Disconnected => (palette.danger.weak, palette.danger.strong.color),
    };
    container::Style {
        background: Some(Background::Color(pair.color)),
        text_color: Some(contrast_safe_text(pair.color, pair.text)),
        border: Border {
            width: 1.0,
            radius: 5.0.into(),
            color: border,
        },
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

fn theme_preview_style(
    theme: &Theme,
    status: Status,
    selected: bool,
    focused: bool,
) -> button::Style {
    let mut style = content_tab_style(theme, status, selected);
    if focused {
        style.border.width = 2.0;
        style.border.color = theme.extended_palette().primary.strong.color;
    }
    style
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

fn setting_row<'a>(
    label: &'a str,
    control: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    row![
        text(label).font(UI_MEDIUM).width(Length::Fixed(150.0)),
        control.into(),
    ]
    .spacing(12)
    .align_y(Center)
    .into()
}

fn theme_gallery_index(theme: DesktopTheme) -> usize {
    DesktopTheme::ALL
        .iter()
        .position(|choice| *choice == theme)
        .unwrap_or_default()
}

const fn theme_gallery_len(classic_themes_expanded: bool) -> usize {
    if classic_themes_expanded {
        DesktopTheme::ALL.len()
    } else {
        FEATURED_THEME_COUNT
    }
}

fn theme_gallery_choice(index: usize, classic_themes_expanded: bool) -> Option<DesktopTheme> {
    (index < theme_gallery_len(classic_themes_expanded))
        .then(|| DesktopTheme::ALL.get(index).copied())
        .flatten()
}

fn theme_swatch(color: presentation::Rgb) -> Element<'static, Message> {
    let color = theme::color(color);
    container(space::horizontal())
        .width(Fill)
        .height(18)
        .style(move |_theme| container::Style {
            background: Some(Background::Color(color)),
            border: Border {
                width: 1.0,
                radius: 3.0.into(),
                color,
            },
            ..container::Style::default()
        })
        .into()
}

fn detail(label: &str, value: String) -> Element<'_, Message> {
    row![
        text(label)
            .width(Length::Fixed(150.0))
            .style(text::secondary),
        text(value)
            .width(Fill)
            .wrapping(text::Wrapping::WordOrGlyph)
    ]
    .width(Fill)
    .align_y(Vertical::Center)
    .into()
}

fn technical_detail(label: &'static str, value: String) -> Element<'static, Message> {
    let copy_value = value.clone();
    row![
        text(label)
            .width(Length::Fixed(170.0))
            .style(text::secondary),
        text(value)
            .width(Fill)
            .wrapping(text::Wrapping::WordOrGlyph),
        button("Copy")
            .on_press(Message::CopyTechnicalValue(copy_value))
            .style(chrome_action_style),
    ]
    .spacing(10)
    .align_y(Vertical::Center)
    .into()
}

fn session_tab_label(name: &str) -> String {
    name.to_owned()
}

fn cancel_inline_navigator_rename(rename: &mut Option<InlineNavigatorRename>) {
    if rename.as_ref().is_some_and(|rename| !rename.pending) {
        *rename = None;
    }
}

fn retain_inline_navigator_rename(
    rename: &mut Option<InlineNavigatorRename>,
    snapshot: &DaemonSnapshot,
) {
    let Some(target) = rename.as_ref().map(|rename| rename.target) else {
        return;
    };
    let exists = match target {
        NavigatorNodeId::Repository(id) => snapshot.projects.iter().any(|item| item.id == id),
        NavigatorNodeId::Checkout(id) => snapshot.worktrees.iter().any(|item| item.id == id),
        NavigatorNodeId::Session(id) => snapshot.sessions.iter().any(|item| item.id == id),
    };
    if !exists {
        *rename = None;
    }
}

const fn active_terminal_action_label(
    action: ActiveTerminalAction,
    _compact: bool,
) -> &'static str {
    match action {
        ActiveTerminalAction::Open => "Open terminal",
        ActiveTerminalAction::Leave => "Leave terminal",
        ActiveTerminalAction::ViewOutput => "View output",
    }
}

fn is_open_terminal_shortcut(value: &str) -> bool {
    value.eq_ignore_ascii_case("o") || value.eq_ignore_ascii_case("a")
}

fn is_open_settings_shortcut(key: &Key, modifiers: keyboard::Modifiers) -> bool {
    modifiers.control() && matches!(key.as_ref(), Key::Character(value) if value == ",")
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::advanced::{
        Layout,
        layout::{Limits, Node},
        renderer::{Headless, Renderer as _},
        widget::{Operation as WidgetOperation, Tree},
    };
    use sylvops_core::provider::{AuthenticationRequirement, ProviderCapabilities};

    fn session(state: SessionState, external_session_id: Option<&str>) -> Session {
        Session {
            id: SessionId::new(),
            worktree_id: WorktreeId::new(),
            provider_profile_id: None,
            provider_kind: ProviderKind::Codex,
            display_name: "Codex".into(),
            state,
            process_id: None,
            external_session_id: external_session_id.map(str::to_owned),
            command: "codex".into(),
            arguments_json: "[]".into(),
            cwd: "C:/repo".into(),
            created_at: 1,
            started_at: Some(1),
            ended_at: Some(2),
            last_activity_at: 2,
            last_seen_output_sequence: 0,
            exit_code: Some(0),
            failure_reason: None,
        }
    }

    fn provider_health(kind: ProviderKind, available: bool, authenticated: bool) -> ProviderHealth {
        ProviderHealth {
            kind,
            available,
            authenticated,
            executable_path: None,
            version: None,
            diagnostic: (!available).then(|| format!("{kind} is unavailable")),
            capabilities: ProviderCapabilities {
                interactive: true,
                resume: kind != ProviderKind::Shell,
                authentication: if kind == ProviderKind::Shell {
                    AuthenticationRequirement::None
                } else {
                    AuthenticationRequirement::ExistingLogin
                },
                ..ProviderCapabilities::default()
            },
            checked_at: 0,
        }
    }

    fn desktop_hierarchy() -> (DesktopApp, ProjectId, WorktreeId, SessionId, SessionId) {
        let runtime_root = std::env::temp_dir().join(format!(
            "sylvops-desktop-explorer-test-{}",
            SessionId::new()
        ));
        let paths = RuntimePaths::discover(Some(&runtime_root)).expect("runtime paths");
        let mut app = DesktopApp::new(paths);
        let workspace_id = WorkspaceId::new();
        let project_id = ProjectId::new();
        let worktree_id = WorktreeId::new();
        let mut first = session(SessionState::Running, None);
        first.worktree_id = worktree_id;
        first.display_name = "First".into();
        let mut second = session(SessionState::NeedsFeedback, None);
        second.worktree_id = worktree_id;
        second.display_name = "Second".into();
        app.snapshot.workspaces.push(Workspace {
            id: workspace_id,
            name: "Local".into(),
            created_at: 1,
            updated_at: 1,
            last_opened_at: Some(1),
            is_open: true,
        });
        app.snapshot.projects.push(Project {
            id: project_id,
            workspace_id,
            name: "SylvOps".into(),
            repository_path: "C:/repo".into(),
            canonical_repository_path: "C:/repo".into(),
            default_branch: Some("main".into()),
            remote_url: None,
            created_at: 1,
            last_activity_at: 1,
        });
        app.snapshot.worktrees.push(Worktree {
            id: worktree_id,
            project_id,
            name: "Primary".into(),
            path: "C:/repo".into(),
            canonical_path: "C:/repo".into(),
            branch: Some("main".into()),
            base_ref: "main".into(),
            base_commit: "abc".into(),
            is_root_checkout: false,
            status: WorktreeStatus::Active,
            created_at: 1,
            last_activity_at: 1,
            removed_at: None,
        });
        let first_id = first.id;
        let second_id = second.id;
        app.snapshot.sessions.extend([first, second]);
        app.selected_project_id = Some(project_id);
        app.selected_worktree_id = Some(worktree_id);
        app.selected_session_id = Some(first_id);
        app.active_session_id = Some(first_id);
        app.open_sessions.push(first_id);
        app.keyboard_panel = DesktopPanel::Sessions;
        app.desktop_state.compact_panel = DesktopPanel::Sessions;
        app.connection = ConnectionState::Connected;
        app.desktop_state.theme = DesktopTheme::Canopy;
        (app, project_id, worktree_id, first_id, second_id)
    }

    #[derive(Debug, Eq, PartialEq)]
    struct RenderedBaseline {
        width: u16,
        height: u16,
        layout_nodes: usize,
        distinct_colors: usize,
        checksum: u64,
    }

    #[cfg(windows)]
    const APPROVED_VISUAL_CHECKSUMS: [u64; 13] = [
        426_262_926_778_126_858,
        1_503_165_031_025_874_429,
        5_686_679_588_026_358_842,
        14_761_951_164_573_712_012,
        7_129_090_979_179_955_617,
        13_544_107_981_426_698_952,
        1_748_718_398_488_552_422,
        5_448_511_970_848_490_064,
        5_019_121_441_122_480_153,
        3_931_679_346_670_316_763,
        17_325_232_898_887_675_372,
        282_116_067_056_645_007,
        5_196_752_353_597_121_531,
    ];
    #[cfg(target_os = "macos")]
    const APPROVED_VISUAL_CHECKSUMS: [u64; 13] = [
        426_262_926_778_126_858,
        1_503_165_031_025_874_429,
        5_686_679_588_026_358_842,
        14_761_951_164_573_712_012,
        7_129_090_979_179_955_617,
        13_544_107_981_426_698_952,
        1_748_718_398_488_552_422,
        5_448_511_970_848_490_064,
        5_019_121_441_122_480_153,
        3_931_679_346_670_316_763,
        17_325_232_898_887_675_372,
        282_116_067_056_645_007,
        5_196_752_353_597_121_531,
    ];
    #[cfg(all(not(windows), not(target_os = "macos")))]
    const APPROVED_VISUAL_CHECKSUMS: [u64; 13] = [
        426_262_926_778_126_858,
        1_503_165_031_025_874_429,
        5_686_679_588_026_358_842,
        14_761_951_164_573_712_012,
        7_129_090_979_179_955_617,
        13_544_107_981_426_698_952,
        1_748_718_398_488_552_422,
        5_448_511_970_848_490_064,
        5_019_121_441_122_480_153,
        3_931_679_346_670_316_763,
        17_325_232_898_887_675_372,
        282_116_067_056_645_007,
        5_196_752_353_597_121_531,
    ];
    const APPROVED_DETAILS_VISUAL_CHECKSUMS: [u64; 2] =
        [7_826_771_872_323_314_152, 16_142_488_885_153_956_544];

    async fn render_desktop_baseline(
        app: &DesktopApp,
        width: u16,
        height: u16,
    ) -> RenderedBaseline {
        let theme = app.theme();
        render_element_baseline(app.view(), &theme, width, height).await
    }

    async fn layout_test_element(
        mut element: Element<'_, Message>,
        limits: Limits,
    ) -> (Element<'_, Message>, iced::Renderer, Tree, Node) {
        load_bundled_ui_fonts_for_rendering();
        let renderer = <iced::Renderer as Headless>::new(UI_FONT, 16.0.into(), Some("tiny-skia"))
            .await
            .expect("tiny-skia headless renderer");
        assert_eq!(renderer.name(), "tiny-skia");
        let mut tree = Tree::new(&element);
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        (element, renderer, tree, node)
    }

    async fn render_element_baseline(
        element: Element<'_, Message>,
        theme: &Theme,
        width: u16,
        height: u16,
    ) -> RenderedBaseline {
        let size = iced::Size::new(f32::from(width), f32::from(height));
        let limits = Limits::new(size, size);
        let (element, mut renderer, tree, node) = layout_test_element(element, limits).await;
        assert_eq!(node.size(), size);
        let viewport = iced::Rectangle::new(iced::Point::ORIGIN, size);
        renderer.reset(viewport);
        element.as_widget().draw(
            &tree,
            &mut renderer,
            theme,
            &iced::advanced::renderer::Style::default(),
            Layout::new(&node),
            iced::mouse::Cursor::Unavailable,
            &viewport,
        );
        let pixels = renderer.screenshot(
            iced::Size::new(u32::from(width), u32::from(height)),
            1.0,
            theme.palette().background,
        );
        assert_eq!(pixels.len(), usize::from(width) * usize::from(height) * 4);
        let distinct_colors = pixels
            .as_chunks::<4>()
            .0
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>()
            .len();
        let checksum = pixels.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
        });
        RenderedBaseline {
            width,
            height,
            layout_nodes: layout_node_count(&node),
            distinct_colors,
            checksum,
        }
    }

    async fn render_session_action_baselines() -> [RenderedBaseline; 4] {
        let (mut app, _, worktree_id, _, _) = desktop_hierarchy();
        app.desktop_state.window_width = 680;
        app.desktop_state.window_height = 480;
        app.narrow_main = true;
        let narrow_terminal_action = render_desktop_baseline(&app, 680, 480).await;

        app.modal = Some(Modal::Form(FormModal::new(Form::session(
            worktree_id,
            vec![provider_health(ProviderKind::Shell, true, true)],
        ))));
        app.desktop_state.window_width = 1_440;
        app.desktop_state.window_height = 900;
        let wide_session_form = render_desktop_baseline(&app, 1_440, 900).await;
        app.desktop_state.window_width = 900;
        app.desktop_state.window_height = 700;
        let compact_session_form = render_desktop_baseline(&app, 900, 700).await;
        app.desktop_state.window_width = 680;
        app.desktop_state.window_height = 480;
        let narrow_session_form = render_desktop_baseline(&app, 680, 480).await;

        [
            narrow_terminal_action,
            wide_session_form,
            compact_session_form,
            narrow_session_form,
        ]
    }

    async fn render_navigator_edge_baselines() -> [RenderedBaseline; 2] {
        let (mut app, _, worktree_id, _, _) = desktop_hierarchy();
        app.desktop_state.window_width = 680;
        app.desktop_state.window_height = 480;
        app.snapshot.projects[0].name = "Repository name that remains clipped inside the navigator panel instead of changing row geometry".into();
        app.snapshot.worktrees[0].name =
            "Managed checkout with an intentionally long descriptive label".into();
        app.snapshot.sessions[0].display_name =
            "Session with a long label that cannot grow the row".into();
        app.narrow_main = false;
        let long_labels = render_desktop_baseline(&app, 680, 480).await;

        let overflow_actions = [NavigatorAction::DeleteCheckout(worktree_id); 4];
        let theme = app.theme();
        let overflow_height = u16::try_from(app.presentation().density.control_height + 8)
            .expect("bounded action strip height");
        let overflowing_actions = render_element_baseline(
            navigator_action_strip(&overflow_actions, app.presentation().density),
            &theme,
            90,
            overflow_height,
        )
        .await;

        [long_labels, overflowing_actions]
    }

    fn layout_node_count(node: &Node) -> usize {
        1 + node.children().iter().map(layout_node_count).sum::<usize>()
    }

    async fn layout_element(element: Element<'_, Message>, width: f32, height: f32) -> Node {
        let limits = Limits::new(iced::Size::ZERO, iced::Size::new(width, height));
        let (_, _, _, node) = layout_test_element(element, limits).await;
        node
    }

    fn layout_origins(node: &Node, origins: &mut Vec<Point>) {
        origins.push(node.bounds().position());
        for child in node.children() {
            layout_origins(child, origins);
        }
    }

    fn max_layout_width(node: &Node) -> f32 {
        node.children()
            .iter()
            .map(max_layout_width)
            .fold(node.bounds().width, f32::max)
    }

    struct ScrollableSnapshot {
        target: widget::Id,
        viewport: Option<(iced::Rectangle, iced::Rectangle, iced::Vector)>,
    }

    impl WidgetOperation for ScrollableSnapshot {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn WidgetOperation)) {
            operate(self);
        }

        fn scrollable(
            &mut self,
            id: Option<&widget::Id>,
            bounds: iced::Rectangle,
            content_bounds: iced::Rectangle,
            translation: iced::Vector,
            _state: &mut dyn iced::advanced::widget::operation::Scrollable,
        ) {
            if id == Some(&self.target) {
                self.viewport = Some((bounds, content_bounds, translation));
            }
        }
    }

    struct ButtonBoundsByLabel<'a> {
        label: &'a str,
        last_container: Option<iced::Rectangle>,
        bounds: Option<iced::Rectangle>,
    }

    impl WidgetOperation for ButtonBoundsByLabel<'_> {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn WidgetOperation)) {
            operate(self);
        }

        fn container(&mut self, _id: Option<&widget::Id>, bounds: iced::Rectangle) {
            self.last_container = Some(bounds);
        }

        fn text(&mut self, _id: Option<&widget::Id>, _bounds: iced::Rectangle, text: &str) {
            if text == self.label {
                self.bounds = self.last_container;
            }
        }
    }

    async fn button_bounds_by_label(
        element: Element<'_, Message>,
        label: &str,
        size: iced::Size,
    ) -> iced::Rectangle {
        let (mut element, renderer, mut tree, node) =
            layout_test_element(element, Limits::new(size, size)).await;
        let mut operation = ButtonBoundsByLabel {
            label,
            last_container: None,
            bounds: None,
        };
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut operation);
        operation.bounds.expect("button label in rendered layout")
    }

    #[tokio::test]
    async fn session_action_buttons_use_the_density_control_height() {
        let (mut app, _, worktree_id, _, _) = desktop_hierarchy();
        let providers = vec![
            provider_health(ProviderKind::Shell, true, true),
            provider_health(ProviderKind::Codex, true, true),
        ];

        for density_choice in [DesktopDensity::Comfortable, DesktopDensity::Compact] {
            app.desktop_state.density = density_choice;
            let expected_height = f32::from(
                u16::try_from(app.presentation().density.control_height)
                    .expect("bounded density control height"),
            );
            for (provider, label) in [
                (ProviderKind::Shell, "Start shell"),
                (ProviderKind::Codex, "Start Codex"),
            ] {
                let mut modal = FormModal::new(Form::session(worktree_id, providers.clone()));
                assert!(modal.form.select_provider(provider));
                app.modal = Some(Modal::Form(modal));
                let bounds =
                    button_bounds_by_label(app.view(), label, iced::Size::new(900.0, 700.0)).await;
                assert!(
                    (bounds.height - expected_height).abs() <= f32::EPSILON,
                    "{density_choice:?} {label} height was {}",
                    bounds.height
                );
            }

            let mut pending = FormModal::new(Form::session(worktree_id, providers.clone()));
            pending.pending = true;
            app.modal = Some(Modal::Form(pending));
            let pending_bounds =
                button_bounds_by_label(app.view(), "Working…", iced::Size::new(900.0, 700.0)).await;
            assert!((pending_bounds.height - expected_height).abs() <= f32::EPSILON);

            let mut checking = FormModal::new(Form::session(worktree_id, providers.clone()));
            checking.provider_probe = Some(ProviderKind::Shell);
            app.modal = Some(Modal::Form(checking));
            let checking_bounds = button_bounds_by_label(
                app.view(),
                "Checking provider…",
                iced::Size::new(900.0, 700.0),
            )
            .await;
            assert!((checking_bounds.height - expected_height).abs() <= f32::EPSILON);

            let bounds = button_bounds_by_label(
                app.terminal_view(),
                "Open terminal",
                iced::Size::new(600.0, 400.0),
            )
            .await;
            assert!(
                (bounds.height - expected_height).abs() <= f32::EPSILON,
                "{density_choice:?} Open terminal height was {}",
                bounds.height
            );
        }
    }

    #[tokio::test]
    async fn selected_and_unselected_navigator_rows_keep_identical_geometry() {
        let (mut app, _, _, _, _) = desktop_hierarchy();
        for density_choice in [DesktopDensity::Comfortable, DesktopDensity::Compact] {
            app.desktop_state.density = density_choice;
            let presentation = app.presentation();
            let expected_height = f32::from(
                u16::try_from(presentation.density.control_height + 14)
                    .expect("bounded navigator row height"),
            );
            let rows = presentation
                .navigator
                .repositories
                .into_iter()
                .chain(presentation.navigator.checkouts)
                .chain(presentation.navigator.sessions);
            for mut row in rows {
                row.selected = false;
                let unselected = layout_element(app.navigator_row(&row), 225.0, 100.0).await;
                row.selected = true;
                let selected = layout_element(app.navigator_row(&row), 225.0, 100.0).await;

                assert_eq!(selected.bounds(), unselected.bounds(), "{:?}", row.kind);
                assert!(
                    (selected.bounds().height - expected_height).abs() <= f32::EPSILON,
                    "{:?}",
                    row.kind
                );
                let mut selected_origins = Vec::new();
                let mut unselected_origins = Vec::new();
                layout_origins(&selected, &mut selected_origins);
                layout_origins(&unselected, &mut unselected_origins);
                assert_eq!(
                    selected_origins, unselected_origins,
                    "selection must not move {:?} title/detail coordinates or insert row content",
                    row.kind
                );
            }
        }
    }

    #[tokio::test]
    async fn navigator_action_strip_keeps_the_tree_origin_fixed() {
        let (mut app, _, worktree_id, _, _) = desktop_hierarchy();
        let with_action = layout_element(app.checkouts_column(), 225.0, 220.0).await;

        app.snapshot
            .worktrees
            .iter_mut()
            .find(|worktree| worktree.id == worktree_id)
            .expect("managed checkout")
            .is_root_checkout = true;
        let without_action = layout_element(app.checkouts_column(), 225.0, 220.0).await;

        let with_action_panel = &with_action.children()[0];
        let without_action_panel = &without_action.children()[0];
        assert_eq!(
            with_action_panel.children().len(),
            3,
            "navigator panels must place the heading, reserved action strip, and tree in separate rows"
        );
        assert_eq!(without_action_panel.children().len(), 3);
        let action_strip = &with_action_panel.children()[1];
        let empty_action_strip = &without_action_panel.children()[1];
        assert_eq!(action_strip.bounds(), empty_action_strip.bounds());
        let expected_strip_height = f32::from(
            u16::try_from(app.presentation().density.control_height)
                .expect("bounded control height"),
        ) + 8.0;
        assert!((action_strip.bounds().height - expected_strip_height).abs() <= f32::EPSILON);
        assert!(layout_node_count(action_strip) > layout_node_count(empty_action_strip));
        assert_eq!(
            with_action_panel.children()[2].bounds().position(),
            without_action_panel.children()[2].bounds().position(),
            "changing strip contents must not move the tree origin"
        );

        let overflow_actions = [NavigatorAction::DeleteCheckout(worktree_id); 4];
        let overflow_strip = layout_element(
            navigator_action_strip(&overflow_actions, app.presentation().density),
            90.0,
            100.0,
        )
        .await;
        assert!((overflow_strip.bounds().width - 90.0).abs() <= f32::EPSILON);
        assert!(
            (overflow_strip.bounds().height - action_strip.bounds().height).abs() <= f32::EPSILON
        );
        assert!(max_layout_width(&overflow_strip) > overflow_strip.bounds().width);
    }

    #[test]
    fn selecting_the_bottom_row_requests_a_complete_reveal() {
        let (mut app, _, _, _, _) = desktop_hierarchy();
        let template = app.snapshot.worktrees[0].clone();
        app.snapshot.worktrees.clear();
        let mut ids = Vec::new();
        for index in 0..8 {
            let mut worktree = template.clone();
            worktree.id = WorktreeId::new();
            worktree.name = format!("Checkout {index}");
            worktree.last_activity_at = i64::from(8 - index);
            ids.push(worktree.id);
            app.snapshot.worktrees.push(worktree);
        }
        app.selected_worktree_id = ids.first().copied();

        app.select_worktree(*ids.last().expect("bottom checkout"));

        assert_eq!(app.pending_navigator_reveal, Some(DesktopPanel::Worktrees));
        assert!((navigator_reveal_offset(7, 8) - 1.0).abs() <= f32::EPSILON);
        assert!(navigator_reveal_offset(0, 8).abs() <= f32::EPSILON);

        app.pending_navigator_reveal = None;
        app.select_worktree(*ids.last().expect("bottom checkout"));
        assert_eq!(app.pending_navigator_reveal, Some(DesktopPanel::Worktrees));
    }

    #[tokio::test]
    async fn bottom_row_reveal_reaches_the_end_of_a_constrained_tree_viewport() {
        let (mut app, _, _, _, _) = desktop_hierarchy();
        let template = app.snapshot.worktrees[0].clone();
        app.snapshot.worktrees.clear();
        for index in 0..8 {
            let mut worktree = template.clone();
            worktree.id = WorktreeId::new();
            worktree.name = format!("Checkout {index}");
            worktree.last_activity_at = i64::from(8 - index);
            app.selected_worktree_id = Some(worktree.id);
            app.snapshot.worktrees.push(worktree);
        }

        let size = iced::Size::new(225.0, 220.0);
        let (mut element, renderer, mut tree, node) =
            layout_test_element(app.checkouts_column(), Limits::new(size, size)).await;
        let scroll_id = navigator_scroll_id(DesktopPanel::Worktrees);
        let mut reveal = iced::advanced::widget::operation::scrollable::snap_to::<()>(
            scroll_id.clone(),
            iced::advanced::widget::operation::scrollable::RelativeOffset {
                x: None,
                y: Some(1.0),
            },
        );
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut reveal);
        let mut snapshot = ScrollableSnapshot {
            target: scroll_id,
            viewport: None,
        };
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut snapshot);
        let (bounds, content_bounds, translation) =
            snapshot.viewport.expect("checkout tree viewport");

        assert!(content_bounds.height > bounds.height);
        assert!(translation.y > 0.0);
        let revealed_bottom = content_bounds.y + content_bounds.height - translation.y;
        assert!((revealed_bottom - (bounds.y + bounds.height)).abs() <= f32::EPSILON);
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
    fn single_clicking_a_session_selects_and_opens_it() {
        let (mut app, _, _, first_id, second_id) = desktop_hierarchy();

        app.select_session(second_id);
        assert_eq!(app.selected_session_id, Some(second_id));
        assert_eq!(app.active_session_id, Some(second_id));
        assert!(app.open_sessions.contains(&first_id));
        assert!(app.open_sessions.contains(&second_id));

        app.select_main_tab(MainTab::Changes);
        assert_eq!(app.active_session_id, Some(second_id));
    }

    #[test]
    fn navigator_context_actions_are_available_from_keyboard_selection() {
        let (mut app, project_id, worktree_id, _, second_id) = desktop_hierarchy();

        app.keyboard_panel = DesktopPanel::Worktrees;
        app.shortcut_new();
        assert!(matches!(
            app.modal,
            Some(Modal::Form(FormModal {
                form: Form {
                    kind: FormKind::CreateWorktree(id),
                    ..
                },
                ..
            })) if id == project_id
        ));

        app.modal = None;
        app.select_worktree(worktree_id);
        app.keyboard_panel = DesktopPanel::Worktrees;
        app.shortcut_rename();
        assert_eq!(
            app.inline_navigator_rename
                .as_ref()
                .map(|rename| rename.target),
            Some(NavigatorNodeId::Checkout(worktree_id))
        );

        app.inline_navigator_rename = None;
        app.select_session(second_id);
        app.keyboard_panel = DesktopPanel::Sessions;
        app.shortcut_delete();
        assert!(matches!(
            app.modal,
            Some(Modal::Confirmation(Confirmation::StopSession { .. }))
        ));
    }

    #[test]
    fn every_navigator_level_supports_inline_rename() {
        let (mut app, project_id, worktree_id, session_id, _) = desktop_hierarchy();
        for target in [
            NavigatorNodeId::Repository(project_id),
            NavigatorNodeId::Checkout(worktree_id),
            NavigatorNodeId::Session(session_id),
        ] {
            let _ = app.begin_navigator_rename(target);
            assert_eq!(
                app.inline_navigator_rename
                    .as_ref()
                    .map(|rename| rename.target),
                Some(target)
            );
            app.inline_navigator_rename = None;
        }
    }

    #[tokio::test]
    async fn rendered_visual_acceptance_baselines_cover_polished_workflows() {
        let (mut app, _, _, _, _) = desktop_hierarchy();

        app.desktop_state.window_width = 1_440;
        app.desktop_state.window_height = 900;
        let canopy_attention = render_desktop_baseline(&app, 1_440, 900).await;

        app.desktop_state.window_width = 900;
        app.desktop_state.window_height = 700;
        let compact = render_desktop_baseline(&app, 900, 700).await;

        app.desktop_state.window_width = 680;
        app.desktop_state.window_height = 480;
        app.narrow_main = false;
        let narrow_explorer = render_desktop_baseline(&app, 680, 480).await;
        app.modal = Some(Modal::Settings);
        let narrow_settings = render_desktop_baseline(&app, 680, 480).await;

        let [
            narrow_terminal_action,
            wide_session_form,
            compact_session_form,
            narrow_session_form,
        ] = render_session_action_baselines().await;

        let [long_labels, overflowing_actions] = render_navigator_edge_baselines().await;

        app.desktop_state.window_width = 900;
        app.desktop_state.window_height = 700;
        app.disclosures.classic_themes = true;
        let theme_gallery = render_desktop_baseline(&app, 900, 700).await;

        let runtime_root = std::env::temp_dir().join(format!(
            "sylvops-desktop-first-run-baseline-{}",
            SessionId::new()
        ));
        let mut first_run = DesktopApp::new(
            RuntimePaths::discover(Some(&runtime_root)).expect("first-run runtime paths"),
        );
        first_run.connection = ConnectionState::Connected;
        first_run.desktop_state.theme = DesktopTheme::Grove;
        first_run.desktop_state.window_width = 1_440;
        first_run.desktop_state.window_height = 900;
        first_run.modal = Some(Modal::Form(FormModal::first_run(Form::workspace())));
        let grove_first_run = render_desktop_baseline(&first_run, 1_440, 900).await;

        app.modal = Some(Modal::Confirmation(Confirmation::StopSession {
            session_name: "First".into(),
            cwd: "C:/repo".into(),
        }));
        app.error = Some("The session could not be stopped. Check its current state.".into());
        let confirmation_error = render_desktop_baseline(&app, 900, 700).await;

        assert_eq!(
            [
                canopy_attention.layout_nodes,
                compact.layout_nodes,
                narrow_explorer.layout_nodes,
                narrow_settings.layout_nodes,
                narrow_terminal_action.layout_nodes,
                wide_session_form.layout_nodes,
                compact_session_form.layout_nodes,
                narrow_session_form.layout_nodes,
                long_labels.layout_nodes,
                overflowing_actions.layout_nodes,
                theme_gallery.layout_nodes,
                grove_first_run.layout_nodes,
                confirmation_error.layout_nodes,
            ],
            [171, 118, 84, 169, 81, 197, 144, 107, 84, 19, 225, 158, 138]
        );
        let baselines = [
            &canopy_attention,
            &compact,
            &narrow_explorer,
            &narrow_settings,
            &narrow_terminal_action,
            &wide_session_form,
            &compact_session_form,
            &narrow_session_form,
            &long_labels,
            &overflowing_actions,
            &theme_gallery,
            &grove_first_run,
            &confirmation_error,
        ];
        for baseline in baselines {
            assert!(baseline.distinct_colors >= 8, "{baseline:?}");
        }
        let unique_checksums = baselines
            .map(|baseline| baseline.checksum)
            .into_iter()
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(unique_checksums.len(), baselines.len());
        if std::env::var_os("CI").is_some() {
            assert_eq!(
                baselines.map(|baseline| baseline.checksum),
                APPROVED_VISUAL_CHECKSUMS
            );
        }
    }

    #[tokio::test]
    async fn rendered_details_cover_verbatim_paths_at_responsive_widths() {
        let (mut app, _, _, _, _) = desktop_hierarchy();
        let canonical = r"\\?\C:\Users\Mite\Projects\Пројект\a-very-long-checkout-name-that-must-wrap-inside-details";
        app.snapshot.projects[0].canonical_repository_path = canonical.into();
        app.snapshot.worktrees[0].canonical_path = canonical.into();
        app.snapshot.sessions[0].cwd = canonical.into();
        app.desktop_state.compact_panel = DesktopPanel::Sessions;
        app.keyboard_panel = DesktopPanel::Sessions;
        app.select_main_tab(MainTab::Details);

        let compact = render_desktop_baseline(&app, 900, 700).await;
        app.narrow_main = true;
        let narrow = render_desktop_baseline(&app, 680, 480).await;

        assert_eq!(
            [compact.layout_nodes, narrow.layout_nodes],
            [230, 230],
            "Details layout changed at a representative width"
        );
        assert!(compact.distinct_colors >= 8, "{compact:?}");
        assert!(narrow.distinct_colors >= 8, "{narrow:?}");
        assert_eq!(
            [compact.checksum, narrow.checksum],
            APPROVED_DETAILS_VISUAL_CHECKSUMS,
            "Details pixels changed at a representative width"
        );
        assert!(!human_readable_path(canonical).contains(r"\\?\"));
        assert!(!human_readable_path(canonical).contains('\u{fffd}'));
    }

    #[test]
    fn cursor_style_controls_cell_fill() {
        let palette =
            presentation::PresentationTheme::resolve(DesktopTheme::Canopy, SystemAppearance::Dark)
                .terminal;
        let (block_foreground, block_background) =
            terminal_cursor_colors(palette, DesktopTerminalCursor::Block);
        let (_, line_background) = terminal_cursor_colors(palette, DesktopTerminalCursor::Line);

        assert_eq!(block_foreground, theme::color(palette.cursor));
        assert_eq!(block_background, Some(theme::color(palette.cursor)));
        assert!(line_background.is_none());
    }

    #[test]
    fn desktop_window_uses_the_embedded_sylvops_icon() {
        let icon = desktop_window_settings().icon.expect("desktop window icon");
        let (rgba, size) = icon.into_raw();

        assert!(size.width > 0);
        assert!(size.height > 0);
        assert!(size.width <= MAX_WINDOW_ICON_SIDE);
        assert!(size.height <= MAX_WINDOW_ICON_SIDE);
        assert_eq!(rgba.len(), (size.width * size.height * 4) as usize);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn desktop_window_matches_the_linux_shell_identity() {
        assert_eq!(
            desktop_window_settings().platform_specific.application_id,
            sylvops_core::LINUX_DESKTOP_ID
        );
    }

    #[test]
    fn appearance_settings_expose_bounded_behavioral_choices() {
        assert_eq!(
            DesktopTheme::ALL[..FEATURED_THEME_COUNT],
            [
                DesktopTheme::System,
                DesktopTheme::Grove,
                DesktopTheme::Canopy,
                DesktopTheme::Midnight,
            ]
        );
        assert_eq!(DesktopTheme::ALL.len() - FEATURED_THEME_COUNT, 7);
        assert_eq!(DENSITY_CHOICES.len(), 2);
        assert_eq!(TERMINAL_FONT_CHOICES.len(), 4);
        assert_eq!(TERMINAL_CURSOR_CHOICES.len(), 2);
        assert_eq!(TERMINAL_FONT_SIZE_CHOICES[0], MIN_TERMINAL_FONT_SIZE);
        assert_eq!(
            TERMINAL_FONT_SIZE_CHOICES[TERMINAL_FONT_SIZE_CHOICES.len() - 1],
            MAX_TERMINAL_FONT_SIZE
        );
    }

    #[test]
    fn appearance_preview_cards_are_keyboard_navigable_with_visible_focus_state() {
        let runtime_root = std::env::temp_dir().join(format!(
            "sylvops-desktop-theme-keyboard-test-{}",
            SessionId::new()
        ));
        let paths = RuntimePaths::discover(Some(&runtime_root)).expect("runtime paths");
        let mut app = DesktopApp::new(paths);
        app.modal = Some(Modal::Settings);

        assert!(app.handle_transient_keyboard(
            &Key::Named(Named::ArrowRight),
            keyboard::Modifiers::default(),
        ));
        assert_eq!(app.disclosures.settings_theme_focus, 1);
        assert!(
            app.handle_transient_keyboard(
                &Key::Named(Named::Enter),
                keyboard::Modifiers::default(),
            )
        );
        assert_eq!(app.desktop_state.theme, DesktopTheme::Grove);

        let theme = app.theme();
        let focused = theme_preview_style(&theme, Status::Active, false, true);
        let unfocused = theme_preview_style(&theme, Status::Active, false, false);
        assert!(focused.border.width > unfocused.border.width);
        assert!(is_open_settings_shortcut(
            &Key::Character(",".into()),
            keyboard::Modifiers::CTRL,
        ));
    }

    #[test]
    fn destructive_confirmation_accepts_enter_and_escape_from_the_keyboard() {
        let (mut app, _, _, _, _) = desktop_hierarchy();
        app.open_stop_confirmation();
        assert!(matches!(app.modal, Some(Modal::Confirmation(_))));

        assert!(
            app.handle_transient_keyboard(
                &Key::Named(Named::Enter),
                keyboard::Modifiers::default(),
            )
        );
        assert!(app.modal.is_none());

        app.open_stop_confirmation();
        assert!(
            app.handle_transient_keyboard(
                &Key::Named(Named::Escape),
                keyboard::Modifiers::default(),
            )
        );
        assert!(app.modal.is_none());
    }

    #[test]
    fn desktop_claude_session_request_keeps_advanced_options_cli_only() {
        let worktree_id = WorktreeId::new();
        let mut form = Form::session(
            worktree_id,
            vec![
                provider_health(ProviderKind::Shell, true, true),
                provider_health(ProviderKind::Claude, true, true),
            ],
        );
        assert!(form.select_provider(ProviderKind::Claude));

        assert_eq!(
            form.session_request(100, 30),
            Some(ClientRequest::CreateSession {
                worktree_id,
                provider: ProviderKind::Claude,
                display_name: None,
                model: None,
                effort: None,
                initial_prompt: None,
                columns: 100,
                rows: 30,
            })
        );
    }

    #[test]
    fn session_provider_and_recovery_controls_have_keyboard_paths() {
        let runtime_root = std::env::temp_dir().join(format!(
            "sylvops-desktop-provider-keyboard-test-{}",
            SessionId::new()
        ));
        let paths = RuntimePaths::discover(Some(&runtime_root)).expect("runtime paths");
        let mut app = DesktopApp::new(paths);
        app.modal = Some(Modal::Form(FormModal::new(Form::session(
            WorktreeId::new(),
            vec![
                provider_health(ProviderKind::Shell, true, true),
                provider_health(ProviderKind::Codex, true, true),
                provider_health(ProviderKind::Claude, true, true),
            ],
        ))));

        assert!(
            app.handle_transient_keyboard(&Key::Character("l".into()), keyboard::Modifiers::ALT,)
        );
        assert!(matches!(
            app.modal,
            Some(Modal::Form(FormModal { ref form, .. }))
                if form.provider().is_some_and(|provider| provider.kind == ProviderKind::Claude)
        ));

        assert!(
            app.handle_transient_keyboard(&Key::Character("c".into()), keyboard::Modifiers::ALT,)
        );
        assert!(matches!(
            app.modal,
            Some(Modal::Form(FormModal { ref form, .. }))
                if form.provider().is_some_and(|provider| provider.kind == ProviderKind::Codex)
        ));
        assert!(app.handle_transient_keyboard(
            &Key::Character("s".into()),
            keyboard::Modifiers::default(),
        ));
        assert!(matches!(
            app.modal,
            Some(Modal::Form(FormModal { ref form, .. }))
                if form.provider().is_some_and(|provider| provider.kind == ProviderKind::Codex)
        ));
    }

    #[test]
    fn active_navigation_tabs_have_a_clear_persistent_accent() {
        let theme = theme::resolve(presentation::PresentationTheme::resolve(
            DesktopTheme::Canopy,
            SystemAppearance::Dark,
        ));
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
    fn selected_navigator_rows_use_a_subtle_surface_fill() {
        for choice in DesktopTheme::ALL {
            let theme = theme::resolve(PresentationTheme::resolve(choice, SystemAppearance::Dark));
            let style = list_item_style(&theme, Status::Active, true);
            assert_eq!(
                style.background,
                Some(Background::Color(
                    theme.extended_palette().background.weak.color
                ))
            );
            let Some(Background::Color(background)) = style.background else {
                unreachable!();
            };
            assert!(background.relative_contrast(style.text_color) >= 4.5);
        }
    }

    #[test]
    fn custom_button_styles_remain_readable_and_have_distinct_hover_states() {
        let choices = [
            ("grove", DesktopTheme::Grove, iced::theme::Mode::Light),
            ("canopy", DesktopTheme::Canopy, iced::theme::Mode::Dark),
            ("midnight", DesktopTheme::Midnight, iced::theme::Mode::Dark),
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
            let system = match mode {
                iced::theme::Mode::Light => SystemAppearance::Light,
                iced::theme::Mode::None | iced::theme::Mode::Dark => SystemAppearance::Dark,
            };
            let theme = theme::resolve(presentation::PresentationTheme::resolve(choice, system));
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
    fn add_controls_are_borderless_and_workspace_add_has_ten_pixel_gap() {
        let theme = theme::resolve(PresentationTheme::resolve(
            DesktopTheme::Canopy,
            SystemAppearance::Dark,
        ));
        for status in [
            Status::Active,
            Status::Hovered,
            Status::Pressed,
            Status::Disabled,
        ] {
            assert!(borderless_icon_style(&theme, status).border.width.abs() < f32::EPSILON);
        }
        assert!(
            borderless_icon_style(&theme, Status::Active)
                .background
                .is_none()
        );
        assert!(
            borderless_icon_style(&theme, Status::Hovered)
                .background
                .is_some()
        );
        assert!(
            borderless_icon_style(&theme, Status::Pressed)
                .background
                .is_some()
        );

        let source = include_str!("lib.rs");
        assert!(source.matches(".style(borderless_icon_style)").count() >= 3);
        assert!(source.matches("space::horizontal().width(10)").count() >= 2);
    }

    #[test]
    fn settings_are_centered_and_bounded_at_every_breakpoint() {
        let source = include_str!("lib.rs");
        let view = source
            .split_once("    fn view(&self)")
            .and_then(|(_, tail)| tail.split_once("    fn top_bar(&self)"))
            .map(|(body, _)| body)
            .expect("desktop view source");
        let settings = source
            .split_once("    fn settings_view(&self)")
            .and_then(|(_, tail)| tail.split_once("    fn appearance_theme_gallery"))
            .map(|(body, _)| body)
            .expect("settings view source");
        assert!(view.contains(".center_x(Fill)"));
        assert!(view.contains(".center_y(Fill)"));
        assert!(view.contains(".padding(16)"));
        assert!(settings.contains(".max_width(640)"));
        assert!(settings.contains("scrollable("));
        assert!(!include_str!("presentation.rs").contains("SettingsMode"));
    }

    #[test]
    fn iced_button_intents_match_the_presentation_contract() {
        for choice in DesktopTheme::ALL {
            let presentation =
                presentation::PresentationTheme::resolve(choice, SystemAppearance::Dark);
            let theme = theme::resolve(presentation);
            for intent in [
                ButtonIntent::Primary,
                ButtonIntent::Secondary,
                ButtonIntent::Quiet,
                ButtonIntent::Danger,
            ] {
                for status in [
                    Status::Active,
                    Status::Hovered,
                    Status::Pressed,
                    Status::Disabled,
                ] {
                    assert_eq!(
                        iced_button_intent_style(&theme, intent, status),
                        button_intent_style(presentation, intent, status),
                        "{choice} {intent:?} {status:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn session_tabs_prioritize_the_session_name() {
        assert_eq!(session_tab_label("Build API"), "Build API");
    }

    #[test]
    fn inline_navigator_rename_retains_input_and_errors_until_resolved() {
        let session_id = SessionId::new();
        let mut rename =
            InlineNavigatorRename::new(NavigatorNodeId::Session(session_id), "Old name");
        rename.update("  New name  ".into());

        assert_eq!(rename.begin_submission().as_deref(), Some("New name"));
        assert!(rename.pending);

        rename.fail("name already exists".into());
        assert!(!rename.pending);
        assert_eq!(rename.value, "  New name  ");
        assert_eq!(rename.error.as_deref(), Some("name already exists"));
    }

    #[test]
    fn inline_navigator_rename_cancels_only_before_submission() {
        let session_id = SessionId::new();
        let target = NavigatorNodeId::Session(session_id);
        let mut rename = Some(InlineNavigatorRename::new(target, "Name"));
        cancel_inline_navigator_rename(&mut rename);
        assert!(rename.is_none());

        let mut pending = InlineNavigatorRename::new(target, "Name");
        assert_eq!(pending.begin_submission().as_deref(), Some("Name"));
        let mut rename = Some(pending);
        cancel_inline_navigator_rename(&mut rename);
        assert!(
            rename.is_some(),
            "an in-flight rename cannot be cancelled locally"
        );
    }

    #[test]
    fn stale_inline_navigator_rename_is_discarded_after_refresh() {
        let (app, project_id, _, _, _) = desktop_hierarchy();
        let mut rename = Some(InlineNavigatorRename::new(
            NavigatorNodeId::Repository(project_id),
            "Name",
        ));
        retain_inline_navigator_rename(&mut rename, &app.snapshot);
        assert!(rename.is_some());

        retain_inline_navigator_rename(&mut rename, &DaemonSnapshot::default());
        assert!(rename.is_none());
    }

    #[test]
    fn footer_uses_a_readable_status_text_size() {
        const {
            assert!(FOOTER_TEXT_SIZE >= 12.0);
        }
        assert!(application_version_text().starts_with("SylvOps v"));
        assert_eq!(
            format_application_version(Some("v0.1.0-preview.2"), None, "0.1.0"),
            "SylvOps v0.1.0-preview.2"
        );
        let source = include_str!("lib.rs");
        let footer = source
            .split_once("    fn footer(&self)")
            .and_then(|(_, tail)| tail.split_once("    fn process_bridge_events"))
            .map(|(body, _)| body)
            .expect("footer source");
        assert!(footer.matches("footer_context(").count() >= 5);
        assert!(footer.contains("scrollable(status)"));
        assert!(footer.contains("scrollable::Direction::Horizontal"));
        assert!(!footer.contains("Length::Fixed"));
        assert!(footer.contains("Wrapping::None"));
    }

    #[test]
    fn navigator_headers_explain_the_three_step_sequence() {
        let source = include_str!("lib.rs");
        let navigator = source
            .split_once("    fn repositories_column(&self)")
            .and_then(|(_, tail)| tail.split_once("    fn compact_navigator(&self)"))
            .map(|(body, _)| body)
            .expect("navigator column source");
        assert!(navigator.contains("\"Repositories\""));
        assert!(navigator.contains("\"Checkouts\""));
        assert!(navigator.contains("\"Sessions\""));
        assert!(!navigator.contains("Choose a codebase"));
        assert!(!navigator.contains("Choose a branch checkout"));
        assert!(!navigator.contains("Choose or start work"));
        assert!(!navigator.contains("1  Repositories"));
        assert!(!navigator.contains("2  Checkouts"));
        assert!(!navigator.contains("3  Sessions"));
        assert!(navigator.matches("navigator_panel").count() >= 3);
        assert!(source.contains("navigator_action_strip(actions, density)"));
        assert!(!navigator.contains("session_indicator_icon"));
        assert!(!source.contains("text(format!(\"· {}\", row.detail))"));
    }

    #[test]
    fn bundled_ui_font_has_all_required_weights() {
        assert_eq!(UI_FONT.family, font::Family::Name("JetBrains Mono"));
        for bytes in [
            JETBRAINS_MONO_REGULAR,
            JETBRAINS_MONO_MEDIUM,
            JETBRAINS_MONO_SEMIBOLD,
        ] {
            assert!(bytes.len() > 200_000);
            assert_eq!(&bytes[..4], &[0, 1, 0, 0]);
        }
        assert_ne!(terminal_font(DesktopTerminalFont::System), UI_FONT);
    }

    #[test]
    fn details_format_dates_for_people_and_bound_technical_copy_payloads() {
        let formatted =
            human_readable_timestamp(1_700_000_000_000).expect("a valid millisecond timestamp");
        assert!(formatted.contains("2023"));
        assert!(!formatted.contains("1700000000000"));
        assert_ne!(
            human_readable_timestamp_for_locale(1_700_000_000_000, "en-US"),
            human_readable_timestamp_for_locale(1_700_000_000_000, "de-DE")
        );
        assert_eq!(chrono_locale("mk-MK"), Locale::mk_MK);
        assert_eq!(chrono_locale("en_US.UTF-8"), Locale::en_US);

        let oversized = "x".repeat(MAX_TECHNICAL_COPY_CHARS + 10);
        let bounded = bounded_technical_copy(&oversized);
        assert_eq!(bounded.chars().count(), MAX_TECHNICAL_COPY_CHARS);
        assert_eq!(human_byte_size(1_536), "1.5 KiB");
    }

    #[test]
    fn diagnostics_are_bounded_single_line_and_redact_secret_assignments() {
        for message in [
            "provider failed\ntoken=super-secret".to_owned(),
            "provider failed password = hunter2".to_owned(),
            "Authorization: Bearer sk-secret".to_owned(),
        ] {
            let diagnostic = bounded_redacted_diagnostic(&message);
            assert!(!diagnostic.contains("super-secret"));
            assert!(!diagnostic.contains("hunter2"));
            assert!(!diagnostic.contains("sk-secret"));
            assert!(!diagnostic.contains('\n'));
            assert!(diagnostic.contains("[redacted]"));
        }

        let oversized = "x".repeat(MAX_USER_DIAGNOSTIC_CHARS + 20);
        assert_eq!(
            bounded_redacted_diagnostic(&oversized).chars().count(),
            MAX_USER_DIAGNOSTIC_CHARS
        );
    }

    #[test]
    fn copying_technical_details_does_not_dismiss_an_unrelated_error() {
        let runtime_root = std::env::temp_dir().join(format!(
            "sylvops-desktop-copy-error-test-{}",
            SessionId::new()
        ));
        let paths = RuntimePaths::discover(Some(&runtime_root)).expect("runtime paths");
        let mut app = DesktopApp::new(paths);
        app.error = Some("Keep this visible".into());

        let _ = app.update(Message::CopyTechnicalValue("bounded value".into()));

        assert_eq!(app.error.as_deref(), Some("Keep this visible"));
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
    fn resume_requires_daemon_eligible_state_and_verified_external_id() {
        let available = session(SessionState::FinishedSeen, Some("verified-id"));
        let codex_health = [provider_health(ProviderKind::Codex, true, true)];
        assert!(session_can_resume(
            &available,
            std::slice::from_ref(&available),
            &codex_health
        ));
        let missing_id = session(SessionState::FinishedSeen, None);
        assert!(!session_can_resume(
            &missing_id,
            std::slice::from_ref(&missing_id),
            &codex_health
        ));
        let terminated = session(SessionState::Terminated, Some("verified-id"));
        assert!(!session_can_resume(
            &terminated,
            std::slice::from_ref(&terminated),
            &codex_health
        ));
        let mut claude = available.clone();
        claude.provider_kind = ProviderKind::Claude;
        let claude_health = [provider_health(ProviderKind::Claude, true, true)];
        assert!(session_can_resume(
            &claude,
            std::slice::from_ref(&claude),
            &claude_health
        ));
        let mut other_provider = claude.clone();
        other_provider.id = SessionId::new();
        other_provider.provider_kind = ProviderKind::Codex;
        other_provider.created_at = claude.created_at + 1;
        assert!(session_can_resume(
            &claude,
            &[claude.clone(), other_provider],
            &claude_health
        ));
        let mut other_worktree = claude.clone();
        other_worktree.id = SessionId::new();
        other_worktree.worktree_id = WorktreeId::new();
        other_worktree.created_at = claude.created_at + 1;
        assert!(session_can_resume(
            &claude,
            &[claude.clone(), other_worktree],
            &claude_health
        ));
        let mut unsupported_provider = available.clone();
        unsupported_provider.provider_kind = ProviderKind::Shell;
        assert!(!session_can_resume(
            &unsupported_provider,
            std::slice::from_ref(&unsupported_provider),
            &[provider_health(ProviderKind::Shell, true, true)]
        ));
    }

    #[test]
    fn successful_resume_selects_successor_and_preserves_source_history() {
        let runtime_root =
            std::env::temp_dir().join(format!("sylvops-desktop-resume-test-{}", SessionId::new()));
        let paths = RuntimePaths::discover(Some(&runtime_root)).expect("runtime paths");
        let mut app = DesktopApp::new(paths);
        let source = session(SessionState::FinishedSeen, Some("verified-id"));
        let mut successor = source.clone();
        successor.id = SessionId::new();
        successor.created_at = 2;
        successor.display_name = "Codex (resumed)".into();
        successor.state = SessionState::Running;
        successor.process_id = Some(42);
        successor.ended_at = None;
        successor.exit_code = None;
        app.snapshot.sessions.push(source.clone());
        app.selected_session_id = Some(source.id);
        app.active_session_id = Some(source.id);
        app.open_sessions.push(source.id);
        app.resume_pending.insert(source.id);

        app.handle_response(
            Operation::Resume(source.id),
            DaemonResponse::SessionResumed {
                revision: 2,
                session: successor.clone(),
            },
        );

        assert_eq!(app.selected_session_id, Some(successor.id));
        assert_eq!(app.active_session_id, Some(successor.id));
        assert_eq!(app.main_tab, MainTab::Terminal);
        assert!(app.open_sessions.contains(&successor.id));
        assert!(!app.resume_pending.contains(&source.id));
        assert!(
            app.snapshot
                .sessions
                .iter()
                .any(|item| item.id == source.id)
        );
        assert!(
            app.snapshot
                .sessions
                .iter()
                .any(|item| item.id == successor.id)
        );
        let historical_source = app
            .snapshot
            .sessions
            .iter()
            .find(|item| item.id == source.id)
            .expect("historical source");
        assert!(!sylvops_core::domain::session_record_can_resume(
            historical_source,
            &app.snapshot.sessions
        ));
    }

    #[test]
    fn desktop_suppresses_duplicate_resume_requests_while_one_is_pending() {
        let runtime_root = std::env::temp_dir().join(format!(
            "sylvops-desktop-resume-pending-test-{}",
            SessionId::new()
        ));
        let paths = RuntimePaths::discover(Some(&runtime_root)).expect("runtime paths");
        let mut app = DesktopApp::new(paths);
        let source = session(SessionState::FinishedSeen, Some("verified-id"));
        app.providers
            .push(provider_health(ProviderKind::Codex, true, true));
        app.snapshot.sessions.push(source.clone());

        let request = app
            .begin_resume_request(source.id)
            .expect("first resume request");
        assert_eq!(
            request,
            ClientRequest::ResumeSession {
                session_id: source.id,
                columns: app.terminal_dimensions().0,
                rows: app.terminal_dimensions().1,
            }
        );
        assert!(app.begin_resume_request(source.id).is_none());
        assert_eq!(app.resume_pending.len(), 1);
    }

    #[test]
    fn stale_resume_error_clears_pending_and_is_actionable() {
        let runtime_root = std::env::temp_dir().join(format!(
            "sylvops-desktop-resume-stale-test-{}",
            SessionId::new()
        ));
        let paths = RuntimePaths::discover(Some(&runtime_root)).expect("runtime paths");
        let mut app = DesktopApp::new(paths);
        let source = session(SessionState::FinishedSeen, Some("verified-id"));
        app.resume_pending.insert(source.id);

        app.handle_response(
            Operation::Resume(source.id),
            DaemonResponse::Error(sylvops_core::protocol::ProtocolFailure {
                code: "conflict".into(),
                message: "This session has already been resumed.".into(),
                retryable: false,
            }),
        );

        assert!(!app.resume_pending.contains(&source.id));
        assert!(
            app.error
                .as_deref()
                .is_some_and(|message| message.contains("already been resumed"))
        );
    }

    #[test]
    fn terminal_actions_keep_standardized_labels_at_every_width() {
        assert_eq!(
            active_terminal_action_label(ActiveTerminalAction::Open, false),
            "Open terminal"
        );
        assert_eq!(
            active_terminal_action_label(ActiveTerminalAction::Open, true),
            "Open terminal"
        );
        assert_eq!(
            active_terminal_action_label(ActiveTerminalAction::Leave, false),
            "Leave terminal"
        );
        assert_eq!(
            active_terminal_action_label(ActiveTerminalAction::Leave, true),
            "Leave terminal"
        );
        assert_eq!(
            active_terminal_action_label(ActiveTerminalAction::ViewOutput, true),
            "View output"
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
    fn first_run_copy_explains_the_hierarchy_in_order() {
        let steps = first_run_steps(FormKind::CreateWorkspace).unwrap();
        assert_eq!(
            steps.map(|step| step.title),
            ["Create workspace", "Add repository", "Start session"]
        );
        assert!(steps[0].description.contains("repositories"));
        assert!(steps[1].description.contains("root checkout"));
        assert!(steps[2].description.contains("Claude Code"));
    }

    #[test]
    fn cancelled_first_run_can_resume_from_the_empty_state() {
        let runtime_root = std::env::temp_dir().join(format!(
            "sylvops-desktop-first-run-resume-test-{}",
            SessionId::new()
        ));
        let paths = RuntimePaths::discover(Some(&runtime_root)).expect("runtime paths");
        let mut app = DesktopApp::new(paths);

        let _ = app.update(Message::NewWorkspace);
        assert_eq!(app.modal_focus, ModalFocus::Pending);
        assert!(matches!(
            app.modal,
            Some(Modal::Form(FormModal {
                first_run: true,
                ..
            }))
        ));
        let _ = app.update(Message::CancelModal);
        let _ = app.update(Message::NewWorkspace);
        assert!(matches!(
            app.modal,
            Some(Modal::Form(FormModal {
                first_run: true,
                ..
            }))
        ));
    }

    #[test]
    fn provider_recovery_copy_offers_retry_and_shell_fallback() {
        let mut codex_missing = provider_health(ProviderKind::Codex, false, false);
        codex_missing.diagnostic = Some(
            "Codex is not installed as a supported native CLI. Install Codex, then refresh Provider health."
                .into(),
        );
        assert!(provider_recovery_message(&codex_missing).contains("Install Codex"));

        let mut codex_logged_out = provider_health(ProviderKind::Codex, true, false);
        codex_logged_out.diagnostic = Some(
            "Codex authentication could not be verified. Sign in to Codex outside SylvOps or run `codex login status`, then refresh Provider health."
                .into(),
        );
        assert!(provider_recovery_message(&codex_logged_out).contains("Sign in to Codex"));

        let mut unavailable = provider_health(ProviderKind::Claude, false, false);
        unavailable.diagnostic = Some(
            "Claude Code is too old. Update it to 2.1.145 or newer, then refresh Provider health."
                .into(),
        );
        let unavailable = provider_recovery_message(&unavailable);
        assert!(unavailable.contains("Update it to 2.1.145 or newer"));
        assert!(unavailable.contains("Shell remains available"));
        assert_eq!(CHECK_AGAIN_LABEL, "Check again");

        let mut unauthenticated = provider_health(ProviderKind::Claude, true, false);
        unauthenticated.diagnostic = Some(
            "Claude Code is not logged in. Run `claude auth login` yourself, then refresh Provider health."
                .into(),
        );
        let unauthenticated = provider_recovery_message(&unauthenticated);
        assert!(unauthenticated.contains("Run `claude auth login` yourself"));
        assert!(unauthenticated.contains("Shell remains available"));
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
            "Start shell"
        );
    }

    #[test]
    fn desktop_data_removal_can_be_cancelled_before_submission() {
        let runtime_root = std::env::temp_dir().join(format!(
            "sylvops-desktop-data-removal-cancel-test-{}",
            SessionId::new()
        ));
        let paths = RuntimePaths::discover(Some(&runtime_root)).expect("runtime paths");
        let mut app = DesktopApp::new(paths);

        app.open_data_removal_confirmation();
        assert!(matches!(app.modal, Some(Modal::DataRemoval(_))));

        let _ = app.update(Message::CancelModal);
        assert!(app.modal.is_none());
    }

    #[test]
    fn desktop_data_removal_recovers_from_an_active_session_refusal() {
        let runtime_root = std::env::temp_dir().join(format!(
            "sylvops-desktop-data-removal-active-test-{}",
            SessionId::new()
        ));
        let paths = RuntimePaths::discover(Some(&runtime_root)).expect("runtime paths");
        let mut app = DesktopApp::new(paths);
        app.modal = Some(Modal::DataRemoval(DataRemovalConfirmation {
            confirmation: sylvops_daemon::data_removal::DATA_REMOVAL_CONFIRMATION.into(),
            pending: true,
        }));

        app.handle_response(
            Operation::PrepareDataRemoval,
            DaemonResponse::Error(sylvops_core::protocol::ProtocolFailure {
                code: "daemon_operation_failed".into(),
                message: "user data cannot be removed while sessions are active".into(),
                retryable: false,
            }),
        );

        assert!(matches!(
            app.modal,
            Some(Modal::DataRemoval(DataRemovalConfirmation {
                pending: false,
                ..
            }))
        ));
        assert!(
            app.error
                .as_deref()
                .is_some_and(|message| message.contains("sessions are active"))
        );
    }

    #[test]
    fn desktop_closes_only_after_data_removal_finishes() {
        let runtime_root = std::env::temp_dir().join(format!(
            "sylvops-desktop-data-removal-finished-test-{}",
            SessionId::new()
        ));
        let paths = RuntimePaths::discover(Some(&runtime_root)).expect("runtime paths");
        let mut app = DesktopApp::new(paths);
        app.modal = Some(Modal::DataRemoval(DataRemovalConfirmation {
            confirmation: sylvops_daemon::data_removal::DATA_REMOVAL_CONFIRMATION.into(),
            pending: true,
        }));

        app.handle_bridge_event(BridgeEvent::DataRemovalFinished(Ok(())));

        assert!(app.modal.is_none());
        assert!(app.closing_since.is_some());
        assert!(app.success.as_ref().is_some_and(|(message, _)| {
            message.contains("worktrees, and branches were preserved")
        }));
    }

    #[test]
    fn desktop_reports_a_bounded_partial_removal_failure() {
        let runtime_root = std::env::temp_dir().join(format!(
            "sylvops-desktop-data-removal-failure-test-{}",
            SessionId::new()
        ));
        let paths = RuntimePaths::discover(Some(&runtime_root)).expect("runtime paths");
        let mut app = DesktopApp::new(paths);
        app.modal = Some(Modal::DataRemoval(DataRemovalConfirmation {
            confirmation: sylvops_daemon::data_removal::DATA_REMOVAL_CONFIRMATION.into(),
            pending: true,
        }));

        app.handle_bridge_event(BridgeEvent::DataRemovalFinished(Err(
            "user-data removal did not complete".into(),
        )));

        assert!(app.modal.is_none());
        assert!(matches!(app.connection, ConnectionState::Disconnected));
        let message = app.error.as_deref().expect("bounded failure message");
        assert!(message.contains("did not complete"));
        assert!(!message.contains(&runtime_root.display().to_string()));
    }
}
