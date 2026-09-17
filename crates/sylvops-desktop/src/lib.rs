//! Native, mouse-first `SylvOps` client backed exclusively by authenticated daemon IPC.

mod bridge;

use std::{collections::HashMap, time::Duration};

use bridge::{Bridge, BridgeEvent, Operation};
use iced::{
    Center, Element, Fill, Font, Length, Subscription, Theme,
    alignment::Vertical,
    keyboard,
    keyboard::{Key, key::Named},
    time,
    widget::{button, column, container, row, rule, scrollable, space, text},
};
use sylvops_core::{
    domain::{
        AttachmentRole, DaemonSnapshot, Project, ProviderKind, Session, SessionState, Workspace,
        Worktree, WorktreeStatus,
    },
    ids::{ProjectId, SessionId, WorkspaceId, WorktreeId},
    protocol::{ClientRequest, DaemonEvent, DaemonResponse},
    provider::ProviderHealth,
    ui::MainTab,
};
use sylvops_daemon::runtime::RuntimePaths;

const EVENT_TICK: Duration = Duration::from_millis(16);
const MAX_EVENTS_PER_TICK: usize = 512;
const DEFAULT_TERMINAL_COLUMNS: u16 = 100;
const DEFAULT_TERMINAL_ROWS: u16 = 30;

/// Opens the native `SylvOps` desktop client.
///
/// # Errors
///
/// Returns an error when the native window or renderer cannot be initialized.
pub fn run(paths: RuntimePaths) -> iced::Result {
    iced::application(
        move || DesktopApp::new(paths.clone()),
        DesktopApp::update,
        DesktopApp::view,
    )
    .title("SylvOps")
    .theme(DesktopApp::theme)
    .subscription(subscription)
    .window_size((1_280.0, 800.0))
    .run()
}

struct TerminalState {
    role: AttachmentRole,
    parser: vt100::Parser,
    last_sequence: u64,
    attached: bool,
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
    settings_open: bool,
    theme_choice: ThemeChoice,
}

#[derive(Clone, Debug)]
enum ConnectionState {
    Connecting,
    Connected,
    Disconnected,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ThemeChoice {
    System,
    Light,
    Dark,
    Nord,
    TokyoNight,
    CatppuccinMocha,
}

impl ThemeChoice {
    const ALL: [Self; 6] = [
        Self::System,
        Self::Light,
        Self::Dark,
        Self::Nord,
        Self::TokyoNight,
        Self::CatppuccinMocha,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::Light => "Light",
            Self::Dark => "Dark",
            Self::Nord => "Nord",
            Self::TokyoNight => "Tokyo Night",
            Self::CatppuccinMocha => "Catppuccin",
        }
    }

    fn iced(self) -> Option<Theme> {
        match self {
            Self::System => None,
            Self::Light => Some(Theme::Light),
            Self::Dark => Some(Theme::Dark),
            Self::Nord => Some(Theme::Nord),
            Self::TokyoNight => Some(Theme::TokyoNight),
            Self::CatppuccinMocha => Some(Theme::CatppuccinMocha),
        }
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
    CreateShell,
    Attach,
    Detach,
    Stop,
    Refresh,
    ToggleSettings,
    SelectTheme(ThemeChoice),
    ClearError,
}

impl DesktopApp {
    fn new(paths: RuntimePaths) -> Self {
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
            settings_open: false,
            theme_choice: ThemeChoice::System,
        }
    }

