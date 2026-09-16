//! Keyboard-first mission control backed exclusively by daemon IPC.

use std::{io::stdout, str::FromStr, time::Duration};

use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
};
use sylvops_core::{
    domain::{
        AttachmentRole, DaemonSnapshot, Project, ProviderKind, Session, SessionState, Workspace,
        Worktree, WorktreeStatus, attention_priority,
    },
    ids::{ProjectId, SessionId, WorkspaceId, WorktreeId},
    protocol::{
        ClientRequest, DaemonEvent, DaemonResponse, MAX_PTY_CHUNK_SIZE, MAX_TERMINAL_COLUMNS,
        MAX_TERMINAL_ROWS, MIN_TERMINAL_COLUMNS, MIN_TERMINAL_ROWS,
    },
};
use sylvops_daemon::{DaemonError, client::DaemonClient, runtime::RuntimePaths};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TuiExit {
    Quit,
}

#[derive(Clone, Copy, Debug)]
enum InputAction {
    CreateWorkspace,
    RegisterProject(WorkspaceId),
    CreateWorktree(ProjectId),
    CreateSession(WorktreeId),
    RenameProject(ProjectId),
    RenameWorktree(WorktreeId),
    RenameSession(SessionId),
}

#[derive(Clone, Debug)]
struct InputModal {
    title: String,
    labels: Vec<&'static str>,
    values: Vec<String>,
    active: usize,
    action: InputAction,
}

#[derive(Clone, Debug)]
enum ConfirmAction {
    RemoveWorktree {
        worktree_id: WorktreeId,
        token: String,
    },
    StopSession(SessionId),
}

#[derive(Clone, Debug)]
struct ConfirmModal {
    title: String,
    detail: String,
    action: ConfirmAction,
}

#[derive(Clone, Debug)]
enum PaletteItem {
    CreateWorkspace,
    RegisterProject,
    Workspace(WorkspaceId, String),
    Project(ProjectId, String),
    Worktree(WorktreeId, String),
    Session(SessionId, String),
}

impl PaletteItem {
    fn label(&self) -> String {
        match self {
            Self::CreateWorkspace => "Command: create workspace".into(),
            Self::RegisterProject => "Command: register repository".into(),
            Self::Workspace(_, name) => format!("Workspace: {name}"),
            Self::Project(_, name) => format!("Project: {name}"),
            Self::Worktree(_, name) => format!("Worktree: {name}"),
            Self::Session(_, name) => format!("Session: {name}"),
        }
    }
}

#[derive(Clone, Debug, Default)]
struct Palette {
    query: String,
    selected: usize,
}

#[derive(Clone, Debug)]
enum Mode {
    Navigation,
    Input(InputModal),
    Confirm(ConfirmModal),
    Palette(Palette),
    Attached,
}

struct AttachedTerminal {
    session_id: SessionId,
    role: AttachmentRole,
    parser: vt100::Parser,
    last_sequence: u64,
    columns: u16,
    rows: u16,
}

struct App {
    snapshot: DaemonSnapshot,
    mode: Mode,
    panel: usize,
    selections: [usize; 3],
    preview: String,
    message: String,
    help: bool,
    attached: Option<AttachedTerminal>,
}