    fn update(&mut self, message: Message) {
        match message {
            Message::Tick => self.process_bridge_events(),
            Message::Keyboard(event) => self.handle_keyboard(event),
            Message::SelectWorkspace(workspace_id) => {
                self.send_request(
                    Operation::OpenWorkspace(workspace_id),
                    ClientRequest::OpenWorkspace { workspace_id },
                );
            }
            Message::SelectProject(project_id) => {
                self.selected_project_id = Some(project_id);
                self.selected_worktree_id = self
                    .worktrees_for(project_id)
                    .first()
                    .map(|worktree| worktree.id);
                self.selected_session_id = self
                    .selected_worktree_id
                    .and_then(|id| self.sessions_for(id).first().map(|session| session.id));
            }
            Message::SelectWorktree(worktree_id) => {
                self.selected_worktree_id = Some(worktree_id);
                self.selected_session_id = self
                    .sessions_for(worktree_id)
                    .first()
                    .map(|session| session.id);
                self.diff = None;
            }
            Message::SelectSession(session_id) => {
                self.selected_session_id = Some(session_id);
                self.active_session_id = Some(session_id);
                if !self.open_sessions.contains(&session_id) {
                    self.open_sessions.push(session_id);
                }
            }
            Message::SelectOpenSession(session_id) => {
                self.select_session_context(session_id);
                self.active_session_id = Some(session_id);
            }
            Message::CloseSessionTab(session_id) => {
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
            }
            Message::SelectMainTab(tab) => {
                self.main_tab = tab;
                if tab == MainTab::Changes {
                    self.load_diff();
                }
            }
            Message::CreateShell => self.create_shell(),
            Message::Attach => self.attach_active(),
            Message::Detach => self.detach_active(),
            Message::Stop => self.stop_active(),
            Message::Refresh => self.request_snapshot(),
            Message::ToggleSettings => self.settings_open = !self.settings_open,
            Message::SelectTheme(theme) => self.theme_choice = theme,
            Message::ClearError => self.error = None,
        }
    }

    fn theme(&self) -> Option<Theme> {
        self.theme_choice.iced()
    }

    fn view(&self) -> Element<'_, Message> {
        let top = self.top_bar();
        let body = if self.settings_open {
            self.settings_view()
        } else {
            self.mission_control()
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
        container(content).width(Fill).height(Fill).into()
    }

    fn top_bar(&self) -> Element<'_, Message> {
        let mut workspaces = row![text("✦ SylvOps").size(20)].spacing(8).align_y(Center);
        for workspace in &self.snapshot.workspaces {
            let label = if workspace.is_open {
                format!("● {}", workspace.name)
            } else {
                workspace.name.clone()
            };
            let action =
                button(text(label).size(14)).on_press(Message::SelectWorkspace(workspace.id));
            workspaces = workspaces.push(action);
        }
        workspaces = workspaces
            .push(space::horizontal())
            .push(button("Refresh").on_press(Message::Refresh))
            .push(button("Settings").on_press(Message::ToggleSettings));
        container(workspaces)
            .height(52)
            .width(Fill)
            .padding([8, 12])
            .center_y(Fill)
            .into()
    }

    fn mission_control(&self) -> Element<'_, Message> {
        row![
            self.projects_column(),
            rule::vertical(1),
            self.worktrees_column(),
            rule::vertical(1),
            self.sessions_column(),
            rule::vertical(1),
            self.workspace_view(),
        ]
        .width(Fill)
        .height(Fill)
        .into()
    }

    fn projects_column(&self) -> Element<'_, Message> {
        let mut items = column![column_heading("Projects", None)].spacing(4);
        for project in self.visible_projects() {
            items = items.push(select_button(
                &project.name,
                self.selected_project_id == Some(project.id),
                Message::SelectProject(project.id),
            ));
        }
        if self.visible_projects().is_empty() {
            items = items.push(empty_hint("No repositories in this workspace."));
        }
        panel(scrollable(items), 180.0)
    }

    fn worktrees_column(&self) -> Element<'_, Message> {
        let mut items = column![column_heading("Worktrees", None)].spacing(4);
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
                ));
            }
        }
        if self.selected_project_id.is_none() {
            items = items.push(empty_hint("Select a project."));
        }
        panel(scrollable(items), 220.0)
    }

    fn sessions_column(&self) -> Element<'_, Message> {
        let create = self.selected_worktree_id.map(|_| Message::CreateShell);
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
                ));
            }
        }
        if self.selected_worktree_id.is_none() {
            items = items.push(empty_hint("Select a worktree."));
        }
        panel(scrollable(items), 250.0)
    }

    fn workspace_view(&self) -> Element<'_, Message> {
        let tabs = self.session_tabs();
        let views = row![
            tab_button("Terminal", MainTab::Terminal, self.main_tab),
            tab_button("Changes", MainTab::Changes, self.main_tab),
            tab_button("Details", MainTab::Details, self.main_tab),
            space::horizontal(),
            self.session_actions(),
        ]
        .spacing(6)
        .align_y(Center);
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
                        button(text(label).size(13))
                            .on_press(Message::SelectOpenSession(session.id))
                            .style(if self.active_session_id == Some(session.id) {
                                button::primary
                            } else {
                                button::secondary
                            }),
                    )
                    .push(
                        button("×")
                            .on_press(Message::CloseSessionTab(session.id))
                            .style(button::text),
                    );
            }
        }
        if self.open_sessions.is_empty() {
            tabs = tabs.push(text("Select a session to open it").style(text::secondary));
        }
        container(tabs)
            .height(44)
            .padding([6, 10])
            .width(Fill)
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
            actions = actions.push(button("Detach").on_press(Message::Detach));
        } else {
            actions = actions.push(
                button("Attach")
                    .on_press(Message::Attach)
                    .style(button::primary),
            );
        }
        actions.push(button("Stop").on_press(Message::Stop)).into()
    }

    fn terminal_view(&self) -> Element<'_, Message> {
        let Some(session_id) = self.active_session_id else {
            return centered_message("Choose a session, then click Attach.");
        };
        let Some(terminal) = self.terminals.get(&session_id) else {
            return centered_message("Session selected. Click Attach to load its terminal.");
        };
        let role = format!("{} · Ctrl+] detaches", terminal.role);
        let contents = terminal.parser.screen().contents();
        container(
            column![
                text(role).size(12).style(text::secondary),
                scrollable(text(contents).font(Font::MONOSPACE).size(14).width(Fill)).height(Fill),
            ]
            .spacing(8),
        )
        .padding(14)
        .width(Fill)
        .height(Fill)
        .into()
    }

    fn changes_view(&self) -> Element<'_, Message> {
        let body = self.diff.as_deref().unwrap_or(
            "Select a worktree and open Changes. The daemon will load a bounded, read-only diff.",
        );
        container(scrollable(text(body).font(Font::MONOSPACE).size(13)))
            .padding(14)
            .width(Fill)
            .height(Fill)
            .into()
    }

    fn details_view(&self) -> Element<'_, Message> {
        let Some(session_id) = self.active_session_id else {
            return centered_message("Select a session to inspect its details.");
        };
        let Some(session) = self.session(session_id) else {
            return centered_message("The selected session is no longer available.");
        };
        let worktree = self
            .snapshot
            .worktrees
            .iter()
            .find(|worktree| worktree.id == session.worktree_id);
        container(
            column![
                text(&session.display_name).size(24),
                detail("Provider", session.provider_kind.to_string()),
                detail("State", session.state.to_string()),
                detail(
                    "Worktree",
                    worktree.map_or_else(|| "Unknown".into(), |item| item.name.clone())
                ),
                detail("Working directory", session.cwd.clone()),
                detail(
                    "Process",
                    session
                        .process_id
                        .map_or_else(|| "—".into(), |id| id.to_string())
                ),
                detail(
                    "Resume",
                    if session.external_session_id.is_some() {
                        "Available".into()
                    } else {
                        "Unavailable".into()
                    }
                ),
            ]
            .spacing(12),
        )
        .padding(20)
        .width(Fill)
        .height(Fill)
        .into()
    }

    fn settings_view(&self) -> Element<'_, Message> {
        let mut themes = row![].spacing(8);
        for choice in ThemeChoice::ALL {
            themes = themes.push(
                button(choice.label())
                    .on_press(Message::SelectTheme(choice))
                    .style(if self.theme_choice == choice {
                        button::primary
                    } else {
                        button::secondary
                    }),
            );
        }
        container(
            column![
                row![
                    text("Settings").size(28),
                    space::horizontal(),
                    button("Done").on_press(Message::ToggleSettings)
                ]
                .align_y(Center),
                text("Appearance").size(18),
                text("Theme changes apply immediately. Persistence and terminal typography follow in the settings milestone.")
                    .style(text::secondary),
                themes,
                rule::horizontal(1),
                text("Safety").size(18),
                text("The desktop remains an IPC client. The daemon still owns PTYs, Git mutations, process cleanup, and audit events.")
                    .style(text::secondary),
            ]
            .spacing(16),
        )
        .padding(24)
        .width(Fill)
        .height(Fill)
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
                text(format!("SylvOps v{}", env!("CARGO_PKG_VERSION"))),
                text(workspace).style(text::secondary),
                text(branch).style(text::secondary),
                text(format!("{:?}", self.main_tab)).style(text::secondary),
                space::horizontal(),
                text(connection).style(text::secondary),
                text("Ctrl+K Commands · Ctrl+] Detach").style(text::secondary),
            ]
            .spacing(14)
            .align_y(Center),
        )
        .height(36)
        .padding([6, 12])
        .width(Fill)
        .into()
    }

    fn process_bridge_events(&mut self) {
        if self.bridge.take_overflowed() {
            self.request_snapshot();
            if let Some(session_id) = self.active_session_id {
                let from_sequence = self
                    .terminals
                    .get(&session_id)
                    .map_or(0, |terminal| terminal.last_sequence);
                self.send_request(
                    Operation::Attach(session_id),
                    ClientRequest::AttachSession {
                        session_id,
                        from_sequence,
                        columns: DEFAULT_TERMINAL_COLUMNS,
                        rows: DEFAULT_TERMINAL_ROWS,
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
            } => {
                self.snapshot = snapshot;
                self.providers = providers;
                self.connection = ConnectionState::Connected;
                self.restore_selection();
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
                self.error = Some(message);
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
                Operation::CreateShell(worktree_id),
                DaemonResponse::SessionCreated { session, .. },
            ) => {
                self.selected_worktree_id = Some(worktree_id);
                self.selected_session_id = Some(session.id);
                self.active_session_id = Some(session.id);
                if !self.open_sessions.contains(&session.id) {
                    self.open_sessions.push(session.id);
                }
                self.request_snapshot();
            }
            (Operation::Stop(session_id), DaemonResponse::Acknowledged) => {
                if let Some(terminal) = self.terminals.get_mut(&session_id) {
                    terminal.attached = false;
                }
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
                let terminal = self
                    .terminals
                    .entry(session_id)
                    .or_insert_with(|| TerminalState {
                        role,
                        parser: vt100::Parser::new(
                            DEFAULT_TERMINAL_ROWS,
                            DEFAULT_TERMINAL_COLUMNS,
                            10_000,
                        ),
                        last_sequence: 0,
                        attached: true,
                    });
                terminal.role = role;
                terminal.attached = true;
                if let Some(snapshot) = terminal_snapshot {
                    terminal.parser =
                        vt100::Parser::new(DEFAULT_TERMINAL_ROWS, DEFAULT_TERMINAL_COLUMNS, 10_000);
                    terminal.parser.process(&snapshot);
                    terminal.last_sequence = replay_through_sequence;
                }
            }
            (Operation::Detach(session_id), DaemonResponse::Acknowledged) => {
                if let Some(terminal) = self.terminals.get_mut(&session_id) {
                    terminal.attached = false;
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
        if self.settings_open || self.main_tab != MainTab::Terminal {
            return;
        }
        let Some(session_id) = self.active_session_id else {
            return;
        };
        let Some(terminal) = self.terminals.get(&session_id) else {
            return;
        };
        if !terminal.attached {
            return;
        }
        let keyboard::Event::KeyPressed {
            key,
            modifiers,
            text,
            ..
        } = event
        else {
            return;
        };
        if modifiers.control() && matches!(key.as_ref(), Key::Character("]")) {
            self.detach_active();
            return;
        }
        if terminal.role != AttachmentRole::Controller {
            return;
        }
        let Some(bytes) = encode_terminal_key(&key, modifiers, text.as_deref()) else {
            return;
        };
        self.send_request(
            Operation::Input(session_id),
            ClientRequest::SessionInput { session_id, bytes },
        );
    }

    fn request_snapshot(&mut self) {
        if self.snapshot_pending {
            return;
        }
        self.snapshot_pending = self
            .bridge
            .request(Operation::RefreshSnapshot, ClientRequest::GetSnapshot);
    }

    fn create_shell(&mut self) {
        let Some(worktree_id) = self.selected_worktree_id else {
            self.error = Some("Select a worktree before creating a session.".into());
            return;
        };
        self.send_request(
            Operation::CreateShell(worktree_id),
            ClientRequest::CreateSession {
                worktree_id,
                provider: ProviderKind::Shell,
                display_name: None,
                model: None,
                effort: None,
                initial_prompt: None,
                columns: DEFAULT_TERMINAL_COLUMNS,
                rows: DEFAULT_TERMINAL_ROWS,
            },
        );
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
        self.send_request(
            Operation::Attach(session_id),
            ClientRequest::AttachSession {
                session_id,
                from_sequence,
                columns: DEFAULT_TERMINAL_COLUMNS,
                rows: DEFAULT_TERMINAL_ROWS,
            },
        );
    }

    fn detach_active(&mut self) {
        if let Some(session_id) = self.active_session_id {
            self.send_request(
                Operation::Detach(session_id),
                ClientRequest::DetachSession { session_id },
            );
        }
    }

    fn stop_active(&mut self) {
        if let Some(session_id) = self.active_session_id {
            self.send_request(
                Operation::Stop(session_id),
                ClientRequest::StopSession { session_id },
            );
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
    ])
}

fn panel<'a>(content: impl Into<Element<'a, Message>>, width: f32) -> Element<'a, Message> {
    container(content)
        .width(Length::Fixed(width))
        .height(Fill)
        .padding(6)
        .into()
}

fn column_heading(label: &str, action: Option<Message>) -> Element<'_, Message> {
    let mut heading = row![text(label).size(12), space::horizontal()].align_y(Center);
    if let Some(action) = action {
        heading = heading.push(button("+").on_press(action).style(button::text));
    }
    container(heading)
        .height(38)
        .padding([6, 8])
        .width(Fill)
        .into()
}

fn select_button(label: &str, selected: bool, message: Message) -> Element<'static, Message> {
    button(text(label.to_owned()).size(13).width(Fill))
        .on_press(message)
        .style(if selected {
            button::primary
        } else {
            button::text
        })
        .width(Fill)
        .padding([7, 8])
        .into()
}