impl App {
    fn new(snapshot: DaemonSnapshot) -> Self {
        let mode = if snapshot.workspaces.is_empty() {
            Mode::Input(workspace_modal())
        } else {
            Mode::Navigation
        };
        Self {
            snapshot,
            mode,
            panel: 0,
            selections: [0; 3],
            preview: "Select a worktree and press g to inspect its bounded Git diff.".into(),
            message: "n new · r rename · d stop/remove · / palette · ? help".into(),
            help: false,
            attached: None,
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
        let workspace = self.active_workspace().map(|workspace| workspace.id);
        self.snapshot
            .projects
            .iter()
            .filter(|project| Some(project.workspace_id) == workspace)
            .collect()
    }

    fn selected_project(&self) -> Option<&Project> {
        self.visible_projects().get(self.selections[0]).copied()
    }

    fn visible_worktrees(&self) -> Vec<&Worktree> {
        let project = self.selected_project().map(|project| project.id);
        let mut worktrees: Vec<_> = self
            .snapshot
            .worktrees
            .iter()
            .filter(|worktree| {
                Some(worktree.project_id) == project && worktree.status != WorktreeStatus::Removed
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

    fn selected_worktree(&self) -> Option<&Worktree> {
        self.visible_worktrees().get(self.selections[1]).copied()
    }

    fn visible_sessions(&self) -> Vec<&Session> {
        let worktree = self.selected_worktree().map(|worktree| worktree.id);
        let mut sessions: Vec<_> = self
            .snapshot
            .sessions
            .iter()
            .filter(|session| Some(session.worktree_id) == worktree)
            .collect();
        sessions.sort_by_key(|session| std::cmp::Reverse(session.last_activity_at));
        sessions
    }

    fn selected_session(&self) -> Option<&Session> {
        self.visible_sessions().get(self.selections[2]).copied()
    }

    fn move_selection(&mut self, delta: isize) {
        let length = match self.panel {
            0 => self.visible_projects().len(),
            1 => self.visible_worktrees().len(),
            2 => self.visible_sessions().len(),
            _ => 0,
        };
        if length == 0 || self.panel > 2 {
            return;
        }
        let current = self.selections[self.panel];
        self.selections[self.panel] = if delta < 0 {
            current.saturating_sub(delta.unsigned_abs())
        } else {
            current.saturating_add(delta.unsigned_abs()).min(length - 1)
        };
        if self.selections[self.panel] != current {
            if self.panel == 0 {
                self.selections[1] = 0;
                self.selections[2] = 0;
            } else if self.panel == 1 {
                self.selections[2] = 0;
            }
        }
    }

    fn select_attention(&mut self) {
        let target = self
            .snapshot
            .sessions
            .iter()
            .filter(|session| attention_priority(session.state) < 4)
            .min_by_key(|session| {
                (
                    attention_priority(session.state),
                    std::cmp::Reverse(session.last_activity_at),
                )
            })
            .map(|session| session.id);
        if let Some(id) = target {
            self.select_session(id);
        } else {
            self.message = "No sessions currently need attention.".into();
        }
    }

    fn select_project(&mut self, id: ProjectId) {
        if let Some(index) = self
            .visible_projects()
            .iter()
            .position(|project| project.id == id)
        {
            self.selections = [index, 0, 0];
            self.panel = 0;
        }
    }

    fn select_worktree(&mut self, id: WorktreeId) {
        let target = self
            .snapshot
            .worktrees
            .iter()
            .find(|worktree| worktree.id == id)
            .map(|worktree| worktree.project_id);
        if let Some(project_id) = target {
            self.select_project(project_id);
            if let Some(index) = self
                .visible_worktrees()
                .iter()
                .position(|worktree| worktree.id == id)
            {
                self.selections[1] = index;
                self.selections[2] = 0;
                self.panel = 1;
            }
        }
    }

    fn select_session(&mut self, id: SessionId) {
        let target = self
            .snapshot
            .sessions
            .iter()
            .find(|session| session.id == id)
            .map(|session| session.worktree_id);
        if let Some(worktree_id) = target {
            self.select_worktree(worktree_id);
            if let Some(index) = self
                .visible_sessions()
                .iter()
                .position(|session| session.id == id)
            {
                self.selections[2] = index;
                self.panel = 2;
            }
        }
    }

    fn clamp_selections(&mut self) {
        let lengths = [
            self.visible_projects().len(),
            self.visible_worktrees().len(),
            self.visible_sessions().len(),
        ];
        for (selection, length) in self.selections.iter_mut().zip(lengths) {
            *selection = (*selection).min(length.saturating_sub(1));
        }
    }

    fn palette_items(&self, query: &str) -> Vec<PaletteItem> {
        let mut items = vec![PaletteItem::CreateWorkspace, PaletteItem::RegisterProject];
        items.extend(
            self.snapshot
                .workspaces
                .iter()
                .map(|item| PaletteItem::Workspace(item.id, item.name.clone())),
        );
        items.extend(
            self.visible_projects()
                .into_iter()
                .map(|item| PaletteItem::Project(item.id, item.name.clone())),
        );
        items.extend(
            self.visible_worktrees()
                .into_iter()
                .map(|item| PaletteItem::Worktree(item.id, item.name.clone())),
        );
        items.extend(
            self.visible_sessions()
                .into_iter()
                .map(|item| PaletteItem::Session(item.id, item.display_name.clone())),
        );
        let query = query.to_lowercase();
        items
            .into_iter()
            .filter(|item| item.label().to_lowercase().contains(&query))
            .collect()
    }
}

/// Opens mission control. The daemon remains authoritative for processes and Git mutations.
///
/// # Errors
///
/// Returns an IPC, terminal initialization, rendering, input, or daemon-operation error.
pub async fn run(paths: &RuntimePaths) -> Result<TuiExit, DaemonError> {
    let client = DaemonClient::connect(paths, "sylvops-tui").await?;
    let mut app = App::new(snapshot(&client).await?);
    let mut events = client.subscribe();
    let _guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)
        .map_err(|error| DaemonError::Lifecycle(format!("cannot initialize TUI: {error}")))?;

    loop {
        let mut refresh = false;
        for _ in 0..512 {
            match events.try_recv() {
                Ok(DaemonEvent::SessionOutput {
                    session_id,
                    sequence,
                    bytes,
                    ..
                }) => {
                    if let Some(attached) = app.attached.as_mut()
                        && attached.session_id == session_id
                        && sequence > attached.last_sequence
                    {
                        attached.parser.process(&bytes);
                        attached.last_sequence = sequence;
                    }
                }
                Ok(DaemonEvent::ResynchronizationRequired {
                    session_id,
                    snapshot_sequence,
                    columns,
                    rows,
                    terminal_snapshot,
                }) => {
                    if let Some(attached) = app.attached.as_mut()
                        && attached.session_id == session_id
                    {
                        attached.parser = vt100::Parser::new(rows, columns, 0);
                        attached.parser.process(&terminal_snapshot);
                        attached.last_sequence = snapshot_sequence;
                        attached.columns = columns;
                        attached.rows = rows;
                    }
                }
                Ok(_) => refresh = true,
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {
                    refresh = true;
                    resynchronize_terminal(&client, &mut app).await?;
                    break;
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                    return Err(DaemonError::Lifecycle("daemon event stream closed".into()));
                }
            }
        }
        if refresh {
            app.snapshot = snapshot(&client).await?;
            app.clamp_selections();
        }
        if matches!(app.mode, Mode::Attached) {
            let area = terminal
                .size()
                .map_err(|error| DaemonError::Lifecycle(error.to_string()))?;
            let area = Rect::new(0, 0, area.width, area.height);
            resize_attached(&client, &mut app, panel_areas(area).1).await?;
        }
        terminal
            .draw(|frame| draw(frame, &app))
            .map_err(|error| DaemonError::Lifecycle(format!("cannot draw TUI: {error}")))?;
        if !event::poll(Duration::from_millis(60))
            .map_err(|error| DaemonError::Lifecycle(format!("cannot poll input: {error}")))?
        {
            continue;
        }
        let input = event::read()
            .map_err(|error| DaemonError::Lifecycle(format!("cannot read input: {error}")))?;
        if let Event::Key(key) = &input
            && key.kind == KeyEventKind::Release
        {
            continue;
        }
        match app.mode.clone() {
            Mode::Navigation => {
                if handle_navigation_input(&client, &mut app, input).await? {
                    return Ok(TuiExit::Quit);
                }
            }
            Mode::Input(_) => handle_modal_input(&client, &mut app, input).await?,
            Mode::Confirm(_) => handle_confirm_input(&client, &mut app, input).await?,
            Mode::Palette(_) => handle_palette_input(&client, &mut app, input).await?,
            Mode::Attached => handle_terminal_input(&client, &mut app, input).await?,
        }
    }
}

async fn handle_navigation_input(
    client: &DaemonClient,
    app: &mut App,
    input: Event,
) -> Result<bool, DaemonError> {
    let Event::Key(key) = input else {
        return Ok(false);
    };
    if key.code == KeyCode::Char('?') {
        app.help = !app.help;
        return Ok(false);
    }
    if app.help {
        if matches!(key.code, KeyCode::Esc | KeyCode::Enter) {
            app.help = false;
        }
        return Ok(false);
    }
    match key.code {
        KeyCode::Char('q') => return Ok(true),
        KeyCode::Tab | KeyCode::Char('l') => app.panel = (app.panel + 1) % 4,
        KeyCode::BackTab | KeyCode::Char('h') => app.panel = (app.panel + 3) % 4,
        KeyCode::Down | KeyCode::Char('j') => app.move_selection(1),
        KeyCode::Up | KeyCode::Char('k') => app.move_selection(-1),
        KeyCode::Char('A') if key.modifiers.contains(KeyModifiers::SHIFT) => {
            app.select_attention();
        }
        KeyCode::Char('/') => app.mode = Mode::Palette(Palette::default()),
        KeyCode::Char('n') => open_create_modal(app),
        KeyCode::Char('r') => open_rename_modal(app),
        KeyCode::Char('d') => open_destructive_confirmation(client, app).await?,
        KeyCode::Enter if app.panel == 2 => attach_selected(client, app).await?,
        KeyCode::Char('g') => show_diff(client, app).await?,
        _ => {}
    }
    Ok(false)
}

fn open_create_modal(app: &mut App) {
    let workspace_id = app.active_workspace().map(|workspace| workspace.id);
    let project_id = app.selected_project().map(|project| project.id);
    let worktree_id = app.selected_worktree().map(|worktree| worktree.id);
    app.mode = match app.panel {
        0 => match workspace_id {
            Some(workspace_id) => Mode::Input(InputModal {
                title: "Register Git repository".into(),
                labels: vec!["Repository path"],
                values: vec![".".into()],
                active: 0,
                action: InputAction::RegisterProject(workspace_id),
            }),
            None => Mode::Input(workspace_modal()),
        },
        1 => project_id.map_or_else(
            || {
                app.message = "Select a project first.".into();
                Mode::Navigation
            },
            |project_id| {
                Mode::Input(InputModal {
                    title: "Create managed worktree".into(),
                    labels: vec!["Branch", "Display name (optional)", "Base ref (optional)"],
                    values: vec![String::new(), String::new(), String::new()],
                    active: 0,
                    action: InputAction::CreateWorktree(project_id),
                })
            },
        ),
        2 => worktree_id.map_or_else(
            || {
                app.message = "Select a worktree first.".into();
                Mode::Navigation
            },
            |worktree_id| {
                Mode::Input(InputModal {
                    title: "Create terminal session".into(),
                    labels: vec![
                        "Provider (shell/codex)",
                        "Display name (optional)",
                        "Model (optional)",
                        "Effort (optional)",
                        "Initial prompt (optional)",
                    ],
                    values: vec![
                        "shell".into(),
                        String::new(),
                        String::new(),
                        String::new(),
                        String::new(),
                    ],
                    active: 0,
                    action: InputAction::CreateSession(worktree_id),
                })
            },
        ),
        _ => {
            app.message = "Creation is available from Projects, Worktrees, or Sessions.".into();
            Mode::Navigation
        }
    };
}

fn open_rename_modal(app: &mut App) {
    let target = match app.panel {
        0 => app.selected_project().map(|project| {
            (
                "Rename project",
                project.name.clone(),
                InputAction::RenameProject(project.id),
            )
        }),
        1 => app.selected_worktree().map(|worktree| {
            (
                "Rename worktree label",
                worktree.name.clone(),
                InputAction::RenameWorktree(worktree.id),
            )
        }),
        2 => app.selected_session().map(|session| {
            (
                "Rename session",
                session.display_name.clone(),
                InputAction::RenameSession(session.id),
            )
        }),
        _ => None,
    };
    if let Some((title, value, action)) = target {
        app.mode = Mode::Input(InputModal {
            title: title.into(),
            labels: vec!["Display name"],
            values: vec![value],
            active: 0,
            action,
        });
    } else {
        app.message = "Select a project, worktree, or session to rename.".into();
    }
}

async fn open_destructive_confirmation(
    client: &DaemonClient,
    app: &mut App,
) -> Result<(), DaemonError> {
    if app.panel == 1 {
        let Some(worktree) = app.selected_worktree().cloned() else {
            app.message = "Select a worktree first.".into();
            return Ok(());
        };
        if worktree.is_root_checkout {
            app.message = "The root checkout cannot be removed by SylvOps.".into();
            return Ok(());
        }
        match client
            .request(&ClientRequest::GetWorktreeStatus {
                worktree_id: worktree.id,
            })
            .await?
        {
            DaemonResponse::WorktreeStatus(status) if status.clean => {
                let Some(token) = status.removal_confirmation_token else {
                    app.message = "No clean-state removal token was issued.".into();
                    return Ok(());
                };
                app.mode = Mode::Confirm(ConfirmModal {
                    title: "Remove clean worktree?".into(),
                    detail: format!(
                        "Remove exactly {}? The Git branch is preserved. Press y to confirm.",
                        worktree.canonical_path
                    ),
                    action: ConfirmAction::RemoveWorktree {
                        worktree_id: worktree.id,
                        token,
                    },
                });
            }
            DaemonResponse::WorktreeStatus(status) => {
                app.message = format!(
                    "Refused: {} tracked, {} untracked, {} ignored entries.",
                    status.tracked_changes, status.untracked_files, status.ignored_files
                );
            }
            response => app.message = response_message("worktree status", response),
        }
    } else if app.panel == 2 {
        let Some(session) = app.selected_session().cloned() else {
            app.message = "Select a session first.".into();
            return Ok(());
        };
        if !matches!(
            session.state,
            SessionState::Fresh
                | SessionState::Starting
                | SessionState::Running
                | SessionState::NeedsFeedback
        ) {
            app.message = "That session is already inactive; its history is retained.".into();
            return Ok(());
        }
        app.mode = Mode::Confirm(ConfirmModal {
            title: "Stop session process tree?".into(),
            detail: format!(
                "Stop '{}' and all descendants? Press y to confirm.",
                session.display_name
            ),
            action: ConfirmAction::StopSession(session.id),
        });
    } else if app.panel == 0 {
        app.message = "Project deletion is intentionally unavailable in this beta.".into();
    }
    Ok(())
}

async fn show_diff(client: &DaemonClient, app: &mut App) -> Result<(), DaemonError> {
    let Some(worktree_id) = app.selected_worktree().map(|worktree| worktree.id) else {
        app.message = "Select a worktree first.".into();
        return Ok(());
    };
    app.preview = match client
        .request(&ClientRequest::GetDiff { worktree_id })
        .await?
    {
        DaemonResponse::Diff(diff) if diff.text.is_empty() => "No tracked changes.".into(),
        DaemonResponse::Diff(diff) => diff.text,
        response => response_message("Git diff", response),
    };
    app.panel = 3;
    Ok(())
}

async fn attach_selected(client: &DaemonClient, app: &mut App) -> Result<(), DaemonError> {
    let Some(session_id) = app.selected_session().map(|session| session.id) else {
        app.message = "Select a session first.".into();
        return Ok(());
    };
    let (columns, rows) = (80, 24);
    match client
        .request(&ClientRequest::AttachSession {
            session_id,
            from_sequence: 0,
            columns,
            rows,
        })
        .await?
    {
        DaemonResponse::Attached {
            role,
            replay_through_sequence,
            terminal_snapshot,
            ..
        } => {
            let restored = terminal_snapshot.is_some();
            let mut parser = vt100::Parser::new(rows, columns, 0);
            if let Some(snapshot) = terminal_snapshot {
                parser.process(&snapshot);
            }
            app.attached = Some(AttachedTerminal {
                session_id,
                role,
                parser,
                last_sequence: if restored { replay_through_sequence } else { 0 },
                columns,
                rows,
            });
            app.panel = 3;
            app.mode = Mode::Attached;
            app.message = if role == AttachmentRole::Controller {
                "Attached as controller · Ctrl+] detaches".into()
            } else {
                "Attached as read-only observer · Ctrl+] detaches".into()
            };
        }
        response => app.message = response_message("session attachment", response),
    }
    Ok(())
}

async fn handle_modal_input(
    client: &DaemonClient,
    app: &mut App,
    input: Event,
) -> Result<(), DaemonError> {
    if let Event::Paste(text) = &input {
        if let Mode::Input(modal) = &mut app.mode {
            modal.values[modal.active].push_str(text);
        }
        return Ok(());
    }
    let Event::Key(key) = input else {
        return Ok(());
    };
    let Mode::Input(modal) = &mut app.mode else {
        return Ok(());
    };
    match key.code {
        KeyCode::Esc => app.mode = Mode::Navigation,
        KeyCode::Tab | KeyCode::Down => modal.active = (modal.active + 1) % modal.values.len(),
        KeyCode::BackTab | KeyCode::Up => {
            modal.active = (modal.active + modal.values.len() - 1) % modal.values.len();
        }
        KeyCode::Backspace => {
            modal.values[modal.active].pop();
        }
        KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            modal.values[modal.active].push(character);
        }
        KeyCode::Enter if modal.active + 1 < modal.values.len() => modal.active += 1,
        KeyCode::Enter => {
            let modal = modal.clone();
            app.mode = Mode::Navigation;
            match perform_input_action(client, &modal).await {
                Ok(message) => {
                    app.message = message;
                    app.snapshot = snapshot(client).await?;
                    app.clamp_selections();
                }
                Err(error) => app.message = error.to_string(),
            }
        }
        _ => {}
    }
    Ok(())
}

async fn perform_input_action(
    client: &DaemonClient,
    modal: &InputModal,
) -> Result<String, DaemonError> {
    let optional = |index: usize| {
        let value = modal.values[index].trim();
        (!value.is_empty()).then(|| value.to_owned())
    };
    let (operation, response) = match modal.action {
        InputAction::CreateWorkspace => (
            "workspace creation",
            client
                .request(&ClientRequest::AddWorkspace {
                    name: modal.values[0].clone(),
                })
                .await?,
        ),
        InputAction::RegisterProject(workspace_id) => (
            "repository registration",
            client
                .request(&ClientRequest::AddProject {
                    workspace_id,
                    repository_path: modal.values[0].clone(),
                })
                .await?,
        ),
        InputAction::CreateWorktree(project_id) => (
            "worktree creation",
            client
                .request(&ClientRequest::CreateWorktree {
                    project_id,
                    branch: modal.values[0].clone(),
                    name: optional(1),
                    base_ref: optional(2),
                })
                .await?,
        ),
        InputAction::CreateSession(worktree_id) => {
            let provider = ProviderKind::from_str(modal.values[0].trim())
                .map_err(DaemonError::InvalidSession)?;
            (
                "session creation",
                client
                    .request(&ClientRequest::CreateSession {
                        worktree_id,
                        provider,
                        display_name: optional(1),
                        model: optional(2),
                        effort: optional(3),
                        initial_prompt: optional(4),
                        columns: 80,
                        rows: 24,
                    })
                    .await?,
            )
        }
        InputAction::RenameProject(project_id) => (
            "project rename",
            client
                .request(&ClientRequest::RenameProject {
                    project_id,
                    name: modal.values[0].clone(),
                })
                .await?,
        ),
        InputAction::RenameWorktree(worktree_id) => (
            "worktree rename",
            client
                .request(&ClientRequest::RenameWorktree {
                    worktree_id,
                    name: modal.values[0].clone(),
                })
                .await?,
        ),
        InputAction::RenameSession(session_id) => (
            "session rename",
            client
                .request(&ClientRequest::RenameSession {
                    session_id,
                    name: modal.values[0].clone(),
                })
                .await?,
        ),
    };
    if let DaemonResponse::Error(error) = response {
        return Err(DaemonError::Lifecycle(format!(
            "{operation} failed: {}",
            error.message
        )));
    }
    Ok(format!("{operation} succeeded"))
}

async fn handle_confirm_input(
    client: &DaemonClient,
    app: &mut App,
    input: Event,
) -> Result<(), DaemonError> {
    let Event::Key(key) = input else {
        return Ok(());
    };
    if matches!(key.code, KeyCode::Esc | KeyCode::Char('n')) {
        app.mode = Mode::Navigation;
        return Ok(());
    }
    if key.code != KeyCode::Char('y') {
        return Ok(());
    }
    let Mode::Confirm(confirm) = app.mode.clone() else {
        return Ok(());
    };
    app.mode = Mode::Navigation;
    let response = match confirm.action {
        ConfirmAction::RemoveWorktree { worktree_id, token } => {
            client
                .request(&ClientRequest::RemoveWorktree {
                    worktree_id,
                    confirmation_token: token,
                })
                .await?
        }
        ConfirmAction::StopSession(session_id) => {
            client
                .request(&ClientRequest::StopSession { session_id })
                .await?
        }
    };
    if let DaemonResponse::Error(error) = response {
        app.message = error.message;
    } else {
        app.message = "Operation completed.".into();
        app.snapshot = snapshot(client).await?;
        app.clamp_selections();
    }
    Ok(())
}

async fn handle_palette_input(
    client: &DaemonClient,
    app: &mut App,
    input: Event,
) -> Result<(), DaemonError> {
    let Event::Key(key) = input else {
        return Ok(());
    };
    let Mode::Palette(mut palette) = app.mode.clone() else {
        return Ok(());
    };
    match key.code {
        KeyCode::Esc => app.mode = Mode::Navigation,
        KeyCode::Backspace => {
            palette.query.pop();
            palette.selected = 0;
            app.mode = Mode::Palette(palette);
        }
        KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            palette.query.push(character);
            palette.selected = 0;
            app.mode = Mode::Palette(palette);
        }
        KeyCode::Down => {
            let length = app.palette_items(&palette.query).len();
            palette.selected = (palette.selected + 1).min(length.saturating_sub(1));
            app.mode = Mode::Palette(palette);
        }
        KeyCode::Up => {
            palette.selected = palette.selected.saturating_sub(1);
            app.mode = Mode::Palette(palette);
        }
        KeyCode::Enter => {
            let item = app
                .palette_items(&palette.query)
                .get(palette.selected)
                .cloned();
            app.mode = Mode::Navigation;
            if let Some(item) = item {
                execute_palette_item(client, app, item).await?;
            }
        }
        _ => {}
    }
    Ok(())
}

async fn execute_palette_item(
    client: &DaemonClient,
    app: &mut App,
    item: PaletteItem,
) -> Result<(), DaemonError> {
    match item {
        PaletteItem::CreateWorkspace => app.mode = Mode::Input(workspace_modal()),
        PaletteItem::RegisterProject => {
            app.panel = 0;
            open_create_modal(app);
        }
        PaletteItem::Workspace(workspace_id, _) => {
            match client
                .request(&ClientRequest::OpenWorkspace { workspace_id })
                .await?
            {
                DaemonResponse::WorkspaceOpened { .. } => {
                    app.snapshot = snapshot(client).await?;
                    app.selections = [0; 3];
                    app.message = "Workspace opened.".into();
                }
                response => app.message = response_message("workspace selection", response),
            }
        }
        PaletteItem::Project(id, _) => app.select_project(id),
        PaletteItem::Worktree(id, _) => app.select_worktree(id),
        PaletteItem::Session(id, _) => app.select_session(id),
    }
    Ok(())
}

async fn handle_terminal_input(
    client: &DaemonClient,
    app: &mut App,
    input: Event,
) -> Result<(), DaemonError> {
    let Some(attached) = app.attached.as_ref() else {
        app.mode = Mode::Navigation;
        return Ok(());
    };
    let session_id = attached.session_id;
    let role = attached.role;
    match input {
        Event::Key(key) if is_detach_key(key) => {
            let _ = client
                .request(&ClientRequest::DetachSession { session_id })
                .await;
            app.attached = None;
            app.mode = Mode::Navigation;
            app.message = "Detached; the daemon still owns the process.".into();
        }
        Event::Key(key) if role == AttachmentRole::Controller => {
            if let Some(bytes) = encode_key(key) {
                let response = client
                    .request(&ClientRequest::SessionInput { session_id, bytes })
                    .await?;
                if !matches!(response, DaemonResponse::Acknowledged) {
                    app.message = response_message("terminal input", response);
                }
            }
        }
        Event::Paste(text) if role == AttachmentRole::Controller => {
            for bytes in text.as_bytes().chunks(MAX_PTY_CHUNK_SIZE) {
                let response = client
                    .request(&ClientRequest::SessionInput {
                        session_id,
                        bytes: bytes.to_vec(),
                    })
                    .await?;
                if !matches!(response, DaemonResponse::Acknowledged) {
                    app.message = response_message("terminal paste", response);
                    break;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

async fn resize_attached(
    client: &DaemonClient,
    app: &mut App,
    area: Rect,
) -> Result<(), DaemonError> {
    let columns = area
        .width
        .saturating_sub(2)
        .clamp(MIN_TERMINAL_COLUMNS, MAX_TERMINAL_COLUMNS);
    let rows = area
        .height
        .saturating_sub(2)
        .clamp(MIN_TERMINAL_ROWS, MAX_TERMINAL_ROWS);
    let Some(attached) = app.attached.as_mut() else {
        return Ok(());
    };
    if attached.columns == columns && attached.rows == rows {
        return Ok(());
    }
    attached.parser.screen_mut().set_size(rows, columns);
    attached.columns = columns;
    attached.rows = rows;
    if attached.role == AttachmentRole::Controller {
        let response = client
            .request(&ClientRequest::ResizeSession {
                session_id: attached.session_id,
                columns,
                rows,
            })
            .await?;
        if !matches!(response, DaemonResponse::Acknowledged) {
            app.message = response_message("terminal resize", response);
        }
    }
    Ok(())
}

async fn resynchronize_terminal(client: &DaemonClient, app: &mut App) -> Result<(), DaemonError> {
    let Some(attached) = app.attached.as_mut() else {
        return Ok(());
    };
    if let DaemonResponse::Attached {
        replay_through_sequence,
        terminal_snapshot: Some(snapshot),
        ..
    } = client
        .request(&ClientRequest::AttachSession {
            session_id: attached.session_id,
            from_sequence: u64::MAX,
            columns: attached.columns,
            rows: attached.rows,
        })
        .await?
    {
        attached.parser = vt100::Parser::new(attached.rows, attached.columns, 0);
        attached.parser.process(&snapshot);
        attached.last_sequence = replay_through_sequence;
    }
    Ok(())
}

async fn snapshot(client: &DaemonClient) -> Result<DaemonSnapshot, DaemonError> {
    match client.request(&ClientRequest::GetSnapshot).await? {
        DaemonResponse::Snapshot(snapshot) => Ok(snapshot),
        DaemonResponse::Error(error) => Err(DaemonError::Lifecycle(error.message)),
        response => Err(DaemonError::Lifecycle(format!(
            "unexpected snapshot response: {response:?}"
        ))),
    }
}

fn workspace_modal() -> InputModal {
    InputModal {
        title: "Create workspace".into(),
        labels: vec!["Workspace name"],
        values: vec!["Local".into()],
        active: 0,
        action: InputAction::CreateWorkspace,
    }
}

fn response_message(operation: &str, response: DaemonResponse) -> String {
    match response {
        DaemonResponse::Error(error) => format!("{operation} failed: {}", error.message),
        response => format!("Unexpected {operation} response: {response:?}"),
    }
}

fn is_detach_key(key: KeyEvent) -> bool {
    key.code == KeyCode::Char(']') && key.modifiers.contains(KeyModifiers::CONTROL)
}

fn encode_key(key: KeyEvent) -> Option<Vec<u8>> {
    let bytes = match key.code {
        KeyCode::Char(character)
            if key.modifiers.contains(KeyModifiers::CONTROL) && character.is_ascii() =>
        {
            let byte = u8::try_from(u32::from(character.to_ascii_uppercase())).ok()?;
            vec![byte & 0x1f]
        }
        KeyCode::Char(character) => character.to_string().into_bytes(),
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        _ => return None,
    };
    Some(bytes)
}

fn draw(frame: &mut ratatui::Frame<'_>, app: &App) {
    let area = frame.area();
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(area);
    let workspace = app
        .active_workspace()
        .map_or("No workspace", |workspace| workspace.name.as_str());
    frame.render_widget(
        Paragraph::new(format!(" SylvOps · Workspace: {workspace}"))
            .style(Style::default().add_modifier(Modifier::BOLD)),
        vertical[0],
    );
    draw_panels(frame, app, vertical[1]);
    frame.render_widget(Paragraph::new(app.message.as_str()), vertical[2]);

    if app.help {
        let popup = centered(area, 76, 70);
        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new("Tab / Shift+Tab or h / l: panels\nj / k: selection\nn: create for panel\nr: rename metadata\nd: stop session/remove clean worktree\nEnter: attach selected session\ng: bounded Git diff\n/: command and entity palette\nShift+A: attention queue\nCtrl+] while attached: detach\nq: close UI; sessions continue\n?: close help")
                .block(Block::default().title(" SylvOps shortcuts ").borders(Borders::ALL))
                .wrap(Wrap { trim: false }),
            popup,
        );
    }
    match &app.mode {
        Mode::Input(modal) => draw_input_modal(frame, modal),
        Mode::Confirm(confirm) => draw_confirm_modal(frame, confirm),
        Mode::Palette(palette) => draw_palette(frame, app, palette),
        Mode::Navigation | Mode::Attached => {}
    }
}

fn draw_panels(frame: &mut ratatui::Frame<'_>, app: &App, area: Rect) {
    let (navigation_area, preview_area) = panel_areas(area);
    if area.width < 80 {
        match app.panel.min(2) {
            0 => draw_projects(frame, app, navigation_area),
            1 => draw_worktrees(frame, app, navigation_area),
            _ => draw_sessions(frame, app, navigation_area),
        }
    } else {
        let navigation = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(34),
                Constraint::Percentage(33),
                Constraint::Percentage(33),
            ])
            .split(navigation_area);
        draw_projects(frame, app, navigation[0]);
        draw_worktrees(frame, app, navigation[1]);
        draw_sessions(frame, app, navigation[2]);
    }
    let title = if matches!(app.mode, Mode::Attached) {
        "Terminal [attached]"
    } else if app.panel == 3 {
        "Preview [active]"
    } else {
        "Preview"
    };
    // `contents` is parser-produced text; untrusted OSC/DCS/control sequences are not emitted.
    let content = app.attached.as_ref().map_or_else(
        || app.preview.clone(),
        |terminal| terminal.parser.screen().contents(),
    );
    frame.render_widget(
        Paragraph::new(content)
            .block(Block::default().title(title).borders(Borders::ALL))
            .wrap(Wrap { trim: false }),
        preview_area,
    );
}

fn panel_areas(area: Rect) -> (Rect, Rect) {
    let direction = if area.width < 90 {
        Direction::Vertical
    } else {
        Direction::Horizontal
    };
    let panels = Layout::default()
        .direction(direction)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);
    (panels[0], panels[1])
}

fn draw_projects(frame: &mut ratatui::Frame<'_>, app: &App, area: Rect) {
    let items = app
        .visible_projects()
        .into_iter()
        .map(|project| ListItem::new(project.name.clone()))
        .collect();
    render_list(
        frame,
        area,
        "Projects",
        items,
        app.panel == 0,
        app.selections[0],
    );
}

fn draw_worktrees(frame: &mut ratatui::Frame<'_>, app: &App, area: Rect) {
    let items = app
        .visible_worktrees()
        .into_iter()
        .map(|worktree| {
            ListItem::new(format!(
                "{} {}",
                if worktree.is_root_checkout {
                    "⌂"
                } else {
                    "⑂"
                },
                worktree.name
            ))
        })
        .collect();
    render_list(
        frame,
        area,
        "Worktrees",
        items,
        app.panel == 1,
        app.selections[1],
    );
}

fn draw_sessions(frame: &mut ratatui::Frame<'_>, app: &App, area: Rect) {
    let items = app
        .visible_sessions()
        .into_iter()
        .map(|session| {
            ListItem::new(Line::from(vec![
                Span::raw(format!("{} ", status_symbol(session.state))),
                Span::raw(&session.display_name),
                Span::styled(
                    format!(" ({})", status_label(session.state)),
                    Style::default().fg(status_color(session.state)),
                ),
            ]))
        })
        .collect();
    render_list(
        frame,
        area,
        "Sessions",
        items,
        app.panel == 2,
        app.selections[2],
    );
}

fn render_list(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    title: &str,
    items: Vec<ListItem<'_>>,
    active: bool,
    selected: usize,
) {
    let mut state = ratatui::widgets::ListState::default().with_selected(Some(selected));
    let block = Block::default()
        .title(if active {
            format!("{title} [active]")
        } else {
            title.into()
        })
        .borders(Borders::ALL)
        .border_style(if active {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        });
    frame.render_stateful_widget(
        List::new(items)
            .block(block)
            .highlight_symbol("> ")
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
        area,
        &mut state,
    );
}

fn draw_input_modal(frame: &mut ratatui::Frame<'_>, modal: &InputModal) {
    let area = centered(frame.area(), 76, 55);
    frame.render_widget(Clear, area);
    let lines: Vec<_> = modal
        .labels
        .iter()
        .zip(&modal.values)
        .enumerate()
        .flat_map(|(index, (label, value))| {
            [
                Line::from(Span::styled(
                    format!("{} {label}", if index == modal.active { ">" } else { " " }),
                    Style::default().add_modifier(Modifier::BOLD),
                )),
                Line::from(format!("  {value}")),
            ]
        })
        .collect();
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title(format!(
                        " {} · Enter next/submit · Esc cancel ",
                        modal.title
                    ))
                    .borders(Borders::ALL),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_confirm_modal(frame: &mut ratatui::Frame<'_>, confirm: &ConfirmModal) {
    let area = centered(frame.area(), 72, 30);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(confirm.detail.as_str())
            .block(
                Block::default()
                    .title(format!(" {} · y confirm · Esc cancel ", confirm.title))
                    .borders(Borders::ALL),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_palette(frame: &mut ratatui::Frame<'_>, app: &App, palette: &Palette) {
    let area = centered(frame.area(), 76, 60);
    frame.render_widget(Clear, area);
    let items = app.palette_items(&palette.query);
    let mut state = ratatui::widgets::ListState::default()
        .with_selected((!items.is_empty()).then_some(palette.selected));
    let items = items
        .iter()
        .map(|item| ListItem::new(item.label()))
        .collect::<Vec<_>>();
    frame.render_stateful_widget(
        List::new(items)
            .block(
                Block::default()
                    .title(format!(" /{} ", palette.query))
                    .borders(Borders::ALL),
            )
            .highlight_symbol("> ")
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
        area,
        &mut state,
    );
}

fn status_symbol(state: SessionState) -> &'static str {
    match state {
        SessionState::Fresh => "○",
        SessionState::Starting | SessionState::Running => "◐",
        SessionState::NeedsFeedback => "!",
        SessionState::FinishedUnseen => "◆",
        SessionState::FinishedSeen => "✓",
        SessionState::Failed | SessionState::Terminated => "×",
        SessionState::Disconnected => "—",
    }
}

fn status_label(state: SessionState) -> &'static str {
    match state {
        SessionState::Fresh => "Fresh",
        SessionState::Starting => "Starting",
        SessionState::Running => "Running",
        SessionState::NeedsFeedback => "Needs feedback",
        SessionState::FinishedUnseen => "Finished unseen",
        SessionState::FinishedSeen => "Finished seen",
        SessionState::Failed => "Failed",
        SessionState::Terminated => "Terminated",
        SessionState::Disconnected => "Disconnected",
    }
}

fn status_color(state: SessionState) -> Color {
    match state {
        SessionState::NeedsFeedback => Color::Yellow,
        SessionState::Failed | SessionState::Terminated => Color::Red,
        SessionState::FinishedUnseen | SessionState::FinishedSeen => Color::Green,
        SessionState::Disconnected => Color::DarkGray,
        _ => Color::Cyan,
    }
}

fn centered(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

#[derive(Debug)]
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self, DaemonError> {
        enable_raw_mode().map_err(|error| DaemonError::Lifecycle(error.to_string()))?;
        if let Err(error) = execute!(stdout(), EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(DaemonError::Lifecycle(error.to_string()));
        }
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(stdout(), LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn populated_snapshot() -> DaemonSnapshot {
        let workspace_id = WorkspaceId::new();
        let project_id = ProjectId::new();
        let worktree_id = WorktreeId::new();
        DaemonSnapshot {
            revision: 1,
            captured_at: 1,
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
                name: "repo".into(),
                repository_path: "/repo".into(),
                canonical_repository_path: "/repo".into(),
                default_branch: Some("main".into()),
                remote_url: None,
                created_at: 1,
                last_activity_at: 1,
            }],
            worktrees: vec![Worktree {
                id: worktree_id,
                project_id,
                name: "root".into(),
                path: "/repo".into(),
                canonical_path: "/repo".into(),
                branch: Some("main".into()),
                base_ref: "HEAD".into(),
                base_commit: "abc".into(),
                is_root_checkout: true,
                status: WorktreeStatus::Active,
                created_at: 1,
                last_activity_at: 1,
                removed_at: None,
            }],
            sessions: vec![Session {
                id: SessionId::new(),
                worktree_id,
                provider_profile_id: None,
                provider_kind: ProviderKind::Shell,
                display_name: "shell".into(),
                state: SessionState::Running,
                process_id: Some(42),
                external_session_id: None,
                command: "/bin/sh".into(),
                arguments_json: "[]".into(),
                cwd: "/repo".into(),
                created_at: 1,
                started_at: Some(1),
                ended_at: None,
                last_activity_at: 1,
                last_seen_output_sequence: 0,
                exit_code: None,
                failure_reason: None,
            }],
            provider_profiles: Vec::new(),
        }
    }

    #[test]
    fn first_run_opens_the_local_workspace_modal() {
        let app = App::new(DaemonSnapshot::default());
        let Mode::Input(modal) = app.mode else {
            panic!("first run must request a workspace");
        };
        assert!(matches!(modal.action, InputAction::CreateWorkspace));
        assert_eq!(modal.values, ["Local"]);
    }

    #[test]
    fn contextual_create_and_rename_modals_target_selected_entities() {
        let mut app = App::new(populated_snapshot());

        open_create_modal(&mut app);
        assert!(matches!(
            app.mode,
            Mode::Input(InputModal {
                action: InputAction::RegisterProject(_),
                ..
            })
        ));

        app.panel = 1;
        open_create_modal(&mut app);
        assert!(matches!(
            app.mode,
            Mode::Input(InputModal {
                action: InputAction::CreateWorktree(_),
                ..
            })
        ));
        open_rename_modal(&mut app);
        assert!(matches!(
            app.mode,
            Mode::Input(InputModal {
                action: InputAction::RenameWorktree(_),
                ..
            })
        ));

        app.panel = 2;
        open_create_modal(&mut app);
        assert!(matches!(
            app.mode,
            Mode::Input(InputModal {
                action: InputAction::CreateSession(_),
                ..
            })
        ));
        open_rename_modal(&mut app);
        assert!(matches!(
            app.mode,
            Mode::Input(InputModal {
                action: InputAction::RenameSession(_),
                ..
            })
        ));
    }

    #[test]
    fn narrow_first_run_layout_renders_without_panicking() {
        let backend = ratatui::backend::TestBackend::new(24, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        let app = App::new(DaemonSnapshot::default());
        terminal.draw(|frame| draw(frame, &app)).unwrap();
    }

    #[test]
    fn attention_order_matches_product_contract() {
        assert!(
            attention_priority(SessionState::NeedsFeedback)
                < attention_priority(SessionState::Failed)
        );
        assert!(
            attention_priority(SessionState::Failed)
                < attention_priority(SessionState::FinishedUnseen)
        );
        assert!(
            attention_priority(SessionState::FinishedUnseen)
                < attention_priority(SessionState::Running)
        );
    }

    #[test]
    fn color_never_replaces_status_text() {
        assert_eq!(status_symbol(SessionState::Disconnected), "—");
        assert_eq!(status_label(SessionState::Disconnected), "Disconnected");
    }

    #[test]
    fn terminal_detach_key_is_not_forwarded() {
        assert!(is_detach_key(KeyEvent::new(
            KeyCode::Char(']'),
            KeyModifiers::CONTROL
        )));
        assert!(!is_detach_key(KeyEvent::new(
            KeyCode::Esc,
            KeyModifiers::NONE
        )));
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Some(vec![0x1b])
        );
    }
}