fn tab_button(label: &str, tab: MainTab, active: MainTab) -> Element<'_, Message> {
    button(label)
        .on_press(Message::SelectMainTab(tab))
        .style(if tab == active {
            button::primary
        } else {
            button::text
        })
        .into()
}

fn centered_message(message: &str) -> Element<'_, Message> {
    container(text(message).style(text::secondary))
        .width(Fill)
        .height(Fill)
        .center(Fill)
        .into()
}

fn empty_hint(message: &str) -> Element<'_, Message> {
    container(text(message).size(12).style(text::secondary))
        .padding(8)
        .width(Fill)
        .into()
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

fn encode_terminal_key(
    key: &Key,
    modifiers: keyboard::Modifiers,
    produced_text: Option<&str>,
) -> Option<Vec<u8>> {
    let mut bytes = match key.as_ref() {
        Key::Named(Named::Enter) => vec![b'\r'],
        Key::Named(Named::Tab) if modifiers.shift() => b"\x1b[Z".to_vec(),
        Key::Named(Named::Tab) => vec![b'\t'],
        Key::Named(Named::Backspace) => vec![0x7f],
        Key::Named(Named::Escape) => vec![0x1b],
        Key::Named(Named::ArrowUp) => b"\x1b[A".to_vec(),
        Key::Named(Named::ArrowDown) => b"\x1b[B".to_vec(),
        Key::Named(Named::ArrowRight) => b"\x1b[C".to_vec(),
        Key::Named(Named::ArrowLeft) => b"\x1b[D".to_vec(),
        Key::Named(Named::Home) => b"\x1b[H".to_vec(),
        Key::Named(Named::End) => b"\x1b[F".to_vec(),
        Key::Named(Named::Delete) => b"\x1b[3~".to_vec(),
        Key::Named(Named::Insert) => b"\x1b[2~".to_vec(),
        Key::Named(Named::PageUp) => b"\x1b[5~".to_vec(),
        Key::Named(Named::PageDown) => b"\x1b[6~".to_vec(),
        Key::Character(character) if modifiers.control() => {
            let character = character.chars().next()?;
            if !character.is_ascii() {
                return None;
            }
            let byte = u8::try_from(u32::from(character.to_ascii_uppercase())).ok()?;
            vec![byte & 0x1f]
        }
        Key::Character(character) => produced_text.unwrap_or(character).as_bytes().to_vec(),
        _ => return None,
    };
    if modifiers.alt() && bytes.first() != Some(&0x1b) {
        bytes.insert(0, 0x1b);
    }
    Some(bytes)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_keyboard_reserves_detach_outside_encoder_and_encodes_navigation() {
        assert_eq!(
            encode_terminal_key(
                &Key::Named(Named::ArrowUp),
                keyboard::Modifiers::empty(),
                None,
            ),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            encode_terminal_key(&Key::Character("c".into()), keyboard::Modifiers::CTRL, None,),
            Some(vec![0x03])
        );
    }
}
