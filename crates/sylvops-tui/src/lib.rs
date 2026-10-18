//! Hierarchical, keyboard-and-mouse mission control backed exclusively by daemon IPC.

mod app;
mod directory_browser;
mod forms;
mod github_modal;
mod input;
mod input_pump;
mod notifications;
mod reducer;
mod removal_notice;
mod render;
mod terminal;
mod theme;

use std::{
    io::stdout,
    path::{Path, PathBuf},
    time::Duration,
};

use app::{
    App, ConfirmAction, Confirmation, ExplorerNode, Flash, FlashKind, HitTarget, Mode,
    SettingsFocus,
};
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
        MouseButton, MouseEventKind,
    },
    execute,
    style::Print,
    terminal::{
        BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
        disable_raw_mode, enable_raw_mode,
    },
};
use directory_browser::{DirectoryBrowser, DirectoryBrowserEntry};
use forms::{Form, FormKind};
use futures_util::StreamExt as _;
use github_modal::{
    GitHubDepth, GitHubEscape, GitHubModal, GitHubQuery, GitHubRequestTarget, StartWorkFocus,
    StartWorkRecovery,
};
use input_pump::InputPump;
use ratatui::{Terminal, backend::CrosstermBackend, layout::Rect};
use reducer::{Effect, reduce};
use sylvops_core::{
    domain::{AttachmentRole, DaemonSnapshot},
    github::{
        GitHubItemIdentity, GitHubItemKind, GitHubItemNumber, StartWorkFailure,
        StartWorkFailureKind, StartWorkOutcome,
    },
    ids::{ProjectId, SessionId, WorkspaceId, WorktreeId},
    protocol::{
        ClientRequest, DaemonEvent, DaemonResponse, MAX_TERMINAL_COLUMNS, MAX_TERMINAL_ROWS,
        MIN_TERMINAL_COLUMNS, MIN_TERMINAL_ROWS, ProjectRemovalRefusal, SessionDeletionRefusal,
        WorkspaceDeletionRefusal,
    },
    session_state::{ProviderActivity, SessionLifecycle, lifecycle_is_active},
    ui::{TuiContentView, TuiDensity, TuiSessionPresentation, TuiState, TuiTheme},
    upgrade::UpgradeStatus,
};
use sylvops_daemon::{DaemonError, client::DaemonClient, runtime::RuntimePaths};
use terminal::{
    AttachedTerminal, TERMINAL_SCROLLBACK_ROWS, TerminalWheelAction, encode_key, history_action,
    is_detach_key,
};
use theme::terminal_background_rgb;

const TICK: Duration = Duration::from_millis(60);
const SAVE_DEBOUNCE: Duration = Duration::from_millis(500);
pub(crate) const MAX_FORM_VALUE_BYTES: usize = 8 * 1024;

pub(crate) struct FormOutcome {
    message: String,
    selection: Option<ExplorerNode>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TuiExit {
    Quit,
}

fn event_requires_snapshot_refresh(event: &DaemonEvent) -> bool {
    !matches!(
        event,
        DaemonEvent::SessionOutput { .. }
            | DaemonEvent::ResynchronizationRequired { .. }
            | DaemonEvent::GitHubAssociationUpdated { .. }
    )
}

#[derive(Clone, Debug)]
pub(crate) enum PaletteItem {
    Settings,
    CreateWorkspace,
    RegisterProject,
    Workspace(WorkspaceId, String),
    Project(ProjectId, String),
    Worktree(WorktreeId, String),
    Session(SessionId, String),
}

impl PaletteItem {
    pub(crate) fn label(&self) -> String {
        match self {
            Self::Settings => "Command: open settings".into(),
            Self::CreateWorkspace => "Command: create workspace".into(),
            Self::RegisterProject => "Command: register repository".into(),
            Self::Workspace(_, name) => format!("Workspace: {name}"),
            Self::Project(_, name) => format!("Project: {name}"),
            Self::Worktree(_, name) => format!("Worktree: {name}"),
            Self::Session(_, name) => format!("Session: {name}"),
        }
    }
}

/// Opens mission control. The daemon remains authoritative for processes and Git mutations.
///
/// # Errors
/// Returns an IPC, terminal initialization, rendering, input, or daemon-operation error.
#[allow(clippy::too_many_lines)]
pub async fn run(paths: &RuntimePaths) -> Result<TuiExit, DaemonError> {
    let invocation_directory = std::env::current_dir().map_err(|error| {
        DaemonError::Lifecycle(format!("cannot capture TUI invocation directory: {error}"))
    })?;
    let client = DaemonClient::connect(paths, "sylvops-tui").await?;
    let mut app = App::new_at(
        snapshot(&client).await?,
        list_providers(&client).await?,
        load_tui_state(&client).await?,
        invocation_directory,
    );
    let mut events = client.subscribe();
    let (github_results_tx, mut github_results) = tokio::sync::mpsc::channel(16);
    let mut github_tasks = GitHubTasks::default();
    let mut attention_alerts = notifications::AttentionTracker::new(&app.snapshot.sessions);
    #[cfg(windows)]
    let popup_notifications = notifications::platform::Notifier::new().ok();
    let mut guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))
        .map_err(|error| DaemonError::Lifecycle(format!("cannot initialize TUI: {error}")))?;
    let mut inputs = event::EventStream::new();
    let mut pending_event = None;
    let mut input_pump: Option<InputPump> = None;
    let mut input_failed = false;
    let mut maintenance = tokio::time::interval(TICK);
    maintenance.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut dirty = true;
    let mut animation_frame = app.status_animation_frame();

    loop {
        let controller = app
            .attached
            .as_ref()
            .filter(|attached| attached.role == AttachmentRole::Controller)
            .map(|attached| attached.session_id);
        if input_pump.as_ref().map(|pump| pump.session_id) != controller {
            input_pump = controller.map(|id| InputPump::new(client.clone(), id));
            input_failed = false;
        }
        guard.sync_theme(app.theme);
        let had_flash = app.flash.is_some();
        app.expire_flash();
        dirty |= had_flash != app.flash.is_some();
        let next_animation_frame = app.status_animation_frame();
        dirty |= next_animation_frame != animation_frame
            && app.snapshot.sessions.iter().any(|session| {
                session.status.lifecycle == SessionLifecycle::Starting
                    || (session.status.lifecycle == SessionLifecycle::Running
                        && matches!(
                            session.status.activity,
                            ProviderActivity::None | ProviderActivity::Working
                        ))
            });
        animation_frame = next_animation_frame;
        for _ in 0..16 {
            match github_results.try_recv() {
                Ok(result) => {
                    apply_github_result_async(&client, &mut app, result).await?;
                    dirty = true;
                }
                Err(
                    tokio::sync::mpsc::error::TryRecvError::Empty
                    | tokio::sync::mpsc::error::TryRecvError::Disconnected,
                ) => break,
            }
        }
        let mut refresh = false;
        for _ in 0..512 {
            match pending_event.take().unwrap_or_else(|| events.try_recv()) {
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
                        dirty = true;
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
                        attached.parser =
                            vt100::Parser::new(rows, columns, TERMINAL_SCROLLBACK_ROWS);
                        attached.parser.process(&terminal_snapshot);
                        attached.last_sequence = snapshot_sequence;
                        attached.columns = columns;
                        attached.rows = rows;
                        dirty = true;
                    }
                }
                Ok(DaemonEvent::UpgradeProgress {
                    status: UpgradeStatus::Installing { .. },
                }) => return Ok(TuiExit::Quit),
                Ok(DaemonEvent::GitHubAssociationUpdated {
                    project_id,
                    association,
                    ..
                }) => {
                    if app.selected_project_id == Some(project_id)
                        && let Some(status) = &mut app.github_status
                    {
                        status.association = association;
                        dirty = true;
                    }
                }
                Ok(event) => refresh |= event_requires_snapshot_refresh(&event),
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
            app.replace_snapshot(snapshot(&client).await?);
            if let Some(message) = attention_alerts.update(&app.snapshot.sessions) {
                let _ = execute!(terminal.backend_mut(), Print("\x07"));
                #[cfg(windows)]
                if !popup_notifications
                    .as_ref()
                    .is_some_and(|notifier| notifier.send(message.clone()))
                {
                    app.flash = Some(Flash::error(
                        "Windows popup notification unavailable; check the highlighted sessions.",
                    ));
                }
                #[cfg(not(windows))]
                let _ = message;
            }
            dirty = true;
        }
        save_debounced(&client, &mut app).await?;
        if matches!(app.mode, Mode::TerminalAttached) {
            let size = terminal
                .size()
                .map_err(|error| DaemonError::Lifecycle(error.to_string()))?;
            let focus = app.focus;
            let navigator_width = app.navigator_width;
            let attention_visible = app.attention_count() > 0;
            resize_attached(
                &client,
                &mut app,
                render::terminal_panel_area(
                    Rect::new(0, 0, size.width, size.height),
                    focus,
                    navigator_width,
                    attention_visible,
                ),
            )
            .await?;
        }
        if dirty {
            draw_synchronized(&mut terminal, |frame| render::draw(frame, &mut app))
                .map_err(|error| DaemonError::Lifecycle(format!("cannot draw TUI: {error}")))?;
            dirty = false;
        }
        let input = tokio::select! {
            event = inputs.next() => event.ok_or_else(|| DaemonError::Lifecycle("terminal input stream closed".into()))?
                .map_err(|error| DaemonError::Lifecycle(error.to_string()))?,
            event = events.recv() => {
                pending_event = Some(event.map_err(|error| match error {
                    tokio::sync::broadcast::error::RecvError::Lagged(count) => tokio::sync::broadcast::error::TryRecvError::Lagged(count),
                    tokio::sync::broadcast::error::RecvError::Closed => tokio::sync::broadcast::error::TryRecvError::Closed,
                }));
                continue;
            },
            Some(result) = github_results.recv() => {
                apply_github_result_async(&client, &mut app, result).await?;
                dirty = true;
                continue;
            },
            result = async {
                match app.resume_response.as_mut() {
                    Some(result) => result.await,
                    None => std::future::pending().await,
                }
            } => {
                app.resume_response = None;
                let result = result.unwrap_or_else(|_| Err(DaemonError::Lifecycle("Session resume request stopped.".into())));
                finish_resume(&client, &mut app, result).await?;
                dirty = true;
                continue;
            },
            result = async {
                match app.pending_form.as_mut() {
                    Some(result) => result.await,
                    None => std::future::pending().await,
                }
            } => {
                app.pending_form = None;
                let result = result.unwrap_or_else(|_| Err(DaemonError::Lifecycle("Worktree creation request stopped.".into())));
                finish_form_submission(&client, &mut app, result).await?;
                dirty = true;
                continue;
            },
            failure = async {
                match input_pump.as_mut() {
                    Some(pump) => (&mut pump.failure).await,
                    None => std::future::pending().await,
                }
            }, if !input_failed => {
                input_failed = true;
                app.flash = Some(Flash::error(failure.unwrap_or_else(|_| "Terminal input worker stopped.".into())));
                dirty = true;
                continue;
            },
            _ = maintenance.tick() => continue,
        };
        if let Event::Key(key) = &input
            && key.kind == KeyEventKind::Release
        {
            continue;
        }
        dirty = true;
        let quit = match app.mode {
            Mode::Navigation => {
                handle_navigation(
                    &client,
                    &github_results_tx,
                    &mut github_tasks,
                    &mut app,
                    input,
                )
                .await?
            }
            Mode::Modal(_) => {
                handle_form(&client, &mut app, input).await?;
                false
            }
            Mode::DirectoryBrowser(_) => {
                handle_directory_browser(&mut app, input).await;
                false
            }
            Mode::Confirmation(_) => {
                handle_confirmation(&client, &mut app, input).await?;
                false
            }
            Mode::RemovalBlocked(_) => {
                removal_notice::handle_input(&mut app, &input);
                false
            }
            Mode::CommandPalette(_) => {
                handle_palette(&client, &mut app, input).await?;
                false
            }
            Mode::Settings(_) => {
                handle_settings(&client, &mut app, &input).await?;
                false
            }
            Mode::GitHub(_) => {
                handle_github(
                    &client,
                    &github_results_tx,
                    &mut github_tasks,
                    &mut app,
                    &input,
                )
                .await?;
                false
            }
            Mode::TerminalAttached => {
                handle_terminal(&client, &mut app, &mut input_pump, input).await?;
                false
            }
        };
        if quit {
            save_tui_state(&client, &app).await?;
            return Ok(TuiExit::Quit);
        }
    }
}

fn draw_synchronized<W: std::io::Write>(
    terminal: &mut Terminal<CrosstermBackend<W>>,
    render: impl FnOnce(&mut ratatui::Frame<'_>),
) -> std::io::Result<()> {
    execute!(terminal.backend_mut(), BeginSynchronizedUpdate)?;
    let result = terminal.draw(render).map(|_| ());
    // Close synchronization even if drawing failed, so the host never freezes the frame.
    let end = execute!(terminal.backend_mut(), EndSynchronizedUpdate);
    result.and(end)
}

async fn handle_navigation(
    client: &DaemonClient,
    github_results: &tokio::sync::mpsc::Sender<GitHubAsyncResult>,
    github_tasks: &mut GitHubTasks,
    app: &mut App,
    event: Event,
) -> Result<bool, DaemonError> {
    if app.help {
        if matches!(event, Event::Key(key) if matches!(key.code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char('?')))
        {
            app.help = false;
        }
        return Ok(false);
    }
    let Some(action) = input::navigation_action(app, &event) else {
        return Ok(false);
    };
    let effects = reduce(app, action);
    execute_effects(client, github_results, github_tasks, app, effects).await
}

async fn execute_effects(
    client: &DaemonClient,
    github_results: &tokio::sync::mpsc::Sender<GitHubAsyncResult>,
    github_tasks: &mut GitHubTasks,
    app: &mut App,
    effects: Vec<Effect>,
) -> Result<bool, DaemonError> {
    for effect in effects {
        match effect {
            Effect::Attach => attach_selected(client, app).await?,
            Effect::OpenAttention {
                session_id,
                attention_id,
                reason,
            } => open_attention(client, app, session_id, attention_id, reason).await?,
            Effect::DismissAttention {
                session_id,
                attention_id,
            } => dismiss_attention(client, app, session_id, attention_id).await?,
            Effect::Resume(session_id) => resume_session(client, app, session_id),
            Effect::OpenCreate => open_create_form(app),
            Effect::OpenCreateWorktree(project_id) => {
                app.mode = Mode::Modal(Form::worktree(project_id));
            }
            Effect::OpenRename => open_rename_form(app),
            Effect::OpenDelete => open_destructive_confirmation(client, app).await?,
            Effect::LoadDiff => show_diff(client, app).await?,
            Effect::LoadGitHubStatus => refresh_github_status(client, app).await?,
            Effect::OpenGitHub => {
                refresh_github_status(client, app).await?;
                open_github(client, github_results, github_tasks, app);
            }
            Effect::Quit => return Ok(true),
        }
    }
    Ok(false)
}

async fn dismiss_attention(
    client: &DaemonClient,
    app: &mut App,
    session_id: SessionId,
    attention_id: sylvops_core::session_state::AttentionEventId,
) -> Result<(), DaemonError> {
    match client
        .request(&ClientRequest::DismissAttention {
            session_id,
            attention_id,
        })
        .await?
    {
        DaemonResponse::SessionAttentionDismissed { revision, session } => {
            replace_session_snapshot(app, revision, session);
            app.flash = Some(Flash::transient(
                FlashKind::Success,
                "Recoverable failure dismissed.",
            ));
        }
        response => {
            app.flash = Some(Flash::error(response_message(
                "attention dismissal",
                response,
            )));
        }
    }
    Ok(())
}

async fn open_attention(
    client: &DaemonClient,
    app: &mut App,
    session_id: SessionId,
    attention_id: sylvops_core::session_state::AttentionEventId,
    reason: sylvops_core::session_state::AttentionReason,
) -> Result<(), DaemonError> {
    use sylvops_core::session_state::{AttentionReason, SessionLifecycle};

    app.reveal_and_select(ExplorerNode::Session(session_id));
    if matches!(
        reason,
        AttentionReason::InputRequested | AttentionReason::RecoverableFailure
    ) {
        return attach_selected_process(client, app).await;
    }
    let live = app
        .selected_session()
        .is_some_and(|session| session.status.lifecycle == SessionLifecycle::Running);
    match client
        .request(&ClientRequest::AcknowledgeAttention {
            session_id,
            attention_id,
        })
        .await?
    {
        DaemonResponse::SessionAttentionAcknowledged { revision, session } => {
            replace_session_snapshot(app, revision, session);
            if live {
                attach_selected_process(client, app).await?;
            } else {
                app.set_tab(TuiContentView::Details);
                app.flash = Some(Flash::transient(
                    FlashKind::Success,
                    "Session history opened and attention acknowledged.",
                ));
            }
        }
        response => {
            app.flash = Some(Flash::error(response_message(
                "attention acknowledgement",
                response,
            )));
        }
    }
    Ok(())
}

fn replace_session_snapshot(app: &mut App, revision: u64, session: sylvops_core::domain::Session) {
    let mut next = app.snapshot.clone();
    next.revision = revision;
    if let Some(existing) = next
        .sessions
        .iter_mut()
        .find(|existing| existing.id == session.id)
    {
        *existing = session;
    }
    app.replace_snapshot(next);
}

#[derive(Debug, Eq, PartialEq)]
enum GitHubDecision {
    None,
    List(sylvops_core::github::GitHubListQuery),
    Detail {
        query: sylvops_core::github::GitHubViewQuery,
        append_comments: bool,
    },
    Browser(GitHubItemIdentity),
    PrepareStartWork {
        issue_number: GitHubItemNumber,
        start_another_worktree: bool,
    },
    StartWork(ClientRequest),
    OpenWorktree(WorktreeId),
    RetrySession {
        worktree_id: WorktreeId,
        provider: sylvops_core::domain::ProviderKind,
    },
    RemoveWorktree(WorktreeId),
    Settings,
    Close,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GitHubAction {
    Issues,
    PullRequests,
    Move(isize),
    Select(usize),
    OpenDetail,
    Refresh,
    Filter,
    OpenBrowser,
    PreviousPage,
    NextPage,
    MoreComments,
    StartWork,
    StartAnother,
    SubmitStartWork,
    RetrySession,
    OpenWorktree,
    RemoveWorktree,
    Settings,
    Back,
}

fn github_input_decision(
    modal: &mut GitHubModal,
    input: &Event,
    clicked: Option<HitTarget>,
) -> GitHubDecision {
    if modal.filter_editing {
        if let Event::Key(key) = input {
            match key.code {
                KeyCode::Esc => {
                    modal.escape();
                }
                KeyCode::Enter => match modal.apply_filter() {
                    Ok(query) => return GitHubDecision::List(query),
                    Err(error) => modal.set_filter_error(error),
                },
                KeyCode::Backspace => {
                    modal.filter_input.pop();
                    modal.filter_error = None;
                }
                KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    push_bounded(&mut modal.filter_input, &character.to_string());
                    modal.filter_error = None;
                }
                _ => {}
            }
        }
        return GitHubDecision::None;
    }

    if modal.depth == GitHubDepth::StartWork {
        return start_work_input_decision(modal, input, clicked);
    }

    if modal.depth == GitHubDepth::Recovery {
        return recovery_input_decision(modal, input, clicked);
    }

    let action = clicked
        .and_then(github_mouse_action)
        .or_else(|| github_keyboard_action(input));
    action.map_or(GitHubDecision::None, |action| {
        apply_github_action(modal, action)
    })
}

fn start_work_input_decision(
    modal: &mut GitHubModal,
    input: &Event,
    clicked: Option<HitTarget>,
) -> GitHubDecision {
    let escape_requested = clicked == Some(HitTarget::GitHubBack)
        || matches!(input, Event::Key(key) if key.code == KeyCode::Esc);
    if escape_requested && modal.start_work.as_ref().is_some_and(|form| form.pending) {
        return GitHubDecision::None;
    }
    if escape_requested {
        modal.escape();
        return GitHubDecision::None;
    }
    let activate = clicked == Some(HitTarget::GitHubStartSubmit);
    let key = match input {
        Event::Key(key) => Some(key),
        _ => None,
    };
    let project_id = modal.project_id;
    if modal.start_work_error.is_some()
        && modal.start_work.is_none()
        && key.is_some_and(|key| matches!(key.code, KeyCode::Char('l' | 'r')))
        && let Some(identity) = modal.selected_identity()
    {
        modal.start_work_initial_base = key
            .is_some_and(|key| key.code == KeyCode::Char('l'))
            .then(|| "HEAD".into());
        return GitHubDecision::PrepareStartWork {
            issue_number: identity.number,
            start_another_worktree: true,
        };
    }
    let Some(form) = &mut modal.start_work else {
        return GitHubDecision::None;
    };
    if form.pending {
        return GitHubDecision::None;
    }
    if clicked == Some(HitTarget::GitHubRefreshBase)
        || key.is_some_and(|key| {
            key.code == KeyCode::Char('r') && key.modifiers.contains(KeyModifiers::CONTROL)
        })
    {
        return GitHubDecision::PrepareStartWork {
            issue_number: form.preparation.issue.identity.number,
            start_another_worktree: form.start_another_worktree,
        };
    }
    if clicked == Some(HitTarget::GitHubCycleBase)
        || key.is_some_and(|key| {
            key.code == KeyCode::Char('b') && key.modifiers.contains(KeyModifiers::CONTROL)
        })
    {
        form.cycle_base();
        return GitHubDecision::None;
    }
    if let Some(HitTarget::GitHubScroll(delta)) = clicked {
        form.scroll_context(delta);
        return GitHubDecision::None;
    }
    if activate || key.is_some_and(|key| key.code == KeyCode::Enter) {
        if form.focus != StartWorkFocus::Confirm {
            form.focus = form.focus.moved(1);
            return GitHubDecision::None;
        }
        return match form.request(project_id) {
            Ok(request) => {
                form.pending = true;
                GitHubDecision::StartWork(request)
            }
            Err(error) => {
                form.error = Some(StartWorkFailure::new(
                    StartWorkFailureKind::InvalidRequest,
                    error,
                ));
                GitHubDecision::None
            }
        };
    }
    let Some(key) = key else {
        return GitHubDecision::None;
    };
    match key.code {
        KeyCode::Tab => form.focus = form.focus.moved(1),
        KeyCode::BackTab => form.focus = form.focus.moved(-1),
        KeyCode::Down | KeyCode::Char('j') if form.focus == StartWorkFocus::Provider => {
            form.move_provider(1);
        }
        KeyCode::Up | KeyCode::Char('k') if form.focus == StartWorkFocus::Provider => {
            form.move_provider(-1);
        }
        KeyCode::Backspace => form.pop_character(),
        KeyCode::PageDown => form.scroll_context(5),
        KeyCode::PageUp => form.scroll_context(-5),
        KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            form.push_character(character);
        }
        _ => {}
    }
    GitHubDecision::None
}

fn recovery_input_decision(
    modal: &mut GitHubModal,
    input: &Event,
    clicked: Option<HitTarget>,
) -> GitHubDecision {
    if modal
        .recovery
        .as_ref()
        .is_some_and(|recovery| recovery.pending)
    {
        return GitHubDecision::None;
    }
    if let Some(action) = clicked.and_then(github_mouse_action) {
        return apply_github_action(modal, action);
    }
    let Event::Key(key) = input else {
        return GitHubDecision::None;
    };
    let Some(recovery) = &modal.recovery else {
        return GitHubDecision::None;
    };
    match key.code {
        KeyCode::Char('s') => GitHubDecision::RetrySession {
            worktree_id: recovery.worktree_id,
            provider: recovery.provider,
        },
        KeyCode::Char('o') => GitHubDecision::OpenWorktree(recovery.worktree_id),
        KeyCode::Char('d') => GitHubDecision::RemoveWorktree(recovery.worktree_id),
        KeyCode::Esc => github_escape_decision(modal),
        _ => GitHubDecision::None,
    }
}

fn github_mouse_action(target: HitTarget) -> Option<GitHubAction> {
    match target {
        HitTarget::GitHubIssues => Some(GitHubAction::Issues),
        HitTarget::GitHubPullRequests => Some(GitHubAction::PullRequests),
        HitTarget::GitHubRow(index) => Some(GitHubAction::Select(index)),
        HitTarget::GitHubFilter => Some(GitHubAction::Filter),
        HitTarget::GitHubRefresh => Some(GitHubAction::Refresh),
        HitTarget::GitHubOpen => Some(GitHubAction::OpenBrowser),
        HitTarget::GitHubPrevious => Some(GitHubAction::PreviousPage),
        HitTarget::GitHubNext => Some(GitHubAction::NextPage),
        HitTarget::GitHubMoreComments => Some(GitHubAction::MoreComments),
        HitTarget::GitHubStartWork => Some(GitHubAction::StartWork),
        HitTarget::GitHubStartAnother => Some(GitHubAction::StartAnother),
        HitTarget::GitHubStartSubmit => Some(GitHubAction::SubmitStartWork),
        HitTarget::GitHubRetrySession => Some(GitHubAction::RetrySession),
        HitTarget::GitHubOpenWorktree => Some(GitHubAction::OpenWorktree),
        HitTarget::GitHubRemoveWorktree => Some(GitHubAction::RemoveWorktree),
        HitTarget::GitHubScroll(delta) => Some(GitHubAction::Move(delta)),
        HitTarget::GitHubBack => Some(GitHubAction::Back),
        HitTarget::Settings => Some(GitHubAction::Settings),
        _ => None,
    }
}

fn github_keyboard_action(input: &Event) -> Option<GitHubAction> {
    let Event::Key(key) = input else {
        return None;
    };
    match key.code {
        KeyCode::Char('i') => Some(GitHubAction::Issues),
        KeyCode::Char('p') => Some(GitHubAction::PullRequests),
        KeyCode::Down | KeyCode::Char('j') => Some(GitHubAction::Move(1)),
        KeyCode::Up | KeyCode::Char('k') => Some(GitHubAction::Move(-1)),
        KeyCode::Enter => Some(GitHubAction::OpenDetail),
        KeyCode::Char('r') => Some(GitHubAction::Refresh),
        KeyCode::Char('/') => Some(GitHubAction::Filter),
        KeyCode::Char('o') => Some(GitHubAction::OpenBrowser),
        KeyCode::Char('n') => Some(GitHubAction::NextPage),
        KeyCode::Char('b') => Some(GitHubAction::PreviousPage),
        KeyCode::Char('c') => Some(GitHubAction::MoreComments),
        KeyCode::Char('s') => Some(GitHubAction::StartWork),
        KeyCode::Char('a') => Some(GitHubAction::StartAnother),
        KeyCode::Char('t') => Some(GitHubAction::Settings),
        KeyCode::Esc => Some(GitHubAction::Back),
        _ => None,
    }
}

#[allow(clippy::too_many_lines)]
fn apply_github_action(modal: &mut GitHubModal, action: GitHubAction) -> GitHubDecision {
    match action {
        GitHubAction::Issues => modal
            .switch_kind(GitHubItemKind::Issue)
            .map_or(GitHubDecision::None, GitHubDecision::List),
        GitHubAction::PullRequests => modal
            .switch_kind(GitHubItemKind::PullRequest)
            .map_or(GitHubDecision::None, GitHubDecision::List),
        GitHubAction::Move(delta) => {
            if modal.depth == GitHubDepth::List {
                modal.move_selection(delta);
            } else {
                modal.scroll_detail(delta);
            }
            GitHubDecision::None
        }
        GitHubAction::Select(index) if modal.depth == GitHubDepth::List => {
            modal.selected_index = index;
            apply_github_action(modal, GitHubAction::OpenDetail)
        }
        GitHubAction::OpenDetail if modal.depth == GitHubDepth::List => modal
            .open_selected()
            .map_or(GitHubDecision::None, |query| GitHubDecision::Detail {
                query,
                append_comments: false,
            }),
        GitHubAction::Refresh => modal
            .refresh()
            .map_or(GitHubDecision::None, github_query_decision),
        GitHubAction::Filter if modal.depth == GitHubDepth::List => {
            modal.start_filtering();
            GitHubDecision::None
        }
        GitHubAction::OpenBrowser => modal
            .begin_browser_open()
            .map_or(GitHubDecision::None, GitHubDecision::Browser),
        GitHubAction::NextPage if modal.depth == GitHubDepth::List => modal
            .next_page()
            .map_or(GitHubDecision::None, GitHubDecision::List),
        GitHubAction::PreviousPage if modal.depth == GitHubDepth::List => modal
            .previous_page()
            .map_or(GitHubDecision::None, GitHubDecision::List),
        GitHubAction::MoreComments if modal.depth == GitHubDepth::Detail => modal
            .load_more_comments()
            .map_or(GitHubDecision::None, |query| GitHubDecision::Detail {
                query,
                append_comments: true,
            }),
        GitHubAction::StartWork => {
            if let Some(worktree_id) = modal.existing_worktree() {
                GitHubDecision::OpenWorktree(worktree_id)
            } else {
                modal.begin_start_work(false).map_or(
                    GitHubDecision::None,
                    |(issue_number, start_another_worktree)| GitHubDecision::PrepareStartWork {
                        issue_number,
                        start_another_worktree,
                    },
                )
            }
        }
        GitHubAction::StartAnother => modal.begin_start_work(true).map_or(
            GitHubDecision::None,
            |(issue_number, start_another_worktree)| GitHubDecision::PrepareStartWork {
                issue_number,
                start_another_worktree,
            },
        ),
        GitHubAction::RetrySession if modal.depth == GitHubDepth::Recovery => modal
            .recovery
            .as_ref()
            .filter(|recovery| !recovery.pending)
            .map_or(GitHubDecision::None, |recovery| {
                GitHubDecision::RetrySession {
                    worktree_id: recovery.worktree_id,
                    provider: recovery.provider,
                }
            }),
        GitHubAction::OpenWorktree if modal.depth == GitHubDepth::Recovery => modal
            .recovery
            .as_ref()
            .map_or(GitHubDecision::None, |recovery| {
                GitHubDecision::OpenWorktree(recovery.worktree_id)
            }),
        GitHubAction::RemoveWorktree if modal.depth == GitHubDepth::Recovery => modal
            .recovery
            .as_ref()
            .map_or(GitHubDecision::None, |recovery| {
                GitHubDecision::RemoveWorktree(recovery.worktree_id)
            }),
        GitHubAction::Settings => GitHubDecision::Settings,
        GitHubAction::Back => github_escape_decision(modal),
        GitHubAction::OpenDetail
        | GitHubAction::Select(_)
        | GitHubAction::Filter
        | GitHubAction::PreviousPage
        | GitHubAction::NextPage
        | GitHubAction::MoreComments
        | GitHubAction::SubmitStartWork
        | GitHubAction::RetrySession
        | GitHubAction::OpenWorktree
        | GitHubAction::RemoveWorktree => GitHubDecision::None,
    }
}

fn github_query_decision(query: GitHubQuery) -> GitHubDecision {
    match query {
        GitHubQuery::List(query) => GitHubDecision::List(query),
        GitHubQuery::Detail(query) => GitHubDecision::Detail {
            query,
            append_comments: false,
        },
    }
}

fn github_escape_decision(modal: &mut GitHubModal) -> GitHubDecision {
    if modal.escape() == GitHubEscape::Close {
        GitHubDecision::Close
    } else {
        GitHubDecision::None
    }
}

#[derive(Debug)]
enum GitHubAsyncResult {
    List {
        request_id: u64,
        project_id: ProjectId,
        response: Result<DaemonResponse, String>,
    },
    Detail {
        request_id: u64,
        project_id: ProjectId,
        append_comments: bool,
        response: Result<DaemonResponse, String>,
    },
    Browser {
        request_id: u64,
        project_id: ProjectId,
        response: Result<DaemonResponse, String>,
    },
    PrepareStartWork {
        request_id: u64,
        project_id: ProjectId,
        start_another_worktree: bool,
        response: Result<DaemonResponse, String>,
    },
    StartWork {
        request_id: u64,
        project_id: ProjectId,
        provider: sylvops_core::domain::ProviderKind,
        response: Result<DaemonResponse, String>,
    },
    RetrySession {
        request_id: u64,
        project_id: ProjectId,
        response: Result<DaemonResponse, String>,
    },
}

#[derive(Default)]
struct GitHubTasks {
    next_request_id: u64,
    list: Option<tokio::task::AbortHandle>,
    detail: Option<tokio::task::AbortHandle>,
    browser: Option<tokio::task::AbortHandle>,
    start_work: Option<tokio::task::AbortHandle>,
}

impl GitHubTasks {
    fn next_request_id(&mut self) -> u64 {
        self.next_request_id = self.next_request_id.wrapping_add(1).max(1);
        self.next_request_id
    }

    fn replace_list(&mut self, handle: tokio::task::AbortHandle) {
        abort_task(&mut self.list);
        abort_task(&mut self.detail);
        self.list = Some(handle);
    }

    fn replace_detail(&mut self, handle: tokio::task::AbortHandle) {
        abort_task(&mut self.detail);
        self.detail = Some(handle);
    }

    fn replace_browser(&mut self, handle: tokio::task::AbortHandle) {
        abort_task(&mut self.browser);
        self.browser = Some(handle);
    }

    fn replace_start_work(&mut self, handle: tokio::task::AbortHandle) {
        abort_task(&mut self.start_work);
        self.start_work = Some(handle);
    }

    fn cancel_all(&mut self) {
        abort_task(&mut self.list);
        abort_task(&mut self.detail);
        abort_task(&mut self.browser);
        abort_task(&mut self.start_work);
    }
}

impl Drop for GitHubTasks {
    fn drop(&mut self) {
        self.cancel_all();
    }
}

fn abort_task(task: &mut Option<tokio::task::AbortHandle>) {
    if let Some(task) = task.take() {
        task.abort();
    }
}

fn open_github(
    client: &DaemonClient,
    results: &tokio::sync::mpsc::Sender<GitHubAsyncResult>,
    tasks: &mut GitHubTasks,
    app: &mut App,
) {
    let Some(project_id) = app.selected_project_id else {
        app.flash = Some(Flash::error("Select a Project before browsing GitHub."));
        return;
    };
    tasks.cancel_all();
    let (modal, query) = GitHubModal::open(project_id);
    let mut modal = modal;
    modal.sync_local_work(&app.snapshot);
    let source_identity = app
        .selected_worktree_id
        .and_then(|worktree_id| {
            app.snapshot
                .work_sources
                .iter()
                .find(|source| source.worktree_id == worktree_id)
        })
        .map(|source| source.item.clone());
    if let Some(identity) = source_identity {
        let repository_changed = app.github_status.as_ref().is_some_and(|status| {
            status
                .association
                .as_ref()
                .is_none_or(|association| association.repository != identity.repository)
        });
        let detail = modal.open_identity(&identity);
        if repository_changed {
            modal.mark_repository_changed(identity);
            app.mode = Mode::GitHub(Box::new(modal));
            return;
        }
        app.mode = Mode::GitHub(Box::new(modal));
        queue_github_detail(client, results, tasks, app, project_id, detail, false);
    } else {
        app.mode = Mode::GitHub(Box::new(modal));
        queue_github_list(client, results, tasks, app, project_id, query);
    }
}

async fn handle_github(
    client: &DaemonClient,
    results: &tokio::sync::mpsc::Sender<GitHubAsyncResult>,
    tasks: &mut GitHubTasks,
    app: &mut App,
    input: &Event,
) -> Result<(), DaemonError> {
    let clicked = if let Event::Mouse(mouse) = input
        && mouse.kind == MouseEventKind::Down(MouseButton::Left)
    {
        app.hit_map.target_at(mouse.column, mouse.row)
    } else {
        None
    };
    let Mode::GitHub(mut modal) = app.mode.clone() else {
        return Ok(());
    };
    let project_id = modal.project_id;
    let decision = github_input_decision(&mut modal, input, clicked);
    app.mode = if decision == GitHubDecision::Close {
        tasks.cancel_all();
        Mode::Navigation
    } else {
        Mode::GitHub(modal)
    };
    match decision {
        GitHubDecision::List(query) => {
            queue_github_list(client, results, tasks, app, project_id, query);
        }
        GitHubDecision::Detail {
            query,
            append_comments,
        } => queue_github_detail(
            client,
            results,
            tasks,
            app,
            project_id,
            query,
            append_comments,
        ),
        GitHubDecision::Browser(identity) => {
            queue_github_browser(client, results, tasks, app, project_id, identity);
        }
        GitHubDecision::PrepareStartWork {
            issue_number,
            start_another_worktree,
        } => queue_prepare_start_work(
            client,
            results,
            tasks,
            app,
            project_id,
            issue_number,
            start_another_worktree,
        ),
        GitHubDecision::StartWork(request) => {
            queue_start_work(client, results, tasks, app, project_id, request);
        }
        GitHubDecision::OpenWorktree(worktree_id) => {
            if open_github_worktree(app, worktree_id) {
                attach_selected(client, app).await?;
            }
        }
        GitHubDecision::RetrySession {
            worktree_id,
            provider,
        } => queue_retry_start_work_session(
            client,
            results,
            tasks,
            app,
            project_id,
            worktree_id,
            provider,
        ),
        GitHubDecision::RemoveWorktree(worktree_id) => {
            app.reveal_and_select(ExplorerNode::Worktree(worktree_id));
            open_destructive_confirmation(client, app).await?;
            apply_checkout_recovery_confirmation_copy(app, worktree_id);
        }
        GitHubDecision::Settings => {
            tasks.cancel_all();
            app.open_settings();
            refresh_github_status(client, app).await?;
        }
        GitHubDecision::None | GitHubDecision::Close => {}
    }
    Ok(())
}

fn apply_checkout_recovery_confirmation_copy(app: &mut App, worktree_id: WorktreeId) {
    let worktree = app
        .snapshot
        .worktrees
        .iter()
        .find(|worktree| worktree.id == worktree_id);
    let Some(worktree) = worktree else {
        return;
    };
    let Mode::Confirmation(confirmation) = &mut app.mode else {
        return;
    };
    if !matches!(
        confirmation.action,
        ConfirmAction::RemoveWorktree {
            worktree_id: candidate,
            ..
        } if candidate == worktree_id
    ) {
        return;
    }
    confirmation.title = "Delete checkout?".into();
    confirmation.detail = format!(
        "Delete checkout '{}' ({}) exactly? The checkout directory at {} is removed without force only after this clean-state confirmation; the Git branch is preserved.",
        worktree.name, worktree.id, worktree.canonical_path
    );
}

fn queue_github_list(
    client: &DaemonClient,
    results: &tokio::sync::mpsc::Sender<GitHubAsyncResult>,
    tasks: &mut GitHubTasks,
    app: &mut App,
    project_id: ProjectId,
    query: sylvops_core::github::GitHubListQuery,
) {
    let request_id = tasks.next_request_id();
    if let Mode::GitHub(modal) = &mut app.mode {
        modal.register_list_request(request_id);
    }
    let request = github_list_request(project_id, query);
    let handle = dispatch_github_request(client, results, request, move |response| {
        GitHubAsyncResult::List {
            request_id,
            project_id,
            response,
        }
    });
    tasks.replace_list(handle);
}

fn queue_github_detail(
    client: &DaemonClient,
    results: &tokio::sync::mpsc::Sender<GitHubAsyncResult>,
    tasks: &mut GitHubTasks,
    app: &mut App,
    project_id: ProjectId,
    query: sylvops_core::github::GitHubViewQuery,
    append_comments: bool,
) {
    let request_id = tasks.next_request_id();
    if let Mode::GitHub(modal) = &mut app.mode {
        modal.register_detail_request(request_id);
    }
    let request = github_detail_request(project_id, query);
    let handle = dispatch_github_request(client, results, request, move |response| {
        GitHubAsyncResult::Detail {
            request_id,
            project_id,
            append_comments,
            response,
        }
    });
    tasks.replace_detail(handle);
}

fn queue_github_browser(
    client: &DaemonClient,
    results: &tokio::sync::mpsc::Sender<GitHubAsyncResult>,
    tasks: &mut GitHubTasks,
    app: &mut App,
    project_id: ProjectId,
    identity: GitHubItemIdentity,
) {
    let request_id = tasks.next_request_id();
    if let Mode::GitHub(modal) = &mut app.mode {
        modal.register_browser_request(request_id);
    }
    let request = github_browser_request(project_id, identity);
    let handle = dispatch_github_request(client, results, request, move |response| {
        GitHubAsyncResult::Browser {
            request_id,
            project_id,
            response,
        }
    });
    tasks.replace_browser(handle);
}

fn queue_prepare_start_work(
    client: &DaemonClient,
    results: &tokio::sync::mpsc::Sender<GitHubAsyncResult>,
    tasks: &mut GitHubTasks,
    app: &mut App,
    project_id: ProjectId,
    issue_number: GitHubItemNumber,
    start_another_worktree: bool,
) {
    let request_id = tasks.next_request_id();
    let mut base = None;
    if let Mode::GitHub(modal) = &mut app.mode {
        base.clone_from(&modal.start_work_initial_base);
        if let Some(form) = &mut modal.start_work {
            match form.base_mode.selection(&form.base_ref) {
                Ok(selection) => {
                    base = Some(selection);
                    form.pending = true;
                }
                Err(error) => {
                    form.error = Some(StartWorkFailure::new(
                        StartWorkFailureKind::InvalidRequest,
                        error,
                    ));
                    return;
                }
            }
        }
        modal.register_start_work_request(request_id);
        modal.start_work_error = None;
        if modal.start_work.is_none() {
            modal.start_work_preparing = true;
        }
    }
    let request = ClientRequest::PrepareStartWork {
        project_id,
        issue_number,
        base,
    };
    let handle = dispatch_github_request(client, results, request, move |response| {
        GitHubAsyncResult::PrepareStartWork {
            request_id,
            project_id,
            start_another_worktree,
            response,
        }
    });
    tasks.replace_start_work(handle);
}

fn queue_start_work(
    client: &DaemonClient,
    results: &tokio::sync::mpsc::Sender<GitHubAsyncResult>,
    tasks: &mut GitHubTasks,
    app: &mut App,
    project_id: ProjectId,
    request: ClientRequest,
) {
    let provider = match &request {
        ClientRequest::StartWork { provider, .. } => *provider,
        _ => return,
    };
    let request_id = tasks.next_request_id();
    if let Mode::GitHub(modal) = &mut app.mode {
        modal.register_start_work_request(request_id);
    }
    let handle = dispatch_github_request(client, results, request, move |response| {
        GitHubAsyncResult::StartWork {
            request_id,
            project_id,
            provider,
            response,
        }
    });
    tasks.replace_start_work(handle);
}

fn queue_retry_start_work_session(
    client: &DaemonClient,
    results: &tokio::sync::mpsc::Sender<GitHubAsyncResult>,
    tasks: &mut GitHubTasks,
    app: &mut App,
    project_id: ProjectId,
    worktree_id: WorktreeId,
    provider: sylvops_core::domain::ProviderKind,
) {
    let session_name = if let Mode::GitHub(modal) = &mut app.mode {
        let Some(recovery) = &mut modal.recovery else {
            return;
        };
        recovery.pending = true;
        recovery.session_name.clone()
    } else {
        return;
    };
    let request_id = tasks.next_request_id();
    if let Mode::GitHub(modal) = &mut app.mode {
        modal.register_start_work_request(request_id);
    }
    let request = ClientRequest::RetryStartWorkSession {
        worktree_id,
        provider,
        display_name: (!session_name.is_empty()).then_some(session_name.clone()),
        columns: 80,
        rows: 24,
    };
    let handle = dispatch_github_request(client, results, request, move |response| {
        GitHubAsyncResult::RetrySession {
            request_id,
            project_id,
            response,
        }
    });
    tasks.replace_start_work(handle);
}

fn dispatch_github_request(
    client: &DaemonClient,
    results: &tokio::sync::mpsc::Sender<GitHubAsyncResult>,
    request: ClientRequest,
    result: impl FnOnce(Result<DaemonResponse, String>) -> GitHubAsyncResult + Send + 'static,
) -> tokio::task::AbortHandle {
    let client = client.clone();
    let results = results.clone();
    tokio::spawn(async move {
        let response = client
            .request(&request)
            .await
            .map_err(|error| error.to_string());
        let _ = results.send(result(response)).await;
    })
    .abort_handle()
}

fn github_list_request(
    project_id: ProjectId,
    query: sylvops_core::github::GitHubListQuery,
) -> ClientRequest {
    ClientRequest::ListGitHubItems { project_id, query }
}

fn github_detail_request(
    project_id: ProjectId,
    query: sylvops_core::github::GitHubViewQuery,
) -> ClientRequest {
    ClientRequest::ViewGitHubItem { project_id, query }
}

fn github_browser_request(project_id: ProjectId, identity: GitHubItemIdentity) -> ClientRequest {
    ClientRequest::OpenGitHubItem {
        project_id,
        identity,
    }
}

#[derive(Debug, Eq, PartialEq)]
enum GitHubResultEffect {
    None,
    Attach { session_id: SessionId },
    OpenWorktree(WorktreeId),
}

#[allow(clippy::too_many_lines)]
fn apply_github_result(
    app: &mut App,
    result: GitHubAsyncResult,
) -> Result<GitHubResultEffect, DaemonError> {
    let accepted = match (&app.mode, &result) {
        (
            Mode::GitHub(modal),
            GitHubAsyncResult::List {
                request_id,
                project_id,
                ..
            },
        ) => modal.project_id == *project_id && modal.accepts_list(*request_id),
        (
            Mode::GitHub(modal),
            GitHubAsyncResult::Detail {
                request_id,
                project_id,
                ..
            },
        ) => modal.project_id == *project_id && modal.accepts_detail(*request_id),
        (
            Mode::GitHub(modal),
            GitHubAsyncResult::Browser {
                request_id,
                project_id,
                ..
            },
        ) => modal.project_id == *project_id && modal.accepts_browser(*request_id),
        (
            Mode::GitHub(modal),
            GitHubAsyncResult::PrepareStartWork {
                request_id,
                project_id,
                ..
            }
            | GitHubAsyncResult::StartWork {
                request_id,
                project_id,
                ..
            }
            | GitHubAsyncResult::RetrySession {
                request_id,
                project_id,
                ..
            },
        ) => modal.project_id == *project_id && modal.accepts_start_work(*request_id),
        _ => false,
    };
    if !accepted {
        return Ok(GitHubResultEffect::None);
    }
    match result {
        GitHubAsyncResult::List { response, .. } => {
            let Mode::GitHub(modal) = &mut app.mode else {
                unreachable!("accepted GitHub list result without a modal")
            };
            apply_github_list_response(modal, response)?;
        }
        GitHubAsyncResult::Detail {
            append_comments,
            response,
            ..
        } => {
            let Mode::GitHub(modal) = &mut app.mode else {
                unreachable!("accepted GitHub detail result without a modal")
            };
            apply_github_detail_response(modal, append_comments, response)?;
        }
        GitHubAsyncResult::Browser { response, .. } => {
            let Mode::GitHub(modal) = &mut app.mode else {
                unreachable!("accepted GitHub browser result without a modal")
            };
            apply_github_browser_response(modal, response)?;
        }
        GitHubAsyncResult::PrepareStartWork {
            start_another_worktree,
            response,
            ..
        } => {
            let Mode::GitHub(modal) = &mut app.mode else {
                unreachable!("accepted Start Work preparation without a modal")
            };
            match response {
                Ok(DaemonResponse::StartWorkPrepared(preparation)) => {
                    if !start_another_worktree
                        && let Some(source) = preparation
                            .existing_work
                            .iter()
                            .max_by_key(|source| source.created_at)
                    {
                        return Ok(GitHubResultEffect::OpenWorktree(source.worktree_id));
                    }
                    modal.receive_start_work_preparation(preparation, start_another_worktree);
                }
                Ok(DaemonResponse::StartWorkFailure(failure)) => {
                    modal.fail_start_work(failure);
                }
                Ok(DaemonResponse::Error(error)) => modal.fail_start_work(StartWorkFailure::new(
                    StartWorkFailureKind::InvalidRequest,
                    bounded_feedback(error.message),
                )),
                Err(error) => modal.fail_start_work(StartWorkFailure::new(
                    StartWorkFailureKind::InvalidRequest,
                    bounded_feedback(error),
                )),
                Ok(response) => {
                    return Err(DaemonError::Lifecycle(format!(
                        "unexpected Start Work preparation response: {response:?}"
                    )));
                }
            }
        }
        GitHubAsyncResult::StartWork {
            provider, response, ..
        } => match response {
            Ok(DaemonResponse::StartWorkCompleted(StartWorkOutcome::Started {
                worktree,
                source,
                session,
                ..
            })) => return Ok(apply_started_work(app, worktree, source, *session)),
            Ok(DaemonResponse::StartWorkCompleted(StartWorkOutcome::WorktreeCreated {
                worktree,
                source,
                failure,
                ..
            })) => {
                let worktree_id = worktree.id;
                let branch = worktree.branch.clone().unwrap_or_else(|| "detached".into());
                let session_name = match &app.mode {
                    Mode::GitHub(modal) => modal
                        .start_work
                        .as_ref()
                        .map_or_else(String::new, |form| form.session_name.trim().to_owned()),
                    _ => String::new(),
                };
                upsert_start_work_state(app, worktree, source, None);
                let Mode::GitHub(modal) = &mut app.mode else {
                    unreachable!("partial Start Work result lost its modal")
                };
                modal.begin_recovery(StartWorkRecovery {
                    worktree_id,
                    branch,
                    provider,
                    session_name,
                    failure,
                    pending: false,
                });
            }
            Ok(DaemonResponse::StartWorkFailure(failure)) => {
                let Mode::GitHub(modal) = &mut app.mode else {
                    unreachable!("Start Work failure lost its modal")
                };
                if failure.kind == StartWorkFailureKind::BranchConflict
                    && let Some(form) = &mut modal.start_work
                {
                    form.branch_conflict = Some(form.branch.clone());
                }
                modal.fail_start_work(failure);
            }
            Ok(response) => {
                return Err(DaemonError::Lifecycle(format!(
                    "unexpected Start Work response: {response:?}"
                )));
            }
            Err(error) => {
                let Mode::GitHub(modal) = &mut app.mode else {
                    unreachable!("Start Work transport failure lost its modal")
                };
                modal.fail_start_work(StartWorkFailure::new(
                    StartWorkFailureKind::InvalidRequest,
                    bounded_feedback(error),
                ));
            }
        },
        GitHubAsyncResult::RetrySession { response, .. } => match response {
            Ok(DaemonResponse::StartWorkCompleted(StartWorkOutcome::Started {
                worktree,
                source,
                session,
                ..
            })) => return Ok(apply_started_work(app, worktree, source, *session)),
            Ok(DaemonResponse::StartWorkFailure(failure)) => {
                let Mode::GitHub(modal) = &mut app.mode else {
                    unreachable!("Retry Session failure lost its modal")
                };
                if let Some(recovery) = &mut modal.recovery {
                    recovery.pending = false;
                    recovery.failure = failure;
                }
            }
            Ok(DaemonResponse::StartWorkCompleted(StartWorkOutcome::WorktreeCreated {
                ..
            })) => {
                unreachable!("Retry Session cannot create another Worktree")
            }
            Ok(response) => {
                return Err(DaemonError::Lifecycle(format!(
                    "unexpected Retry Session response: {response:?}"
                )));
            }
            Err(error) => {
                let Mode::GitHub(modal) = &mut app.mode else {
                    unreachable!("Retry Session transport failure lost its modal")
                };
                if let Some(recovery) = &mut modal.recovery {
                    recovery.pending = false;
                    recovery.failure = StartWorkFailure::new(
                        StartWorkFailureKind::ProviderLaunchFailed,
                        bounded_feedback(error),
                    );
                }
            }
        },
    }
    Ok(GitHubResultEffect::None)
}

fn apply_started_work(
    app: &mut App,
    worktree: sylvops_core::domain::Worktree,
    source: sylvops_core::github::WorkSource,
    session: sylvops_core::domain::Session,
) -> GitHubResultEffect {
    let session_id = session.id;
    upsert_start_work_state(app, worktree, source, Some(session));
    app.mode = Mode::Navigation;
    app.reveal_and_select(ExplorerNode::Session(session_id));
    app.main_tab = TuiContentView::Terminal;
    GitHubResultEffect::Attach { session_id }
}

fn upsert_start_work_state(
    app: &mut App,
    worktree: sylvops_core::domain::Worktree,
    source: sylvops_core::github::WorkSource,
    session: Option<sylvops_core::domain::Session>,
) {
    if let Some(index) = app
        .snapshot
        .worktrees
        .iter()
        .position(|existing| existing.id == worktree.id)
    {
        app.snapshot.worktrees[index] = worktree;
    } else {
        app.snapshot.worktrees.push(worktree);
    }
    if let Some(existing) = app
        .snapshot
        .work_sources
        .iter_mut()
        .find(|existing| existing.worktree_id == source.worktree_id)
    {
        *existing = source;
    } else {
        app.snapshot.work_sources.push(source);
    }
    if let Some(session) = session {
        if let Some(existing) = app
            .snapshot
            .sessions
            .iter_mut()
            .find(|existing| existing.id == session.id)
        {
            *existing = session;
        } else {
            app.snapshot.sessions.push(session);
        }
    }
    if let Mode::GitHub(modal) = &mut app.mode {
        modal.sync_local_work(&app.snapshot);
    }
}

async fn apply_github_result_async(
    client: &DaemonClient,
    app: &mut App,
    result: GitHubAsyncResult,
) -> Result<(), DaemonError> {
    match apply_github_result(app, result)? {
        GitHubResultEffect::None => {}
        GitHubResultEffect::OpenWorktree(worktree_id) => {
            if open_github_worktree(app, worktree_id) {
                attach_selected(client, app).await?;
            }
        }
        GitHubResultEffect::Attach { session_id } => {
            app.mode = Mode::Navigation;
            app.reveal_and_select(ExplorerNode::Session(session_id));
            attach_selected(client, app).await?;
        }
    }
    Ok(())
}

fn open_github_worktree(app: &mut App, worktree_id: WorktreeId) -> bool {
    let session = app
        .snapshot
        .sessions
        .iter()
        .filter(|session| session.worktree_id == worktree_id)
        .max_by_key(|session| {
            (
                lifecycle_is_active(session.status.lifecycle),
                session.last_activity_at,
            )
        })
        .map(|session| (session.id, lifecycle_is_active(session.status.lifecycle)));
    app.mode = Mode::Navigation;
    if let Some((session_id, active)) = session {
        app.reveal_and_select(ExplorerNode::Session(session_id));
        if active {
            app.main_tab = TuiContentView::Terminal;
            true
        } else {
            app.main_tab = TuiContentView::Details;
            false
        }
    } else {
        app.reveal_and_select(ExplorerNode::Worktree(worktree_id));
        app.set_tab(sylvops_core::ui::TuiContentView::Details);
        false
    }
}

fn apply_github_list_response(
    modal: &mut GitHubModal,
    response: Result<DaemonResponse, String>,
) -> Result<(), DaemonError> {
    match response {
        Ok(DaemonResponse::GitHubItems(page)) => modal.receive_list(page),
        Ok(DaemonResponse::GitHubBrowseFailure(failure)) => {
            let diagnostic = bounded_feedback(failure.diagnostic.clone());
            modal.fail(GitHubRequestTarget::List, failure, diagnostic);
        }
        Ok(DaemonResponse::GitHubFailure(failure)) => {
            modal.set_configuration_error(failure.message().into());
        }
        Ok(DaemonResponse::Error(error)) => {
            modal.set_configuration_error(bounded_feedback(error.message));
        }
        Err(error) => modal.set_configuration_error(bounded_feedback(error)),
        Ok(response) => {
            return Err(DaemonError::Lifecycle(format!(
                "unexpected GitHub list response: {response:?}"
            )));
        }
    }
    Ok(())
}

fn apply_github_detail_response(
    modal: &mut GitHubModal,
    append_comments: bool,
    response: Result<DaemonResponse, String>,
) -> Result<(), DaemonError> {
    match response {
        Ok(DaemonResponse::GitHubItem(detail)) if append_comments => modal.append_comments(detail),
        Ok(DaemonResponse::GitHubItem(detail)) => modal.receive_detail(detail),
        Ok(DaemonResponse::GitHubBrowseFailure(failure)) => {
            let diagnostic = bounded_feedback(failure.diagnostic.clone());
            modal.fail(GitHubRequestTarget::Detail, failure, diagnostic);
        }
        Ok(DaemonResponse::GitHubFailure(failure)) => {
            modal.set_configuration_error(failure.message().into());
        }
        Ok(DaemonResponse::Error(error)) => {
            modal.set_configuration_error(bounded_feedback(error.message));
        }
        Err(error) => modal.set_configuration_error(bounded_feedback(error)),
        Ok(response) => {
            return Err(DaemonError::Lifecycle(format!(
                "unexpected GitHub detail response: {response:?}"
            )));
        }
    }
    Ok(())
}

fn apply_github_browser_response(
    modal: &mut GitHubModal,
    response: Result<DaemonResponse, String>,
) -> Result<(), DaemonError> {
    match response {
        Ok(DaemonResponse::Acknowledged) => modal.browser_opened(),
        Ok(DaemonResponse::GitHubBrowseFailure(failure)) => {
            let diagnostic = bounded_feedback(failure.diagnostic.clone());
            modal.fail(GitHubRequestTarget::Browser, failure, diagnostic);
        }
        Ok(DaemonResponse::GitHubFailure(failure)) => {
            modal.browser_failed(failure.message().into());
        }
        Ok(DaemonResponse::Error(error)) => {
            modal.browser_failed(bounded_feedback(error.message));
        }
        Err(error) => modal.browser_failed(bounded_feedback(error)),
        Ok(response) => {
            return Err(DaemonError::Lifecycle(format!(
                "unexpected GitHub browser response: {response:?}"
            )));
        }
    }
    Ok(())
}

async fn refresh_github_status(client: &DaemonClient, app: &mut App) -> Result<(), DaemonError> {
    match client
        .request(&ClientRequest::GetGitHubStatus {
            project_id: app.selected_project_id,
        })
        .await?
    {
        DaemonResponse::GitHubStatus(status) => {
            app.github_status = Some(status);
            Ok(())
        }
        DaemonResponse::Error(error) => {
            app.flash = Some(Flash::error(bounded_feedback(error.message)));
            Ok(())
        }
        DaemonResponse::GitHubFailure(failure) => {
            app.flash = Some(Flash::error(failure.message()));
            Ok(())
        }
        response => Err(DaemonError::Lifecycle(format!(
            "unexpected GitHub status response: {response:?}"
        ))),
    }
}

fn selected_node(app: &App) -> Option<ExplorerNode> {
    app.selected_node()
}

fn removal_inspection_request(app: &App) -> Option<ClientRequest> {
    match selected_node(app)? {
        ExplorerNode::Workspace(workspace_id) => {
            Some(ClientRequest::InspectWorkspaceDeletion { workspace_id })
        }
        ExplorerNode::Project(project_id) => {
            Some(ClientRequest::InspectProjectRemoval { project_id })
        }
        ExplorerNode::Worktree(worktree_id) => {
            Some(ClientRequest::GetWorktreeStatus { worktree_id })
        }
        ExplorerNode::Session(session_id) => {
            Some(ClientRequest::InspectSessionDeletion { session_id })
        }
    }
}

fn open_create_form(app: &mut App) {
    app.mode = match selected_node(app) {
        Some(ExplorerNode::Workspace(id)) => Mode::Modal(Form::repository(id)),
        Some(ExplorerNode::Session(id)) => app
            .snapshot
            .sessions
            .iter()
            .find(|session| session.id == id)
            .map_or(Mode::Navigation, |session| {
                Mode::Modal(Form::session(session.worktree_id, app.providers.clone()))
            }),
        Some(ExplorerNode::Worktree(id)) => Mode::Modal(Form::session(id, app.providers.clone())),
        Some(ExplorerNode::Project(id)) => Mode::Modal(Form::worktree(id)),
        None => app.active_workspace().map_or_else(
            || Mode::Modal(Form::workspace()),
            |workspace| Mode::Modal(Form::repository(workspace.id)),
        ),
    };
}

fn open_rename_form(app: &mut App) {
    let form = match selected_node(app) {
        Some(ExplorerNode::Project(id)) => app
            .snapshot
            .projects
            .iter()
            .find(|item| item.id == id)
            .map(|item| Form::rename("Rename project", &item.name, FormKind::RenameProject(id))),
        Some(ExplorerNode::Worktree(id)) => app
            .snapshot
            .worktrees
            .iter()
            .find(|item| item.id == id)
            .map(|item| {
                Form::rename(
                    "Rename worktree label",
                    &item.name,
                    FormKind::RenameWorktree(id),
                )
            }),
        Some(ExplorerNode::Session(id)) => app
            .snapshot
            .sessions
            .iter()
            .find(|item| item.id == id)
            .map(|item| {
                Form::rename(
                    "Rename session",
                    &item.display_name,
                    FormKind::RenameSession(id),
                )
            }),
        Some(ExplorerNode::Workspace(_)) | None => None,
    };
    if let Some(form) = form {
        app.mode = Mode::Modal(form);
    } else {
        app.flash = Some(Flash::error(
            "Select a project, worktree, or session to rename.",
        ));
    }
}

async fn open_destructive_confirmation(
    client: &DaemonClient,
    app: &mut App,
) -> Result<(), DaemonError> {
    let Some(request) = removal_inspection_request(app) else {
        app.flash = Some(Flash::error("Nothing is selected."));
        return Ok(());
    };
    let response = client.request(&request).await?;
    apply_removal_inspection(app, response);
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn apply_removal_inspection(app: &mut App, response: DaemonResponse) {
    match response {
        DaemonResponse::WorkspaceDeletionInspected(inspection) => {
            let Some(workspace) = app
                .snapshot
                .workspaces
                .iter()
                .find(|item| item.id == inspection.workspace_id)
            else {
                app.flash = Some(Flash::error(
                    "Workspace deletion target is no longer visible.",
                ));
                return;
            };
            let Some(token) = inspection.authorization_token else {
                let reason = inspection.refusals.first().map_or_else(
                    || "The daemon did not authorize Workspace deletion.".into(),
                    workspace_deletion_refusal_text,
                );
                app.flash = Some(Flash::error(bounded_feedback(format!(
                    "Delete Workspace refused: {reason}"
                ))));
                return;
            };
            let fallback = if inspection.is_open {
                inspection.fallback_workspace_id.map_or_else(
                    || "No Workspace will remain open.".into(),
                    |id| format!("Workspace {id} will become active."),
                )
            } else {
                "The active Workspace will not change.".into()
            };
            app.mode = Mode::Confirmation(Confirmation {
                title: "Delete Workspace?".into(),
                detail: format!(
                    "Delete Workspace '{}' ({}) exactly? Only SylvOps metadata is deleted; repositories, Worktrees, branches, and filesystem content is preserved. {fallback}",
                    workspace.name, workspace.id
                ),
                action: ConfirmAction::DeleteWorkspace {
                    workspace_id: workspace.id,
                    token,
                },
            });
        }
        DaemonResponse::ProjectRemovalInspected(inspection) => {
            let Some(project) = app
                .snapshot
                .projects
                .iter()
                .find(|item| item.id == inspection.project_id)
            else {
                app.flash = Some(Flash::error("Project removal target is no longer visible."));
                return;
            };
            let Some(token) = inspection.authorization_token else {
                let reason = if inspection.refusals.is_empty() {
                    "The daemon did not authorize Project removal.".into()
                } else {
                    inspection
                        .refusals
                        .iter()
                        .map(project_removal_refusal_text)
                        .collect::<Vec<_>>()
                        .join(" ")
                };
                app.flash = Some(Flash::error(bounded_feedback(format!(
                    "Remove Project refused: {reason}"
                ))));
                return;
            };
            app.mode = Mode::Confirmation(Confirmation {
                title: "Remove Project?".into(),
                detail: format!(
                    "Remove Project '{}' ({}) exactly? The repository at {} and all filesystem and Git content are preserved.",
                    project.name, project.id, project.canonical_repository_path
                ),
                action: ConfirmAction::RemoveProject {
                    project_id: project.id,
                    token,
                },
            });
        }
        DaemonResponse::WorktreeStatus(status) => {
            let Some(worktree) = app
                .snapshot
                .worktrees
                .iter()
                .find(|item| item.id == status.worktree_id)
            else {
                app.flash = Some(Flash::error(
                    "Worktree removal target is no longer visible.",
                ));
                return;
            };
            if worktree.is_root_checkout {
                app.flash = Some(Flash::error(
                    "Remove Worktree refused: the repository root checkout is preserved.",
                ));
                return;
            }
            if worktree.status != sylvops_core::domain::WorktreeStatus::Active {
                app.flash = Some(Flash::error(format!(
                    "Remove Worktree refused: '{}' is not active (state: {}).",
                    worktree.name, worktree.status
                )));
                return;
            }
            if app.snapshot.sessions.iter().any(|session| {
                session.worktree_id == worktree.id && lifecycle_is_active(session.status.lifecycle)
            }) {
                app.flash = Some(Flash::error(
                    "Remove Worktree refused: an active Session remains. Stop it and wait for exit first.",
                ));
                return;
            }
            if !status.clean {
                app.flash = None;
                app.mode = Mode::RemovalBlocked(removal_notice::RemovalNotice::new(&status));
                return;
            }
            let Some(token) = status.removal_confirmation_token else {
                app.flash = Some(Flash::error(
                    "The daemon did not authorize removal. Inspect the Worktree again.",
                ));
                return;
            };
            app.mode = Mode::Confirmation(Confirmation {
                title: "Remove Worktree?".into(),
                detail: format!(
                    "Remove Worktree '{}' ({}) exactly? The managed Worktree directory and its filesystem content at {} are removed; the Git branch is preserved.",
                    worktree.name, worktree.id, worktree.canonical_path
                ),
                action: ConfirmAction::RemoveWorktree {
                    worktree_id: worktree.id,
                    token,
                },
            });
        }
        DaemonResponse::SessionDeletionInspected(inspection) => {
            let Some(session) = app
                .snapshot
                .sessions
                .iter()
                .find(|item| item.id == inspection.session_id)
            else {
                app.flash = Some(Flash::error("Session removal target is no longer visible."));
                return;
            };
            if matches!(
                inspection.lifecycle,
                SessionLifecycle::Fresh | SessionLifecycle::Starting | SessionLifecycle::Running
            ) {
                app.mode = Mode::Confirmation(Confirmation {
                    title: "Stop Session?".into(),
                    detail: format!(
                        "Stop Session '{}' ({}) and its descendant processes? Session history, Worktree, and filesystem content are preserved.",
                        session.display_name, session.id
                    ),
                    action: ConfirmAction::StopSession(session.id),
                });
                return;
            }
            let Some(token) = inspection.authorization_token else {
                let reason =
                    session_deletion_refusal_text(inspection.refusal, inspection.lifecycle);
                app.flash = Some(Flash::error(bounded_feedback(format!(
                    "Delete Session refused: {reason}"
                ))));
                return;
            };
            app.mode = Mode::Confirmation(Confirmation {
                title: "Delete Session?".into(),
                detail: format!(
                    "Delete Session '{}' ({}) exactly? Only Session history and metadata are deleted; Worktree {} and filesystem content are preserved.",
                    session.display_name, session.id, inspection.worktree_id
                ),
                action: ConfirmAction::DeleteSession {
                    session_id: session.id,
                    token,
                },
            });
        }
        response => {
            app.flash = Some(Flash::error(response_message(
                "removal inspection",
                response,
            )));
        }
    }
}

fn workspace_deletion_refusal_text(refusal: &WorkspaceDeletionRefusal) -> String {
    match refusal {
        WorkspaceDeletionRefusal::ProjectsRemain {
            count, truncated, ..
        } => format!(
            "{}{count} Project{} remain. Remove every Project first.",
            if *truncated { "At least " } else { "" },
            if *count == 1 { "" } else { "s" }
        ),
    }
}

fn project_removal_refusal_text(refusal: &ProjectRemovalRefusal) -> String {
    match refusal {
        ProjectRemovalRefusal::SessionsRemain {
            count, truncated, ..
        } => format!(
            "{}{count} Session{} remain. Stop active Sessions and delete inactive Sessions first.",
            if *truncated { "At least " } else { "" },
            if *count == 1 { "" } else { "s" }
        ),
        ProjectRemovalRefusal::ManagedWorktreesRemain {
            count, truncated, ..
        } => format!(
            "{}{count} managed Worktree{} remain. Remove every managed Worktree first.",
            if *truncated { "At least " } else { "" },
            if *count == 1 { "" } else { "s" }
        ),
        ProjectRemovalRefusal::InconsistentDescendants => {
            "Project metadata is inconsistent; refresh before trying again.".into()
        }
    }
}

fn session_deletion_refusal_text(
    refusal: Option<SessionDeletionRefusal>,
    lifecycle: SessionLifecycle,
) -> String {
    match refusal {
        Some(SessionDeletionRefusal::LiveState) => {
            "the Session is active. Stop it and wait for exit first.".into()
        }
        Some(SessionDeletionRefusal::OwnedProcess) => {
            "the Session still owns a process tree. Stop it and wait for exit first.".into()
        }
        Some(SessionDeletionRefusal::IneligibleState) | None => {
            format!("lifecycle {lifecycle:?} is not eligible for deletion.")
        }
    }
}

fn bounded_feedback(mut text: String) -> String {
    const MAX_CHARS: usize = 512;
    if let Some((index, _)) = text.char_indices().nth(MAX_CHARS) {
        text.truncate(index);
        text.push_str("...");
    }
    text
}

#[allow(clippy::too_many_lines)]
async fn handle_form(
    client: &DaemonClient,
    app: &mut App,
    input: Event,
) -> Result<(), DaemonError> {
    if matches!(&app.mode, Mode::Modal(form) if form.pending) {
        return Ok(());
    }
    let mouse_target = if let Event::Mouse(mouse) = &input
        && mouse.kind == MouseEventKind::Down(MouseButton::Left)
    {
        app.hit_map.target_at(mouse.column, mouse.row)
    } else {
        None
    };
    if matches!(mouse_target, Some(HitTarget::ModalCancel)) {
        app.mode = Mode::Navigation;
        return Ok(());
    }
    if matches!(mouse_target, Some(HitTarget::ModalBrowse)) {
        open_directory_browser(app).await;
        return Ok(());
    }
    if let Some(HitTarget::ModalField(index)) = mouse_target {
        if let Mode::Modal(form) = &mut app.mode {
            form.active = index.min(form.visible_field_indices().len().saturating_sub(1));
        }
        return Ok(());
    }
    if matches!(mouse_target, Some(HitTarget::ModalProvider(_))) {
        if let Mode::Modal(form) = &mut app.mode {
            form.provider_index = if form.providers.is_empty() {
                0
            } else {
                (form.provider_index + 1) % form.providers.len()
            };
        }
        return Ok(());
    }
    if matches!(mouse_target, Some(HitTarget::ModalProviderProbe)) {
        probe_form_provider(client, app).await?;
        return Ok(());
    }
    let mut submit = matches!(mouse_target, Some(HitTarget::ModalSubmit));
    if let Some(HitTarget::ModalWorktreeBase(mode)) = mouse_target {
        if let Mode::Modal(form) = &mut app.mode {
            form.select_worktree_base(mode);
        }
        return Ok(());
    }
    let mut probe_provider = false;
    let mut browse = false;
    match input {
        Event::Paste(text) => {
            if let Mode::Modal(form) = &mut app.mode
                && let Some(index) = form.active_field_index()
            {
                push_bounded(&mut form.fields[index].value, &text);
                form.clear_errors();
            }
        }
        Event::Key(key) => {
            let Mode::Modal(form) = &mut app.mode else {
                return Ok(());
            };
            match key.code {
                KeyCode::Esc => app.mode = Mode::Navigation,
                KeyCode::Tab | KeyCode::Down => form.next_field(1),
                KeyCode::BackTab | KeyCode::Up => form.next_field(-1),
                KeyCode::Left if form.is_session() => form.select_next_provider(-1),
                KeyCode::Right if form.is_session() => form.select_next_provider(1),
                KeyCode::Left if form.is_worktree() => form.next_worktree_base(-1),
                KeyCode::Right if form.is_worktree() => form.next_worktree_base(1),
                KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    probe_provider = form.is_session();
                }
                KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    browse = matches!(form.kind, FormKind::RegisterProject(_));
                }
                KeyCode::Backspace => {
                    if let Some(index) = form.active_field_index() {
                        form.fields[index].value.pop();
                        form.clear_errors();
                    }
                }
                KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Some(index) = form.active_field_index() {
                        push_bounded(&mut form.fields[index].value, &character.to_string());
                        form.clear_errors();
                    }
                }
                KeyCode::Enter => {
                    if form.active + 1 < form.visible_field_indices().len() {
                        form.next_field(1);
                    } else {
                        submit = true;
                    }
                }
                _ => {}
            }
        }
        _ => {}
    }
    if probe_provider {
        probe_form_provider(client, app).await?;
        return Ok(());
    }
    if browse {
        open_directory_browser(app).await;
        return Ok(());
    }
    if !submit {
        return Ok(());
    }
    let valid = if let Mode::Modal(form) = &mut app.mode {
        form.validate()
    } else {
        false
    };
    if !valid {
        return Ok(());
    }
    let mut form = match &app.mode {
        Mode::Modal(form) => form.clone(),
        _ => return Ok(()),
    };
    if let Err(error) = prepare_form_for_submission(&mut form, &app.invocation_directory) {
        if let Mode::Modal(current) = &mut app.mode {
            current.submission_error = Some(error.into());
        }
        return Ok(());
    }
    if form.is_worktree() {
        let (send, receive) = tokio::sync::oneshot::channel();
        app.pending_form = Some(receive);
        if let Mode::Modal(current) = &mut app.mode {
            current.pending = true;
        }
        let client = client.clone();
        tokio::spawn(async move {
            let _ = send.send(perform_form(&client, &form).await);
        });
        return Ok(());
    }
    finish_form_submission(client, app, perform_form(client, &form).await).await
}

async fn finish_form_submission(
    client: &DaemonClient,
    app: &mut App,
    result: Result<FormOutcome, DaemonError>,
) -> Result<(), DaemonError> {
    match result {
        Ok(outcome) => {
            app.mode = Mode::Navigation;
            app.flash = Some(Flash::transient(FlashKind::Success, outcome.message));
            app.replace_snapshot(snapshot(client).await?);
            if let Some(selection) = outcome.selection {
                match selection {
                    ExplorerNode::Worktree(id) => {
                        if let Some(project_id) = app
                            .snapshot
                            .worktrees
                            .iter()
                            .find(|item| item.id == id)
                            .map(|item| item.project_id)
                        {
                            app.expanded_projects.insert(project_id);
                        }
                    }
                    ExplorerNode::Session(id) => {
                        if let Some(worktree_id) = app
                            .snapshot
                            .sessions
                            .iter()
                            .find(|item| item.id == id)
                            .map(|item| item.worktree_id)
                        {
                            app.expanded_worktrees.insert(worktree_id);
                            if let Some(project_id) = app
                                .snapshot
                                .worktrees
                                .iter()
                                .find(|item| item.id == worktree_id)
                                .map(|item| item.project_id)
                            {
                                app.expanded_projects.insert(project_id);
                            }
                        }
                    }
                    ExplorerNode::Workspace(_) | ExplorerNode::Project(_) => {}
                }
                app.select_node(selection);
            }
        }
        Err(error) => {
            if let Mode::Modal(current) = &mut app.mode {
                current.pending = false;
                current.submission_error = Some(bounded_feedback(error.to_string()));
            }
        }
    }
    Ok(())
}

async fn open_directory_browser(app: &mut App) {
    let form = match &app.mode {
        Mode::Modal(form) if matches!(form.kind, FormKind::RegisterProject(_)) => form.clone(),
        _ => return,
    };
    app.mode =
        Mode::DirectoryBrowser(DirectoryBrowser::new(form, app.invocation_directory.clone()).await);
}

async fn handle_directory_browser(app: &mut App, input: Event) {
    let mouse_target = if let Event::Mouse(mouse) = &input
        && mouse.kind == MouseEventKind::Down(MouseButton::Left)
    {
        app.hit_map.target_at(mouse.column, mouse.row)
    } else {
        None
    };
    if matches!(mouse_target, Some(HitTarget::DirectoryBrowserCancel))
        || matches!(&input, Event::Key(key) if key.code == KeyCode::Esc)
    {
        restore_directory_browser_form(app, None);
        return;
    }
    if matches!(mouse_target, Some(HitTarget::DirectoryBrowserSelect)) {
        select_browser_current_directory(app);
        return;
    }

    let clicked_row = match mouse_target {
        Some(HitTarget::DirectoryBrowserRow(index)) => Some(index),
        _ => None,
    };
    let mut activate = clicked_row.is_some();
    let mut refresh = false;
    if let Mode::DirectoryBrowser(browser) = &mut app.mode {
        if let Some(index) = clicked_row {
            browser.selected = index.min(browser.visible_entries().len().saturating_sub(1));
        }
        match input {
            Event::Paste(text) => browser.push_filter(&text),
            Event::Mouse(mouse) if mouse.kind == MouseEventKind::ScrollDown => {
                browser.move_selection(1);
            }
            Event::Mouse(mouse) if mouse.kind == MouseEventKind::ScrollUp => {
                browser.move_selection(-1);
            }
            Event::Key(key) => match key.code {
                KeyCode::Down => browser.move_selection(1),
                KeyCode::Up => browser.move_selection(-1),
                KeyCode::Backspace => browser.pop_filter(),
                KeyCode::Enter => activate = true,
                KeyCode::Char('h') if key.modifiers.is_empty() => browser.toggle_hidden(),
                KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    refresh = true;
                }
                KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    browser.push_filter(&character.to_string());
                }
                _ => {}
            },
            _ => {}
        }
    }
    if !activate {
        if refresh && let Mode::DirectoryBrowser(browser) = &mut app.mode {
            browser.refresh().await;
        }
        return;
    }
    let entry = match &app.mode {
        Mode::DirectoryBrowser(browser) => browser.selected_entry(),
        _ => None,
    };
    match entry {
        Some(DirectoryBrowserEntry::SelectCurrent) => select_browser_current_directory(app),
        Some(DirectoryBrowserEntry::Navigate { path, .. }) => {
            if let Mode::DirectoryBrowser(browser) = &mut app.mode {
                browser.navigate(path).await;
            }
        }
        None => {}
    }
}

fn select_browser_current_directory(app: &mut App) {
    let selected = match &app.mode {
        Mode::DirectoryBrowser(browser) if browser.can_select_current() => browser.current.clone(),
        _ => return,
    };
    restore_directory_browser_form(app, Some(selected));
}

fn restore_directory_browser_form(app: &mut App, selected: Option<PathBuf>) {
    let Mode::DirectoryBrowser(browser) = &app.mode else {
        return;
    };
    let mut form = browser.form.clone();
    if let Some(selected) = selected {
        if let Some(path) = selected.to_str() {
            form.set_repository_path(path.into());
        } else {
            form.submission_error =
                Some("The selected directory cannot be represented by the IPC path format.".into());
        }
    }
    app.mode = Mode::Modal(form);
}

fn prepare_form_for_submission(
    form: &mut Form,
    invocation_directory: &Path,
) -> Result<(), &'static str> {
    let Some(submitted) = form.repository_path().map(str::trim) else {
        return Ok(());
    };
    let submitted = Path::new(submitted);
    if !submitted.is_absolute() {
        let absolute = invocation_directory.join(submitted);
        let path = absolute
            .to_str()
            .ok_or("The repository path cannot be represented by the IPC path format.")?;
        form.set_repository_path(path.into());
    }
    Ok(())
}

async fn probe_form_provider(client: &DaemonClient, app: &mut App) -> Result<(), DaemonError> {
    let Some(kind) = (match &app.mode {
        Mode::Modal(form) if form.is_session() => form.provider().map(|provider| provider.kind),
        _ => None,
    }) else {
        return Ok(());
    };
    let health = match client
        .request(&ClientRequest::ProbeProvider { kind })
        .await?
    {
        DaemonResponse::Provider(health) if health.kind == kind => health,
        DaemonResponse::Error(error) => {
            if let Mode::Modal(form) = &mut app.mode {
                form.submission_error = Some(error.message);
            }
            return Ok(());
        }
        response => {
            return Err(DaemonError::Lifecycle(format!(
                "unexpected provider probe response: {response:?}"
            )));
        }
    };
    if let Some(provider) = app
        .providers
        .iter_mut()
        .find(|provider| provider.kind == kind)
    {
        provider.clone_from(&health);
    }
    if let Mode::Modal(form) = &mut app.mode {
        if let Some(provider) = form
            .providers
            .iter_mut()
            .find(|provider| provider.kind == kind)
        {
            provider.clone_from(&health);
        }
        form.clear_errors();
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
async fn perform_form(client: &DaemonClient, form: &Form) -> Result<FormOutcome, DaemonError> {
    let (message, response) = match form.kind {
        FormKind::CreateWorkspace => (
            "Workspace created",
            client
                .request(&ClientRequest::AddWorkspace {
                    name: form.fields[0].value.trim().into(),
                })
                .await?,
        ),
        FormKind::RegisterProject(workspace_id) => (
            "Repository ready",
            client
                .request(&ClientRequest::EnsureProject {
                    workspace_id,
                    repository_path: form.repository_path().unwrap_or_default().trim().into(),
                })
                .await?,
        ),
        FormKind::CreateWorktree(_) => (
            "Worktree created",
            client
                .request(
                    &form
                        .worktree_request()
                        .map_err(DaemonError::InvalidSession)?,
                )
                .await?,
        ),
        FormKind::CreateSession(_) => {
            let request = form
                .session_request(80, 24)
                .ok_or_else(|| DaemonError::Lifecycle("no provider selected".into()))?;
            ("Session created", client.request(&request).await?)
        }
        FormKind::RenameProject(project_id) => (
            "Project renamed",
            client
                .request(&ClientRequest::RenameProject {
                    project_id,
                    name: form.fields[0].value.trim().into(),
                })
                .await?,
        ),
        FormKind::RenameWorktree(worktree_id) => (
            "Worktree renamed",
            client
                .request(&ClientRequest::RenameWorktree {
                    worktree_id,
                    name: form.fields[0].value.trim().into(),
                })
                .await?,
        ),
        FormKind::RenameSession(session_id) => (
            "Session renamed",
            client
                .request(&ClientRequest::RenameSession {
                    session_id,
                    name: form.fields[0].value.trim().into(),
                })
                .await?,
        ),
    };
    let message = match &response {
        DaemonResponse::ProjectReady { created: true, .. } => "Repository registered",
        DaemonResponse::ProjectReady { created: false, .. } => "Existing repository selected",
        _ => message,
    };
    let selection = match &response {
        DaemonResponse::ProjectAdded { root_worktree, .. }
        | DaemonResponse::ProjectReady { root_worktree, .. } => {
            Some(ExplorerNode::Worktree(root_worktree.id))
        }
        DaemonResponse::WorktreeCreated { worktree, .. } => {
            Some(ExplorerNode::Worktree(worktree.id))
        }
        DaemonResponse::SessionCreated { session, .. } => Some(ExplorerNode::Session(session.id)),
        _ => None,
    };
    match response {
        DaemonResponse::Error(error) => Err(DaemonError::Lifecycle(error.message)),
        DaemonResponse::WorktreeCreated { worktree, .. } => Ok(FormOutcome {
            message: format!(
                "Worktree created from {} @ {}",
                worktree.base_ref,
                worktree.base_commit.chars().take(8).collect::<String>()
            ),
            selection,
        }),
        _ => Ok(FormOutcome {
            message: message.into(),
            selection,
        }),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConfirmationDecision {
    Confirm,
    Cancel,
    Ignore,
}

fn confirmation_decision(app: &App, input: &Event) -> ConfirmationDecision {
    let clicked = if let Event::Mouse(mouse) = input
        && mouse.kind == MouseEventKind::Down(MouseButton::Left)
    {
        app.hit_map.target_at(mouse.column, mouse.row)
    } else {
        None
    };
    if matches!(clicked, Some(HitTarget::Confirm))
        || matches!(input, Event::Key(key) if key.code == KeyCode::Char('y'))
    {
        ConfirmationDecision::Confirm
    } else if matches!(clicked, Some(HitTarget::Cancel))
        || matches!(input, Event::Key(key) if matches!(key.code, KeyCode::Esc | KeyCode::Char('n')))
    {
        ConfirmationDecision::Cancel
    } else {
        ConfirmationDecision::Ignore
    }
}

fn dismiss_confirmation(app: &mut App) {
    app.mode = Mode::Navigation;
}

fn confirmation_request(action: &ConfirmAction) -> ClientRequest {
    match action {
        ConfirmAction::DeleteWorkspace {
            workspace_id,
            token,
        } => ClientRequest::DeleteWorkspace {
            workspace_id: *workspace_id,
            authorization_token: token.clone(),
        },
        ConfirmAction::RemoveProject { project_id, token } => ClientRequest::RemoveProject {
            project_id: *project_id,
            authorization_token: token.clone(),
        },
        ConfirmAction::RemoveWorktree { worktree_id, token } => ClientRequest::RemoveWorktree {
            worktree_id: *worktree_id,
            confirmation_token: token.clone(),
        },
        ConfirmAction::StopSession(session_id) => ClientRequest::StopSession {
            session_id: *session_id,
        },
        ConfirmAction::DeleteSession { session_id, token } => ClientRequest::DeleteSession {
            session_id: *session_id,
            authorization_token: token.clone(),
        },
    }
}

fn confirmation_success_message(
    action: &ConfirmAction,
    response: DaemonResponse,
) -> Result<&'static str, String> {
    if let DaemonResponse::Error(error) = response {
        return Err(error.message);
    }
    match (action, response) {
        (
            ConfirmAction::DeleteWorkspace { workspace_id, .. },
            DaemonResponse::WorkspaceDeleted {
                workspace_id: deleted,
                ..
            },
        ) if workspace_id == &deleted => Ok("Workspace deleted."),
        (
            ConfirmAction::RemoveProject { project_id, .. },
            DaemonResponse::ProjectRemoved {
                project_id: removed,
                ..
            },
        ) if project_id == &removed => Ok("Project removed; repository preserved."),
        (
            ConfirmAction::RemoveWorktree { worktree_id, .. },
            DaemonResponse::WorktreeRemoved { worktree, .. },
        ) if worktree_id == &worktree.id => Ok("Worktree removed; branch preserved."),
        (ConfirmAction::StopSession(_), DaemonResponse::Acknowledged) => Ok("Session stopped."),
        (
            ConfirmAction::DeleteSession { session_id, .. },
            DaemonResponse::SessionDeleted {
                session_id: deleted,
                ..
            },
        ) if session_id == &deleted => Ok("Session deleted."),
        (_, response) => Err(format!(
            "unexpected removal confirmation response: {response:?}"
        )),
    }
}

async fn handle_confirmation(
    client: &DaemonClient,
    app: &mut App,
    input: Event,
) -> Result<(), DaemonError> {
    match confirmation_decision(app, &input) {
        ConfirmationDecision::Cancel => {
            dismiss_confirmation(app);
            return Ok(());
        }
        ConfirmationDecision::Ignore => return Ok(()),
        ConfirmationDecision::Confirm => {}
    }
    let Mode::Confirmation(dialog) = app.mode.clone() else {
        return Ok(());
    };
    let response = client
        .request(&confirmation_request(&dialog.action))
        .await?;
    dismiss_confirmation(app);
    match confirmation_success_message(&dialog.action, response) {
        Ok(message) => {
            app.flash = Some(Flash::transient(FlashKind::Success, message));
            app.replace_snapshot(snapshot(client).await?);
        }
        Err(message) => app.flash = Some(Flash::error(bounded_feedback(message))),
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SettingsDecision {
    Continue,
    Apply,
    Cancel,
    AssociateGitHub(usize),
    ClearGitHub,
    RefreshGitHub,
}

fn cycle_choice<T: Copy + PartialEq>(choices: &[T], current: T, delta: isize) -> T {
    let current = choices
        .iter()
        .position(|choice| *choice == current)
        .unwrap_or(0);
    let len = isize::try_from(choices.len()).unwrap_or(1);
    let next = (isize::try_from(current).unwrap_or(0) + delta).rem_euclid(len);
    choices[usize::try_from(next).unwrap_or(0)]
}

fn adjust_setting(app: &mut App, settings: SettingsFocus, delta: isize) {
    match settings {
        SettingsFocus::Theme => app.theme = cycle_choice(&TuiTheme::ALL, app.theme, delta),
        SettingsFocus::Presentation => {
            app.session_presentation = cycle_choice(
                &TuiSessionPresentation::ALL,
                app.session_presentation,
                delta,
            );
        }
        SettingsFocus::Density => {
            app.density = cycle_choice(&TuiDensity::ALL, app.density, delta);
        }
        SettingsFocus::GitHub | SettingsFocus::Apply | SettingsFocus::Cancel => {}
    }
}

#[allow(clippy::too_many_lines)]
fn handle_settings_input(app: &mut App, input: &Event) -> SettingsDecision {
    let clicked = if let Event::Mouse(mouse) = input
        && mouse.kind == MouseEventKind::Down(MouseButton::Left)
    {
        app.hit_map.target_at(mouse.column, mouse.row)
    } else {
        None
    };
    match clicked {
        Some(HitTarget::SettingsGithubRemote(index)) => {
            if let Mode::Settings(settings) = &mut app.mode {
                settings.focus = SettingsFocus::GitHub;
                settings.github_remote_index = index;
            }
            return SettingsDecision::AssociateGitHub(index);
        }
        Some(HitTarget::SettingsGithubClear) => return SettingsDecision::ClearGitHub,
        Some(HitTarget::SettingsGithubRefresh) => return SettingsDecision::RefreshGitHub,
        Some(HitTarget::SettingsTheme(choice)) => {
            app.theme = choice;
            if let Mode::Settings(settings) = &mut app.mode {
                settings.focus = SettingsFocus::Theme;
            }
            return SettingsDecision::Continue;
        }
        Some(HitTarget::SettingsDensity(choice)) => {
            app.density = choice;
            if let Mode::Settings(settings) = &mut app.mode {
                settings.focus = SettingsFocus::Density;
            }
            return SettingsDecision::Continue;
        }
        Some(HitTarget::SettingsPresentation(choice)) => {
            app.session_presentation = choice;
            if let Mode::Settings(settings) = &mut app.mode {
                settings.focus = SettingsFocus::Presentation;
            }
            return SettingsDecision::Continue;
        }
        Some(HitTarget::SettingsApply) => {
            app.apply_settings();
            return SettingsDecision::Apply;
        }
        Some(HitTarget::SettingsCancel) => {
            app.cancel_settings();
            return SettingsDecision::Cancel;
        }
        _ => {}
    }

    let Event::Key(key) = input else {
        return SettingsDecision::Continue;
    };
    if key.code == KeyCode::Esc {
        app.cancel_settings();
        return SettingsDecision::Cancel;
    }
    let Mode::Settings(mut settings) = app.mode else {
        return SettingsDecision::Continue;
    };
    match key.code {
        KeyCode::Down | KeyCode::Tab => settings.focus = settings.focus.moved(1),
        KeyCode::Up | KeyCode::BackTab => settings.focus = settings.focus.moved(-1),
        KeyCode::Left | KeyCode::Char('h') if settings.focus == SettingsFocus::GitHub => {
            let count = app
                .github_status
                .as_ref()
                .map_or(0, |status| status.remotes.len());
            if count > 0 {
                settings.github_remote_index = settings
                    .github_remote_index
                    .saturating_add_signed(-1)
                    .min(count - 1);
            }
        }
        KeyCode::Right | KeyCode::Char('l') if settings.focus == SettingsFocus::GitHub => {
            let count = app
                .github_status
                .as_ref()
                .map_or(0, |status| status.remotes.len());
            if count > 0 {
                settings.github_remote_index = settings
                    .github_remote_index
                    .saturating_add(1)
                    .min(count - 1);
            }
        }
        KeyCode::Left | KeyCode::Char('h') => adjust_setting(app, settings.focus, -1),
        KeyCode::Right | KeyCode::Char('l') => adjust_setting(app, settings.focus, 1),
        KeyCode::Enter if settings.focus == SettingsFocus::GitHub => {
            return SettingsDecision::AssociateGitHub(settings.github_remote_index);
        }
        KeyCode::Char('c') if settings.focus == SettingsFocus::GitHub => {
            return SettingsDecision::ClearGitHub;
        }
        KeyCode::Char('r') if settings.focus == SettingsFocus::GitHub => {
            return SettingsDecision::RefreshGitHub;
        }
        KeyCode::Enter if settings.focus == SettingsFocus::Apply => {
            app.apply_settings();
            return SettingsDecision::Apply;
        }
        KeyCode::Enter if settings.focus == SettingsFocus::Cancel => {
            app.cancel_settings();
            return SettingsDecision::Cancel;
        }
        _ => {}
    }
    app.mode = Mode::Settings(settings);
    SettingsDecision::Continue
}

async fn handle_settings(
    client: &DaemonClient,
    app: &mut App,
    input: &Event,
) -> Result<(), DaemonError> {
    match handle_settings_input(app, input) {
        SettingsDecision::Apply => {
            save_tui_state(client, app).await?;
            app.navigation_dirty_at = None;
        }
        SettingsDecision::AssociateGitHub(index) => {
            let Some(project_id) = app.selected_project_id else {
                app.flash = Some(Flash::error(
                    "Select a Project before choosing a GitHub remote.",
                ));
                return Ok(());
            };
            let Some(remote_name) = app
                .github_status
                .as_ref()
                .and_then(|status| status.remotes.get(index))
                .map(|remote| remote.name.clone())
            else {
                app.flash = Some(Flash::error("No validated GitHub remote is selected."));
                return Ok(());
            };
            apply_github_association(
                client,
                app,
                ClientRequest::SetGitHubAssociation {
                    project_id,
                    remote_name,
                    authorization_token: None,
                },
            )
            .await?;
        }
        SettingsDecision::ClearGitHub => {
            let Some(project_id) = app.selected_project_id else {
                app.flash = Some(Flash::error(
                    "Select a Project before clearing its GitHub association.",
                ));
                return Ok(());
            };
            apply_github_association(
                client,
                app,
                ClientRequest::ClearGitHubAssociation {
                    project_id,
                    authorization_token: None,
                },
            )
            .await?;
        }
        SettingsDecision::RefreshGitHub => refresh_github_status(client, app).await?,
        SettingsDecision::Continue | SettingsDecision::Cancel => {}
    }
    Ok(())
}

async fn apply_github_association(
    client: &DaemonClient,
    app: &mut App,
    request: ClientRequest,
) -> Result<(), DaemonError> {
    match client.request(&request).await? {
        DaemonResponse::GitHubAssociationUpdated { association, .. } => {
            if let Some(status) = &mut app.github_status {
                status.association = association;
            }
            app.flash = Some(Flash::transient(
                FlashKind::Success,
                "GitHub Project association updated.",
            ));
        }
        DaemonResponse::Error(error) => {
            app.flash = Some(Flash::error(bounded_feedback(error.message)));
        }
        DaemonResponse::GitHubFailure(failure) => {
            app.flash = Some(Flash::error(failure.message()));
        }
        response => {
            return Err(DaemonError::Lifecycle(format!(
                "unexpected GitHub association response: {response:?}"
            )));
        }
    }
    Ok(())
}

async fn handle_palette(
    client: &DaemonClient,
    app: &mut App,
    input: Event,
) -> Result<(), DaemonError> {
    let Mode::CommandPalette(mut palette) = app.mode.clone() else {
        return Ok(());
    };
    let clicked = if let Event::Mouse(mouse) = &input
        && mouse.kind == MouseEventKind::Down(MouseButton::Left)
    {
        app.hit_map.target_at(mouse.column, mouse.row)
    } else {
        None
    };
    if let Some(HitTarget::PaletteRow(index)) = clicked {
        let item = palette_items(app, &palette.query).get(index).cloned();
        app.mode = Mode::Navigation;
        if let Some(item) = item {
            execute_palette_item(client, app, item).await?;
        }
        return Ok(());
    }
    match input {
        Event::Key(key) => match key.code {
            KeyCode::Esc => app.mode = Mode::Navigation,
            KeyCode::Backspace => {
                palette.query.pop();
                palette.selected = 0;
                app.mode = Mode::CommandPalette(palette);
            }
            KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                push_bounded(&mut palette.query, &character.to_string());
                palette.selected = 0;
                app.mode = Mode::CommandPalette(palette);
            }
            KeyCode::Down => {
                palette.selected = (palette.selected + 1)
                    .min(palette_items(app, &palette.query).len().saturating_sub(1));
                app.mode = Mode::CommandPalette(palette);
            }
            KeyCode::Up => {
                palette.selected = palette.selected.saturating_sub(1);
                app.mode = Mode::CommandPalette(palette);
            }
            KeyCode::Enter => {
                let item = palette_items(app, &palette.query)
                    .get(palette.selected)
                    .cloned();
                app.mode = Mode::Navigation;
                if let Some(item) = item {
                    execute_palette_item(client, app, item).await?;
                }
            }
            _ => {}
        },
        Event::Mouse(mouse) if mouse.kind == MouseEventKind::ScrollDown => {
            palette.selected = (palette.selected + 1)
                .min(palette_items(app, &palette.query).len().saturating_sub(1));
            app.mode = Mode::CommandPalette(palette);
        }
        Event::Mouse(mouse) if mouse.kind == MouseEventKind::ScrollUp => {
            palette.selected = palette.selected.saturating_sub(1);
            app.mode = Mode::CommandPalette(palette);
        }
        _ => {}
    }
    Ok(())
}

pub(crate) fn palette_items(app: &App, query: &str) -> Vec<PaletteItem> {
    let mut items = vec![
        PaletteItem::Settings,
        PaletteItem::CreateWorkspace,
        PaletteItem::RegisterProject,
    ];
    items.extend(
        app.snapshot
            .workspaces
            .iter()
            .map(|item| PaletteItem::Workspace(item.id, item.name.clone())),
    );
    items.extend(
        app.visible_projects()
            .into_iter()
            .map(|item| PaletteItem::Project(item.id, item.name.clone())),
    );
    items.extend(
        app.snapshot
            .worktrees
            .iter()
            .filter(|item| {
                app.visible_projects()
                    .iter()
                    .any(|project| project.id == item.project_id)
            })
            .map(|item| PaletteItem::Worktree(item.id, item.name.clone())),
    );
    items.extend(
        app.snapshot
            .sessions
            .iter()
            .map(|item| PaletteItem::Session(item.id, item.display_name.clone())),
    );
    let query = query.to_lowercase();
    items
        .into_iter()
        .filter(|item| item.label().to_lowercase().contains(&query))
        .collect()
}

async fn execute_palette_item(
    client: &DaemonClient,
    app: &mut App,
    item: PaletteItem,
) -> Result<(), DaemonError> {
    match item {
        PaletteItem::Settings => {
            app.open_settings();
            refresh_github_status(client, app).await?;
        }
        PaletteItem::CreateWorkspace => app.mode = Mode::Modal(Form::workspace()),
        PaletteItem::RegisterProject => {
            app.mode = app.active_workspace().map_or_else(
                || Mode::Modal(Form::workspace()),
                |workspace| Mode::Modal(Form::repository(workspace.id)),
            );
        }
        PaletteItem::Workspace(workspace_id, _) => match client
            .request(&ClientRequest::OpenWorkspace { workspace_id })
            .await?
        {
            DaemonResponse::WorkspaceOpened { .. } => {
                app.selected_workspace = true;
                app.selected_project_id = None;
                app.selected_worktree_id = None;
                app.selected_session_id = None;
                app.replace_snapshot(snapshot(client).await?);
                app.flash = Some(Flash::transient(FlashKind::Success, "Workspace opened."));
            }
            response => {
                app.flash = Some(Flash::error(response_message(
                    "workspace selection",
                    response,
                )));
            }
        },
        PaletteItem::Project(id, _) => {
            app.expanded_projects.insert(id);
            app.select_node(ExplorerNode::Project(id));
        }
        PaletteItem::Worktree(id, _) => {
            if let Some(project_id) = app
                .snapshot
                .worktrees
                .iter()
                .find(|item| item.id == id)
                .map(|item| item.project_id)
            {
                app.expanded_projects.insert(project_id);
            }
            app.select_node(ExplorerNode::Worktree(id));
        }
        PaletteItem::Session(id, _) => {
            if let Some(worktree_id) = app
                .snapshot
                .sessions
                .iter()
                .find(|item| item.id == id)
                .map(|item| item.worktree_id)
            {
                app.expanded_worktrees.insert(worktree_id);
                if let Some(project_id) = app
                    .snapshot
                    .worktrees
                    .iter()
                    .find(|item| item.id == worktree_id)
                    .map(|item| item.project_id)
                {
                    app.expanded_projects.insert(project_id);
                }
            }
            app.select_node(ExplorerNode::Session(id));
        }
    }
    Ok(())
}

async fn show_diff(client: &DaemonClient, app: &mut App) -> Result<(), DaemonError> {
    app.diff_scroll = 0;
    let Some(worktree_id) = app.selected_worktree_id else {
        app.flash = Some(Flash::error("Select a worktree or session first."));
        return Ok(());
    };
    match client
        .request(&ClientRequest::GetDiff { worktree_id })
        .await?
    {
        DaemonResponse::Diff(diff) if diff.text.is_empty() => {
            app.diff = Some("Clean worktree — no tracked changes.".into());
        }
        DaemonResponse::Diff(diff) => {
            app.diff = Some(if diff.truncated {
                format!(
                    "{}\n\n[Diff truncated at the configured safety limit]",
                    diff.text
                )
            } else {
                diff.text
            });
        }
        response => app.diff = Some(response_message("Git diff", response)),
    }
    app.set_tab(TuiContentView::Changes);
    Ok(())
}

async fn attach_selected(client: &DaemonClient, app: &mut App) -> Result<(), DaemonError> {
    if app.resume_pending.is_some() || app.resume_response.is_some() {
        return Ok(());
    }
    if let Some(session) = app.selected_session()
        && !lifecycle_is_active(session.status.lifecycle)
    {
        if sylvops_core::provider::session_can_resume(
            session,
            &app.snapshot.sessions,
            &app.providers,
        ) {
            let session_id = session.id;
            resume_session(client, app, session_id);
        } else {
            app.flash = Some(Flash::error(
                "Cannot resume this conversation: it needs a verified saved conversation, an available resume-capable provider, and no newer successor. History is retained; use New to start a fresh session explicitly.",
            ));
        }
        return Ok(());
    }
    attach_selected_process(client, app).await
}

async fn attach_selected_process(client: &DaemonClient, app: &mut App) -> Result<(), DaemonError> {
    let Some(session_id) = app.selected_session_id else {
        app.flash = Some(Flash::error("Select a session first."));
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
            let mut parser = vt100::Parser::new(rows, columns, TERMINAL_SCROLLBACK_ROWS);
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
            app.main_tab = TuiContentView::Terminal;
            app.mode = Mode::TerminalAttached;
            app.flash = Some(Flash::transient(
                FlashKind::Info,
                if role == AttachmentRole::Controller {
                    "Attached as controller · Ctrl+] detaches"
                } else {
                    "Attached as read-only observer · Ctrl+] detaches"
                },
            ));
        }
        response => {
            app.flash = Some(Flash::error(response_message(
                "session attachment",
                response,
            )));
        }
    }
    Ok(())
}

fn resume_session(client: &DaemonClient, app: &mut App, source_session_id: SessionId) {
    if app.resume_response.is_some() {
        return;
    }
    app.resume_pending = Some(source_session_id);
    app.flash = Some(Flash::transient(FlashKind::Info, "Resuming session…"));
    let client = client.clone();
    let (send, receive) = tokio::sync::oneshot::channel();
    app.resume_response = Some(receive);
    tokio::spawn(async move {
        let result = client
            .request(&ClientRequest::ResumeSession {
                session_id: source_session_id,
                columns: 80,
                rows: 24,
            })
            .await;
        let _ = send.send(result);
    });
}

async fn finish_resume(
    client: &DaemonClient,
    app: &mut App,
    response: Result<DaemonResponse, DaemonError>,
) -> Result<(), DaemonError> {
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            app.resume_pending = None;
            app.flash = Some(Flash::error(format!("Could not resume session: {error}")));
            return Ok(());
        }
    };
    let Some(session) = apply_resume_response(app, response) else {
        return Ok(());
    };
    app.replace_snapshot(snapshot(client).await?);
    select_resumed_session(app, &session);
    app.flash = Some(Flash::transient(
        FlashKind::Success,
        "Session resumed into a new history record.",
    ));
    attach_selected_process(client, app).await
}

fn apply_resume_response(
    app: &mut App,
    response: DaemonResponse,
) -> Option<sylvops_core::domain::Session> {
    app.resume_pending = None;
    match response {
        DaemonResponse::SessionResumed { session, .. } => Some(session),
        response => {
            app.flash = Some(Flash::error(response_message("session resume", response)));
            None
        }
    }
}

fn select_resumed_session(app: &mut App, session: &sylvops_core::domain::Session) {
    if let Some(worktree) = app
        .snapshot
        .worktrees
        .iter()
        .find(|worktree| worktree.id == session.worktree_id)
    {
        app.expanded_projects.insert(worktree.project_id);
        app.expanded_worktrees.insert(worktree.id);
    }
    app.select_node(ExplorerNode::Session(session.id));
    app.set_tab(TuiContentView::Terminal);
}

async fn handle_terminal(
    client: &DaemonClient,
    app: &mut App,
    input_pump: &mut Option<InputPump>,
    input: Event,
) -> Result<(), DaemonError> {
    let Some(attached) = app.attached.as_ref() else {
        app.mode = Mode::Navigation;
        return Ok(());
    };
    let session_id = attached.session_id;
    let role = attached.role;
    let clicked_detach = matches!(&input, Event::Mouse(mouse) if mouse.kind == MouseEventKind::Down(MouseButton::Left) && matches!(app.hit_map.target_at(mouse.column, mouse.row), Some(HitTarget::Detach)));
    let history = match &input {
        Event::Key(key) => history_action(*key),
        Event::Mouse(mouse) if mouse.kind == MouseEventKind::Down(MouseButton::Left) => {
            match app.hit_map.target_at(mouse.column, mouse.row) {
                Some(HitTarget::TerminalHistory(action)) => Some(action),
                _ => None,
            }
        }
        _ => None,
    };
    let wheel = match &input {
        Event::Mouse(mouse)
            if matches!(
                mouse.kind,
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
            ) =>
        {
            terminal_wheel_position(app, *mouse)
        }
        _ => None,
    };
    match input {
        Event::Key(key) if is_detach_key(key) => {
            input_pump.take();
            detach(client, app, session_id).await
        }
        Event::Mouse(_) if clicked_detach => {
            input_pump.take();
            detach(client, app, session_id).await
        }
        Event::Mouse(_) if wheel.is_some() => {
            let (scroll_up, row, column) = wheel.expect("wheel guard checked");
            let action = app
                .attached
                .as_mut()
                .map(|terminal| terminal.wheel_action(scroll_up, row, column));
            if let Some(TerminalWheelAction::Application(bytes)) = action {
                enqueue_terminal_input(app, input_pump.as_ref(), bytes);
            }
            Ok(())
        }
        _ if history.is_some() => {
            let action = app.attached.as_mut().map(|terminal| {
                terminal.apply_history_action(history.expect("history guard checked"))
            });
            if let Some(TerminalWheelAction::Application(bytes)) = action {
                enqueue_terminal_input(app, input_pump.as_ref(), bytes);
            }
            Ok(())
        }
        Event::Key(key) if role == AttachmentRole::Controller => {
            if let Some(terminal) = app.attached.as_mut() {
                terminal.scroll_to_latest();
            }
            if let Some(bytes) = encode_key(key) {
                enqueue_terminal_input(app, input_pump.as_ref(), bytes);
            }
            Ok(())
        }
        Event::Paste(text) if role == AttachmentRole::Controller => {
            if let Some(terminal) = app.attached.as_mut() {
                terminal.scroll_to_latest();
            }
            enqueue_terminal_input(app, input_pump.as_ref(), text.into_bytes());
            Ok(())
        }
        _ => Ok(()),
    }
}

fn enqueue_terminal_input(app: &mut App, pump: Option<&InputPump>, bytes: Vec<u8>) {
    let result = pump
        .ok_or("Terminal input is unavailable; detach and reattach.")
        .and_then(|pump| pump.enqueue(bytes));
    if let Err(error) = result {
        app.flash = Some(Flash::error(error));
    }
}

fn terminal_wheel_position(
    app: &App,
    mouse: crossterm::event::MouseEvent,
) -> Option<(bool, u16, u16)> {
    let scroll_up = match mouse.kind {
        MouseEventKind::ScrollUp => true,
        MouseEventKind::ScrollDown => false,
        _ => return None,
    };
    let viewport = app
        .hit_map
        .0
        .iter()
        .rev()
        .find(|region| region.target == HitTarget::TerminalViewport)?
        .area;
    if app.hit_map.target_at(mouse.column, mouse.row) != Some(HitTarget::TerminalViewport) {
        return None;
    }
    Some((
        scroll_up,
        mouse.row.saturating_sub(viewport.y),
        mouse.column.saturating_sub(viewport.x),
    ))
}

async fn detach(
    client: &DaemonClient,
    app: &mut App,
    session_id: SessionId,
) -> Result<(), DaemonError> {
    let _ = client
        .request(&ClientRequest::DetachSession { session_id })
        .await;
    app.attached = None;
    app.mode = Mode::Navigation;
    app.flash = Some(Flash::transient(
        FlashKind::Info,
        "Detached; the daemon still owns the process.",
    ));
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
            app.flash = Some(Flash::error(response_message("terminal resize", response)));
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
        attached.parser =
            vt100::Parser::new(attached.rows, attached.columns, TERMINAL_SCROLLBACK_ROWS);
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

async fn list_providers(
    client: &DaemonClient,
) -> Result<Vec<sylvops_core::provider::ProviderHealth>, DaemonError> {
    match client.request(&ClientRequest::ListProviders).await? {
        DaemonResponse::Providers(providers) => Ok(providers
            .into_iter()
            .filter(|provider| provider.capabilities.interactive)
            .collect()),
        DaemonResponse::Error(error) => Err(DaemonError::Lifecycle(error.message)),
        response => Err(DaemonError::Lifecycle(format!(
            "unexpected provider response: {response:?}"
        ))),
    }
}

async fn load_tui_state(client: &DaemonClient) -> Result<Option<TuiState>, DaemonError> {
    match client.request(&ClientRequest::GetTuiState).await? {
        DaemonResponse::TuiState(state) => Ok(state),
        DaemonResponse::Error(error) => Err(DaemonError::Lifecycle(error.message)),
        response => Err(DaemonError::Lifecycle(format!(
            "unexpected TUI state response: {response:?}"
        ))),
    }
}

async fn save_tui_state(client: &DaemonClient, app: &App) -> Result<(), DaemonError> {
    match client
        .request(&ClientRequest::SaveTuiState {
            state: app.persisted_state(),
        })
        .await?
    {
        DaemonResponse::TuiStateSaved => Ok(()),
        DaemonResponse::Error(error) => Err(DaemonError::Lifecycle(error.message)),
        response => Err(DaemonError::Lifecycle(format!(
            "unexpected TUI state save response: {response:?}"
        ))),
    }
}

async fn save_debounced(client: &DaemonClient, app: &mut App) -> Result<(), DaemonError> {
    if debounced_save_due(app) {
        save_tui_state(client, app).await?;
        app.navigation_dirty_at = None;
    }
    Ok(())
}

fn debounced_save_due(app: &App) -> bool {
    !matches!(&app.mode, Mode::Settings(_))
        && app
            .navigation_dirty_at
            .is_some_and(|changed| changed.elapsed() >= SAVE_DEBOUNCE)
}

fn response_message(operation: &str, response: DaemonResponse) -> String {
    match response {
        DaemonResponse::Error(error) => format!("{operation} failed: {}", error.message),
        response => format!("Unexpected {operation} response: {response:?}"),
    }
}

fn push_bounded(target: &mut String, input: &str) {
    let remaining = MAX_FORM_VALUE_BYTES.saturating_sub(target.len());
    if remaining == 0 {
        return;
    }
    let end = input
        .char_indices()
        .map(|(index, character)| index + character.len_utf8())
        .take_while(|end| *end <= remaining)
        .last()
        .unwrap_or(0);
    target.push_str(&input[..end]);
}

const RESET_TERMINAL_BACKGROUND: &str = "\x1b]111\x1b\\";

#[derive(Debug, Default)]
struct TerminalBackgroundSync {
    applied: Option<TuiTheme>,
}

impl TerminalBackgroundSync {
    fn update(&mut self, theme: TuiTheme) -> Option<String> {
        if self.applied == Some(theme) {
            return None;
        }
        self.applied = Some(theme);
        Some(terminal_background_sequence(theme))
    }
}

fn terminal_background_sequence(theme: TuiTheme) -> String {
    terminal_background_rgb(theme).map_or_else(
        || RESET_TERMINAL_BACKGROUND.to_owned(),
        |(red, green, blue)| format!("\x1b]11;rgb:{red:02x}/{green:02x}/{blue:02x}\x1b\\"),
    )
}

#[derive(Debug, Default)]
struct TerminalGuard {
    background: TerminalBackgroundSync,
}

impl TerminalGuard {
    fn enter() -> Result<Self, DaemonError> {
        enable_raw_mode().map_err(|error| DaemonError::Lifecycle(error.to_string()))?;
        if let Err(error) = execute!(stdout(), EnterAlternateScreen, EnableMouseCapture) {
            let _ = disable_raw_mode();
            return Err(DaemonError::Lifecycle(error.to_string()));
        }
        Ok(Self::default())
    }

    fn sync_theme(&mut self, theme: TuiTheme) {
        if let Some(sequence) = self.background.update(theme) {
            let _ = execute!(stdout(), Print(sequence));
        }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(
            stdout(),
            EndSynchronizedUpdate,
            Print(RESET_TERMINAL_BACKGROUND),
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        let _ = disable_raw_mode();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sylvops_core::domain::{
        Project, ProviderKind, Session, Workspace, Worktree, WorktreeStatus,
    };
    use sylvops_core::github::{
        GitHubBody, GitHubBrowseFailure, GitHubBrowseFailureKind, GitHubChangedFileSummary,
        GitHubCheckState, GitHubCheckSummary, GitHubCommentPage, GitHubCursor, GitHubHost,
        GitHubItemDetail, GitHubItemNumber, GitHubItemState, GitHubItemSummary, GitHubLabelName,
        GitHubListPage, GitHubLogin, GitHubOwner, GitHubPullRequestSummary,
        GitHubRepositoryIdentity, GitHubRepositoryName, GitHubReviewState, GitHubTimestamp,
        GitHubTitle, StartWorkBase, StartWorkBranch, StartWorkIssueContext, StartWorkPreparation,
        WorkSource,
    };
    use sylvops_core::provider::{AuthenticationRequirement, ProviderCapabilities, ProviderHealth};
    use sylvops_core::session_state::{
        AttentionAcknowledgement, AttentionEvent, AttentionEventId, AttentionReason,
        ProviderActivity, SessionLifecycle, SessionStatus,
    };
    use sylvops_core::ui::{DEFAULT_TUI_NAVIGATOR_WIDTH, TuiNavigatorView};

    fn provider_health(kind: ProviderKind) -> ProviderHealth {
        ProviderHealth {
            kind,
            available: true,
            authenticated: true,
            executable_path: None,
            version: None,
            diagnostic: None,
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
                status: SessionStatus {
                    lifecycle: SessionLifecycle::Running,
                    activity: ProviderActivity::None,
                    attention: None,
                },
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
            work_sources: Vec::new(),
            provider_profiles: Vec::new(),
        }
    }

    fn with_pending_attention(
        mut session: Session,
        name: &str,
        reason: AttentionReason,
        event_id: u64,
        occurred_at: i64,
    ) -> Session {
        session.id = SessionId::new();
        session.display_name = name.into();
        session.last_activity_at = occurred_at;
        session.status = SessionStatus {
            lifecycle: if reason == AttentionReason::TerminalFailure {
                SessionLifecycle::Failed
            } else {
                SessionLifecycle::Running
            },
            activity: match reason {
                AttentionReason::InputRequested => ProviderActivity::AwaitingInput,
                AttentionReason::RecoverableFailure => ProviderActivity::RecoverableFailure,
                AttentionReason::SuccessfulCompletion => ProviderActivity::Completed,
                AttentionReason::TerminalFailure => ProviderActivity::None,
            },
            attention: Some(AttentionEvent {
                id: AttentionEventId::try_from(event_id).unwrap(),
                reason,
                acknowledgement: AttentionAcknowledgement::Pending,
                occurred_at,
            }),
        };
        if session.status.lifecycle != SessionLifecycle::Running {
            session.process_id = None;
        }
        session
    }

    fn github_identity(kind: GitHubItemKind, number: u64) -> GitHubItemIdentity {
        GitHubItemIdentity::new(
            GitHubRepositoryIdentity::new(
                GitHubHost::new("github.example.com").unwrap(),
                GitHubOwner::new("acme").unwrap(),
                GitHubRepositoryName::new("widgets").unwrap(),
            ),
            kind,
            GitHubItemNumber::new(number).unwrap(),
        )
    }

    fn github_summary(kind: GitHubItemKind, number: u64) -> GitHubItemSummary {
        GitHubItemSummary {
            identity: github_identity(kind, number),
            state: GitHubItemState::Open,
            title: GitHubTitle::new(format!("Bounded item {number}")).unwrap(),
            author: None,
            labels: vec![GitHubLabelName::new("ready").unwrap()],
            assignees: vec![GitHubLogin::new("maintainer").unwrap()],
            created_at: GitHubTimestamp::new("2026-10-13T12:00:00Z").unwrap(),
            updated_at: GitHubTimestamp::new("2026-10-13T13:00:00Z").unwrap(),
            pull_request: (kind == GitHubItemKind::PullRequest).then_some(
                GitHubPullRequestSummary {
                    head_branch: sylvops_core::github::GitHubBranchName::new("feature").unwrap(),
                    base_branch: sylvops_core::github::GitHubBranchName::new("main").unwrap(),
                    draft: false,
                    review: GitHubReviewState::ReviewRequired,
                    checks: GitHubCheckSummary {
                        state: GitHubCheckState::Pending,
                        total: 3,
                    },
                    changed_files: GitHubChangedFileSummary {
                        total: 4,
                        additions: 12,
                        deletions: 3,
                    },
                },
            ),
        }
    }

    fn github_detail(kind: GitHubItemKind, number: u64) -> GitHubItemDetail {
        let summary = github_summary(kind, number);
        GitHubItemDetail {
            browser_target: summary.identity.browser_target(),
            summary,
            body: GitHubBody::new("Untrusted bounded body\nSecond line").unwrap(),
            comments: GitHubCommentPage {
                comments: Vec::new(),
                next_cursor: Some(GitHubCursor::new("comments-page-2").unwrap()),
            },
        }
    }

    fn start_work_preparation(snapshot: &DaemonSnapshot, number: u64) -> StartWorkPreparation {
        StartWorkPreparation {
            issue: StartWorkIssueContext::try_from(github_detail(GitHubItemKind::Issue, number))
                .unwrap(),
            base: StartWorkBase::new(
                snapshot.worktrees[0].id,
                "main",
                "0123456789abcdef0123456789abcdef01234567",
            )
            .unwrap(),
            suggested_branch: StartWorkBranch::new(format!("issue/{number}-bounded-item")).unwrap(),
            suggested_branch_conflict: false,
            eligible_providers: vec![ProviderKind::Codex],
            existing_work: Vec::new(),
        }
    }

    fn sourced_worktree(snapshot: &DaemonSnapshot, number: u64) -> (Worktree, WorkSource) {
        let project_id = snapshot.projects[0].id;
        let identity = github_identity(GitHubItemKind::Issue, number);
        let worktree = Worktree {
            id: WorktreeId::new(),
            project_id,
            name: format!("Issue #{number}"),
            path: format!("/repo/.sylvops/{number}"),
            canonical_path: format!("/repo/.sylvops/{number}"),
            branch: Some(format!("issue/{number}-bounded-item")),
            base_ref: "main".into(),
            base_commit: "0123456789abcdef0123456789abcdef01234567".into(),
            is_root_checkout: false,
            status: WorktreeStatus::Active,
            created_at: 2,
            last_activity_at: 2,
            removed_at: None,
        };
        let source = WorkSource {
            worktree_id: worktree.id,
            project_id,
            canonical_url: identity.browser_target(),
            item: identity,
            created_at: 2,
        };
        (worktree, source)
    }

    #[test]
    fn session_overview_selection_orders_filters_and_reconciles_from_snapshots() {
        let mut snapshot = populated_snapshot();
        let running_id = snapshot.sessions[0].id;
        snapshot.sessions[0].display_name = "running".into();
        snapshot.sessions[0].last_activity_at = 80;

        let mut starting = snapshot.sessions[0].clone();
        starting.id = SessionId::new();
        starting.display_name = "starting".into();
        starting.status.lifecycle = SessionLifecycle::Starting;
        starting.status.lifecycle = SessionLifecycle::Starting;
        starting.process_id = None;
        starting.last_activity_at = 100;
        let starting_id = starting.id;

        let needs_feedback = with_pending_attention(
            snapshot.sessions[0].clone(),
            "needs feedback",
            AttentionReason::InputRequested,
            1,
            20,
        );
        let needs_feedback_id = needs_feedback.id;

        let mut inactive = snapshot.sessions[0].clone();
        inactive.id = SessionId::new();
        inactive.display_name = "finished history".into();
        inactive.status.lifecycle = SessionLifecycle::Exited;
        inactive.status.lifecycle = SessionLifecycle::Exited;
        inactive.process_id = None;
        inactive.last_activity_at = 200;
        snapshot
            .sessions
            .extend([starting, needs_feedback, inactive]);

        let mut app = App::new(snapshot.clone(), Vec::new(), None);
        app.select_navigator_view(TuiNavigatorView::Active);
        assert_eq!(
            app.session_overview()
                .into_iter()
                .map(|session| session.id)
                .collect::<Vec<_>>(),
            vec![needs_feedback_id, running_id, starting_id]
        );
        assert_eq!(app.selected_session_id, Some(needs_feedback_id));

        app.move_overview_session(2);
        assert_eq!(app.selected_session_id, Some(starting_id));

        snapshot
            .sessions
            .iter_mut()
            .find(|session| session.id == starting_id)
            .unwrap()
            .status
            .lifecycle = SessionLifecycle::Exited;
        snapshot
            .sessions
            .iter_mut()
            .find(|session| session.id == starting_id)
            .unwrap()
            .status
            .lifecycle = SessionLifecycle::Exited;
        app.replace_snapshot(snapshot);
        assert_eq!(app.selected_session_id, Some(needs_feedback_id));
        assert_eq!(
            app.snapshot
                .sessions
                .iter()
                .find(|session| session.id == starting_id)
                .unwrap()
                .display_name,
            "starting"
        );
    }

    #[test]
    fn needs_attention_is_complete_urgent_recent_and_independent_from_active_sessions() {
        let mut snapshot = populated_snapshot();
        let running_id = snapshot.sessions[0].id;
        let base = snapshot.sessions[0].clone();
        let input_old = with_pending_attention(
            base.clone(),
            "input old",
            AttentionReason::InputRequested,
            1,
            100,
        );
        let input_new = with_pending_attention(
            base.clone(),
            "input new",
            AttentionReason::InputRequested,
            2,
            200,
        );
        let recoverable = with_pending_attention(
            base.clone(),
            "recoverable",
            AttentionReason::RecoverableFailure,
            3,
            400,
        );
        let terminal = with_pending_attention(
            base.clone(),
            "terminal",
            AttentionReason::TerminalFailure,
            4,
            500,
        );
        let completion = with_pending_attention(
            base.clone(),
            "completion",
            AttentionReason::SuccessfulCompletion,
            5,
            600,
        );
        let pending_ids = [
            input_new.id,
            input_old.id,
            recoverable.id,
            terminal.id,
            completion.id,
        ];
        snapshot
            .sessions
            .extend([input_old, input_new, recoverable, terminal, completion]);

        let app = App::new(snapshot, Vec::new(), None);
        assert_eq!(
            app.needs_attention_sessions()
                .into_iter()
                .map(|session| session.id)
                .collect::<Vec<_>>(),
            pending_ids
        );
        assert_eq!(
            app.active_sessions()
                .into_iter()
                .map(|session| session.id)
                .collect::<Vec<_>>(),
            vec![
                pending_ids[4],
                pending_ids[2],
                pending_ids[0],
                pending_ids[1],
                running_id,
            ]
        );
    }

    #[test]
    fn rendered_attention_rows_pair_distinct_icons_labels_and_semantic_surfaces() {
        let reasons = [
            (AttentionReason::InputRequested, "?", "Input requested"),
            (
                AttentionReason::RecoverableFailure,
                "!",
                "Recoverable failure",
            ),
            (AttentionReason::TerminalFailure, "×", "Terminal failure"),
            (
                AttentionReason::SuccessfulCompletion,
                "✓",
                "Successful Completion",
            ),
        ];
        let mut snapshot = populated_snapshot();
        let running_id = snapshot.sessions[0].id;
        let base = snapshot.sessions[0].clone();
        let attention = reasons
            .iter()
            .enumerate()
            .map(|(index, (reason, _, _))| {
                with_pending_attention(
                    base.clone(),
                    &format!("attention-{index}"),
                    *reason,
                    u64::try_from(index + 1).unwrap(),
                    i64::try_from(index + 1).unwrap(),
                )
            })
            .collect::<Vec<_>>();
        let ids = attention
            .iter()
            .map(|session| session.id)
            .collect::<Vec<_>>();
        snapshot.sessions.extend(attention);
        let mut app = App::new(snapshot, Vec::new(), None);
        app.select_navigator_view(TuiNavigatorView::Active);
        app.session_presentation = TuiSessionPresentation::List;
        app.navigator_width = 80;

        let mut normal_backgrounds = Vec::new();
        for (index, (_, icon, label)) in reasons.iter().enumerate() {
            app.selected_session_id = Some(running_id);
            let backend = ratatui::backend::TestBackend::new(120, 32);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| render::draw(frame, &mut app))
                .unwrap();
            let normal_area = app
                .hit_map
                .0
                .iter()
                .find(|region| region.target == HitTarget::SessionOverview(ids[index]))
                .unwrap()
                .area;
            let normal = terminal
                .backend()
                .buffer()
                .cell((normal_area.x, normal_area.y))
                .unwrap()
                .bg;
            let text = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>();
            assert!(text.contains(icon));
            assert!(text.contains(label));

            app.selected_session_id = Some(ids[index]);
            terminal
                .draw(|frame| render::draw(frame, &mut app))
                .unwrap();
            let selected_area = app
                .hit_map
                .0
                .iter()
                .find(|region| region.target == HitTarget::SessionOverview(ids[index]))
                .unwrap()
                .area;
            let selected = terminal
                .backend()
                .buffer()
                .cell((selected_area.x, selected_area.y))
                .unwrap()
                .bg;
            assert_ne!(normal, selected, "{label}");
            normal_backgrounds.push(normal);
        }
        normal_backgrounds.sort_by_key(|color| format!("{color:?}"));
        normal_backgrounds.dedup();
        assert_eq!(normal_backgrounds.len(), reasons.len());
    }

    fn render_session_overview(
        snapshot: &DaemonSnapshot,
        width: u16,
        presentation: TuiSessionPresentation,
    ) -> (String, app::HitMap) {
        let backend = ratatui::backend::TestBackend::new(width, 32);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new(snapshot.clone(), Vec::new(), None);
        app.select_navigator_view(TuiNavigatorView::Active);
        app.navigator_width = 80;
        app.session_presentation = presentation;
        terminal
            .draw(|frame| render::draw(frame, &mut app))
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        (text, app.hit_map)
    }

    #[test]
    fn sessions_cards_are_detailed_and_list_rows_are_compact_with_responsive_hit_targets() {
        let mut snapshot = populated_snapshot();
        snapshot.captured_at = 120_000;
        snapshot.sessions[0].display_name = "running shell".into();
        snapshot.sessions[0].last_activity_at = 60_000;
        for (name, lifecycle, provider_activity, activity) in [
            (
                "feedback codex",
                SessionLifecycle::Running,
                ProviderActivity::AwaitingInput,
                119_000,
            ),
            (
                "starting claude",
                SessionLifecycle::Starting,
                ProviderActivity::None,
                118_000,
            ),
        ] {
            let mut session = snapshot.sessions[0].clone();
            session.id = SessionId::new();
            session.display_name = name.into();
            session.status.lifecycle = lifecycle;
            session.status.activity = provider_activity;
            session.last_activity_at = activity;
            snapshot.sessions.push(session);
        }

        let (cards, card_hits) =
            render_session_overview(&snapshot, 140, TuiSessionPresentation::Cards);
        let card_items = card_hits
            .0
            .iter()
            .filter(|region| matches!(region.target, HitTarget::SessionOverview(_)))
            .collect::<Vec<_>>();
        assert_eq!(card_items.len(), 3);
        assert_ne!(card_items[0].area.x, card_items[1].area.x);
        assert_eq!(
            card_hits
                .0
                .iter()
                .filter(|region| matches!(region.target, HitTarget::SessionOverviewOpen(_)))
                .count(),
            4
        );
        assert_eq!(
            card_hits
                .0
                .iter()
                .filter(|region| matches!(region.target, HitTarget::SessionOverviewStop(_)))
                .count(),
            4
        );
        assert!(card_hits.0.iter().any(|region| {
            matches!(region.target, HitTarget::SessionOverviewOpen(_)) && region.area.y == 31
        }));

        let (list, list_hits) =
            render_session_overview(&snapshot, 140, TuiSessionPresentation::List);
        let list_items = list_hits
            .0
            .iter()
            .filter_map(|region| match region.target {
                HitTarget::SessionOverview(_) => Some(region.area),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(list_items.len(), 3);
        assert!(list_items.iter().all(|area| area.height == 1));
        assert!(
            list_items
                .windows(2)
                .all(|pair| { pair[0].x == pair[1].x && pair[0].y.saturating_add(1) == pair[1].y })
        );

        let (_, narrow_hits) =
            render_session_overview(&snapshot, 84, TuiSessionPresentation::Cards);
        let narrow_x = narrow_hits
            .0
            .iter()
            .filter_map(|region| match region.target {
                HitTarget::SessionOverview(_) => Some(region.area.x),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(narrow_x.windows(2).all(|pair| pair[0] == pair[1]));

        for text in [&cards, &list] {
            assert!(text.contains("feedback codex"));
            assert!(text.contains("Needs input"));
        }
        assert!(cards.contains("Provider: Shell"));
        assert!(cards.contains("Project: repo"));
        assert!(cards.contains("Worktree: root"));
        assert!(cards.contains("Activity: 1m ago"));
        assert!(cards.contains("Open terminal"));
        assert!(cards.contains("Stop"));

        assert!(!list.contains("Provider: Shell"));
        assert!(!list.contains("Project: repo"));
        assert!(!list.contains("Worktree: root"));
        assert!(!list.contains("Activity: 1m ago"));
        assert!(!list.contains("Open terminal"));
    }

    #[test]
    fn sessions_navigation_scrolls_selects_and_exposes_explicit_actions() {
        use crossterm::event::{KeyEvent, MouseEvent};

        let mut snapshot = populated_snapshot();
        for index in 1..8 {
            let mut session = snapshot.sessions[0].clone();
            session.id = SessionId::new();
            session.display_name = format!("session {index}");
            session.last_activity_at = i64::from(index);
            snapshot.sessions.push(session);
        }
        let mut app = App::new(snapshot, Vec::new(), None);
        app.focus = app::FocusZone::Explorer;

        let active_key = Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        assert_eq!(
            input::navigation_action(&app, &active_key),
            Some(reducer::Action::SelectNavigatorView(
                TuiNavigatorView::Active
            ))
        );
        reduce(
            &mut app,
            reducer::Action::SelectNavigatorView(TuiNavigatorView::Active),
        );
        let first = app.selected_session_id;
        reduce(&mut app, reducer::Action::Move(1));
        assert_ne!(app.selected_session_id, first);
        assert_eq!(
            reduce(&mut app, reducer::Action::Primary),
            vec![reducer::Effect::Attach]
        );

        for presentation in [TuiSessionPresentation::Cards, TuiSessionPresentation::List] {
            app.session_presentation = presentation;
            app.move_overview_session(1_000);
            let selected = app.selected_session_id.unwrap();
            let backend = ratatui::backend::TestBackend::new(100, 20);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| render::draw(frame, &mut app))
                .unwrap();
            assert!(
                app.hit_map
                    .0
                    .iter()
                    .any(|region| { region.target == HitTarget::SessionOverview(selected) })
            );
        }

        // Compact list rows omit inline actions, so exercise mouse actions on a card render.
        app.session_presentation = TuiSessionPresentation::Cards;
        let backend = ratatui::backend::TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render::draw(frame, &mut app))
            .unwrap();
        let selected = app.selected_session_id.unwrap();
        let open = app
            .hit_map
            .0
            .iter()
            .find(|region| region.target == HitTarget::SessionOverviewOpen(selected))
            .unwrap();
        let open_click = Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: open.area.x,
            row: open.area.y,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(
            input::navigation_action(&app, &open_click),
            Some(reducer::Action::OpenOverviewSession(selected))
        );
        assert_eq!(
            reduce(&mut app, reducer::Action::OpenOverviewSession(selected)),
            vec![reducer::Effect::Attach]
        );
        assert_eq!(
            reduce(&mut app, reducer::Action::StopOverviewSession(selected)),
            vec![reducer::Effect::OpenDelete]
        );

        let stale = SessionId::new();
        assert_eq!(
            reduce(&mut app, reducer::Action::OpenOverviewSession(stale)),
            []
        );
        assert_eq!(
            reduce(&mut app, reducer::Action::StopOverviewSession(stale)),
            []
        );
        assert_eq!(app.selected_session_id, Some(selected));
    }

    #[test]
    fn attention_actions_open_history_or_input_and_only_dismiss_recoverable_failure() {
        let mut snapshot = populated_snapshot();
        let base = snapshot.sessions[0].clone();
        let input =
            with_pending_attention(base.clone(), "input", AttentionReason::InputRequested, 1, 4);
        let recoverable = with_pending_attention(
            base.clone(),
            "recoverable",
            AttentionReason::RecoverableFailure,
            2,
            3,
        );
        let completion = with_pending_attention(
            base,
            "completion",
            AttentionReason::SuccessfulCompletion,
            3,
            2,
        );
        snapshot
            .sessions
            .extend([input.clone(), recoverable.clone(), completion.clone()]);
        let mut app = App::new(snapshot, Vec::new(), None);
        app.select_navigator_view(TuiNavigatorView::Active);

        assert_eq!(app.selected_session_id, Some(input.id));
        assert_eq!(
            reduce(&mut app, reducer::Action::OpenOverviewSession(input.id)),
            vec![reducer::Effect::OpenAttention {
                session_id: input.id,
                attention_id: input.status.attention.unwrap().id,
                reason: AttentionReason::InputRequested,
            }]
        );
        assert_eq!(
            reduce(&mut app, reducer::Action::DismissAttention(recoverable.id)),
            vec![reducer::Effect::DismissAttention {
                session_id: recoverable.id,
                attention_id: recoverable.status.attention.unwrap().id,
            }]
        );
        assert_eq!(
            input::navigation_action(
                &app,
                &Event::Key(crossterm::event::KeyEvent::new(
                    KeyCode::Char('d'),
                    KeyModifiers::NONE,
                )),
            ),
            Some(reducer::Action::DismissAttention(recoverable.id))
        );
        assert_eq!(
            reduce(
                &mut app,
                reducer::Action::OpenOverviewSession(completion.id)
            ),
            vec![reducer::Effect::OpenAttention {
                session_id: completion.id,
                attention_id: completion.status.attention.unwrap().id,
                reason: AttentionReason::SuccessfulCompletion,
            }]
        );
    }

    #[test]
    fn navigator_view_and_mouse_resize_are_persisted_and_bounded() {
        use crossterm::event::{KeyEvent, MouseEvent};

        let persisted = TuiState {
            selected_content_view: TuiContentView::Sessions,
            navigator_view: TuiNavigatorView::Active,
            navigator_width: u16::MAX,
            ..TuiState::default()
        };
        let mut app = App::new(populated_snapshot(), Vec::new(), Some(persisted));
        assert_eq!(app.main_tab, TuiContentView::Terminal);
        assert_eq!(app.navigator_view, TuiNavigatorView::Active);
        assert_eq!(app.navigator_width, 80);

        let backend = ratatui::backend::TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render::draw(frame, &mut app))
            .unwrap();
        let divider = app
            .hit_map
            .0
            .iter()
            .find(|region| region.target == HitTarget::NavigatorDivider)
            .expect("navigator divider")
            .area;
        let down = Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: divider.x,
            row: divider.y,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(
            input::navigation_action(&app, &down),
            Some(reducer::Action::BeginNavigatorResize)
        );
        reduce(&mut app, reducer::Action::BeginNavigatorResize);
        let drag = Event::Mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 50,
            row: divider.y,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(
            input::navigation_action(&app, &drag),
            Some(reducer::Action::ResizeNavigator(50))
        );
        reduce(&mut app, reducer::Action::ResizeNavigator(50));
        assert_eq!(app.navigator_width, 50);

        let explorer_key = Event::Key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
        assert_eq!(
            input::navigation_action(&app, &explorer_key),
            Some(reducer::Action::SelectNavigatorView(
                TuiNavigatorView::Explorer
            ))
        );
        reduce(
            &mut app,
            reducer::Action::SelectNavigatorView(TuiNavigatorView::Explorer),
        );
        let saved = app.persisted_state();
        assert_eq!(saved.navigator_view, TuiNavigatorView::Explorer);
        assert_eq!(saved.navigator_width, 50);
        assert_eq!(saved.selected_content_view, TuiContentView::Terminal);
    }

    #[test]
    fn sessions_empty_state_keeps_context_and_guides_creation_or_selection() {
        let mut snapshot = populated_snapshot();
        snapshot.sessions[0].status.lifecycle = SessionLifecycle::Exited;
        snapshot.sessions[0].status.lifecycle = SessionLifecycle::Exited;
        snapshot.sessions[0].process_id = None;
        let mut app = App::new(snapshot, Vec::new(), None);
        app.select_navigator_view(TuiNavigatorView::Active);
        let backend = ratatui::backend::TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render::draw(frame, &mut app))
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(text.contains("No Active Sessions"));
        assert!(text.contains("Project: repo"));
        assert!(text.contains("Worktree: root"));
        assert!(text.contains("New Session"));
        assert!(
            app.hit_map
                .0
                .iter()
                .any(|region| region.target == HitTarget::New)
        );
        assert!(
            app.explorer_rows()
                .iter()
                .any(|row| { matches!(row.node, ExplorerNode::Session(_)) })
        );

        let project_id = app.snapshot.projects[0].id;
        app.select_node(ExplorerNode::Project(project_id));
        terminal
            .draw(|frame| render::draw(frame, &mut app))
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(text.contains("No Worktree selected"));
        assert!(text.contains("Select an active Worktree"));
    }

    #[test]
    fn successful_resume_selects_successor_and_preserves_source_history() {
        let mut snapshot = populated_snapshot();
        snapshot.sessions[0].provider_kind = ProviderKind::Codex;
        snapshot.sessions[0].status.lifecycle = SessionLifecycle::Exited;
        snapshot.sessions[0].process_id = None;
        snapshot.sessions[0].external_session_id = Some("verified-id".into());
        snapshot.sessions[0].ended_at = Some(2);
        let source = snapshot.sessions[0].clone();
        let mut successor = source.clone();
        successor.id = SessionId::new();
        successor.created_at = 3;
        successor.display_name = "Codex (resumed)".into();
        successor.status.lifecycle = SessionLifecycle::Running;
        successor.ended_at = None;
        let mut app = App::new(snapshot, Vec::new(), None);
        app.snapshot.sessions.push(successor.clone());

        select_resumed_session(&mut app, &successor);

        assert_eq!(app.selected_session_id, Some(successor.id));
        assert_eq!(app.main_tab, TuiContentView::Terminal);
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
    fn stale_resume_error_clears_pending_and_is_actionable() {
        let source_session_id = SessionId::new();
        let mut app = App::new(DaemonSnapshot::default(), Vec::new(), None);
        app.resume_pending = Some(source_session_id);

        let resumed = apply_resume_response(
            &mut app,
            DaemonResponse::Error(sylvops_core::protocol::ProtocolFailure {
                code: "conflict".into(),
                message: "This session has already been resumed.".into(),
                retryable: false,
            }),
        );

        assert!(resumed.is_none());
        assert!(app.resume_pending.is_none());
        assert!(
            app.flash
                .as_ref()
                .is_some_and(|flash| flash.text.contains("already been resumed"))
        );
    }

    #[test]
    fn reconnect_snapshot_discards_stale_ids_and_non_persisted_attachment() {
        let snapshot = populated_snapshot();
        let expected = snapshot.sessions[0].id;
        let app = App::new(
            snapshot,
            Vec::new(),
            Some(TuiState {
                selected_project_id: Some(ProjectId::new()),
                selected_worktree_id: Some(WorktreeId::new()),
                selected_session_id: Some(SessionId::new()),
                selected_content_view: TuiContentView::Details,
                ..TuiState::default()
            }),
        );
        assert_eq!(app.selected_session_id, Some(expected));
        assert_eq!(app.main_tab, TuiContentView::Details);
        assert!(app.attached.is_none());
    }

    #[test]
    fn hierarchy_renders_wide_medium_narrow_and_short() {
        for (width, height) in [(120, 32), (84, 24), (60, 20), (40, 8)] {
            let backend = ratatui::backend::TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            let mut app = App::new(populated_snapshot(), Vec::new(), None);
            terminal
                .draw(|frame| render::draw(frame, &mut app))
                .unwrap();

            let row_text = |row| {
                (0..width)
                    .map(|column| {
                        terminal
                            .backend()
                            .buffer()
                            .cell((column, row))
                            .unwrap()
                            .symbol()
                    })
                    .collect::<String>()
            };
            let header = row_text(0);
            let footer = row_text(height - 1);
            assert!(
                header.contains("Local"),
                "missing Workspace at {width}x{height}"
            );
            assert!(
                header.contains("Settings"),
                "missing Settings at {width}x{height}"
            );
            assert!(
                header.contains("Connected") || header.contains('●'),
                "missing connectivity at {width}x{height}"
            );
            assert!(
                footer.contains("Explorer"),
                "wrong footer at {width}x{height}"
            );
            assert_eq!(
                app.hit_map
                    .0
                    .iter()
                    .find(|region| region.target == HitTarget::Settings)
                    .expect("Settings hit target")
                    .area
                    .y,
                0
            );
            assert!(app.hit_map.0.iter().any(|region| {
                region.target == HitTarget::FocusMain && region.area.y == height - 1
            }));
            assert!(app.hit_map.0.iter().all(|region| {
                region.area.x.saturating_add(region.area.width) <= width
                    && region.area.y.saturating_add(region.area.height) <= height
            }));

            app.snapshot.sessions[0].display_name = "a very long selected Session name".repeat(6);
            app.focus = app::FocusZone::Main;
            terminal
                .draw(|frame| render::draw(frame, &mut app))
                .unwrap();
            let rendered = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>();
            assert!(
                rendered.contains("● Main"),
                "missing explicit Main focus at {width}x{height}"
            );
            assert!(app.hit_map.0.iter().all(|region| {
                region.area.x.saturating_add(region.area.width) <= width
                    && region.area.y.saturating_add(region.area.height) <= height
            }));
        }
    }

    #[test]
    fn chrome_actions_and_shortcuts_follow_focus_and_attention_context() {
        let backend = ratatui::backend::TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new(populated_snapshot(), Vec::new(), None);
        app.snapshot.sessions[0].status.lifecycle = SessionLifecycle::Exited;

        terminal
            .draw(|frame| render::draw(frame, &mut app))
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(rendered.contains("● [e] Explorer"));
        assert!(
            !app.hit_map
                .0
                .iter()
                .any(|region| region.target == HitTarget::Attention)
        );
        let delete_actions = app
            .hit_map
            .0
            .iter()
            .filter(|region| region.target == HitTarget::Delete)
            .collect::<Vec<_>>();
        assert_eq!(delete_actions.len(), 1);
        let delete = delete_actions[0].area;
        assert_eq!(delete.y, 23);
        assert_eq!(
            app.hit_map.target_at(delete.x, delete.y),
            Some(HitTarget::Delete)
        );

        app.snapshot.sessions[0].status.lifecycle = SessionLifecycle::Running;
        app.select_navigator_view(TuiNavigatorView::Active);
        terminal
            .draw(|frame| render::draw(frame, &mut app))
            .unwrap();
        let footer = (0..120)
            .map(|column| {
                terminal
                    .backend()
                    .buffer()
                    .cell((column, 23))
                    .unwrap()
                    .symbol()
            })
            .collect::<String>();
        assert!(footer.contains("Sessions"));
        assert!(footer.contains("Open"));
        assert!(!footer.contains("Rename"));
        let footer_open = app
            .hit_map
            .0
            .iter()
            .find(|region| {
                matches!(region.target, HitTarget::SessionOverviewOpen(_)) && region.area.y == 23
            })
            .expect("footer open action");
        assert_eq!(
            app.hit_map
                .target_at(footer_open.area.x, footer_open.area.y),
            Some(footer_open.target)
        );
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(rendered.contains("● [a] Sessions"));

        app.snapshot.sessions[0].status.lifecycle = SessionLifecycle::Running;
        app.snapshot.sessions[0].status.activity = ProviderActivity::AwaitingInput;
        app.snapshot.sessions[0].status.attention = Some(AttentionEvent {
            id: AttentionEventId::try_from(1).unwrap(),
            reason: AttentionReason::InputRequested,
            acknowledgement: AttentionAcknowledgement::Pending,
            occurred_at: 10,
        });
        terminal
            .draw(|frame| render::draw(frame, &mut app))
            .unwrap();
        let attention = app
            .hit_map
            .0
            .iter()
            .find(|region| region.target == HitTarget::Attention)
            .expect("conditional attention banner");
        assert_eq!(attention.area.height, 1);
        assert_eq!(attention.area.y, 1);
    }

    #[test]
    fn footer_shortcuts_follow_the_active_mode() {
        let backend = ratatui::backend::TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new(populated_snapshot(), Vec::new(), None);

        reduce(&mut app, reducer::Action::Settings);
        terminal
            .draw(|frame| render::draw(frame, &mut app))
            .unwrap();
        let footer = (0..120)
            .map(|column| {
                terminal
                    .backend()
                    .buffer()
                    .cell((column, 23))
                    .unwrap()
                    .symbol()
            })
            .collect::<String>();
        assert!(footer.contains("Settings"));
        assert!(footer.contains("apply"));
        assert!(footer.contains("cancel"));
        assert!(!footer.contains("Open"));

        let session_id = app.snapshot.sessions[0].id;
        app.cancel_settings();
        app.attached = Some(AttachedTerminal {
            session_id,
            role: AttachmentRole::Controller,
            parser: vt100::Parser::new(24, 80, 0),
            last_sequence: 0,
            columns: 80,
            rows: 24,
        });
        app.mode = Mode::TerminalAttached;
        terminal
            .draw(|frame| render::draw(frame, &mut app))
            .unwrap();
        let detach = app
            .hit_map
            .0
            .iter()
            .find(|region| region.target == HitTarget::Detach && region.area.y == 23)
            .expect("footer detach action");
        assert_eq!(
            app.hit_map.target_at(detach.area.x, detach.area.y),
            Some(HitTarget::Detach)
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn settings_input_previews_applies_and_cancels_every_tui_choice() {
        use crossterm::event::{KeyEvent, MouseEvent};
        use sylvops_core::ui::{TuiDensity, TuiTheme};

        let backend = ratatui::backend::TestBackend::new(120, 32);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new(populated_snapshot(), Vec::new(), None);
        terminal
            .draw(|frame| render::draw(frame, &mut app))
            .unwrap();

        let open_key = Event::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        assert_eq!(
            input::navigation_action(&app, &open_key),
            Some(reducer::Action::Settings)
        );
        assert!(matches!(
            palette_items(&app, "settings").as_slice(),
            [PaletteItem::Settings]
        ));

        let header_settings = app
            .hit_map
            .0
            .iter()
            .find(|region| region.target == HitTarget::Settings)
            .cloned()
            .expect("settings header hit target");
        let open_click = Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: header_settings.area.x,
            row: header_settings.area.y,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(
            input::navigation_action(&app, &open_click),
            Some(reducer::Action::Settings)
        );
        reduce(&mut app, reducer::Action::Settings);
        assert!(matches!(app.mode, Mode::Settings(_)));

        terminal
            .draw(|frame| render::draw(frame, &mut app))
            .unwrap();
        for expected in TuiTheme::ALL {
            let region = app
                .hit_map
                .0
                .iter()
                .find(|region| region.target == HitTarget::SettingsTheme(expected))
                .cloned()
                .expect("theme hit target");
            let decision = handle_settings_input(
                &mut app,
                &Event::Mouse(MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: region.area.x,
                    row: region.area.y,
                    modifiers: KeyModifiers::NONE,
                }),
            );
            assert_eq!(decision, SettingsDecision::Continue);
            assert_eq!(app.theme, expected);
        }
        for expected in TuiSessionPresentation::ALL {
            let region = app
                .hit_map
                .0
                .iter()
                .find(|region| region.target == HitTarget::SettingsPresentation(expected))
                .cloned()
                .expect("presentation hit target");
            handle_settings_input(
                &mut app,
                &Event::Mouse(MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: region.area.x,
                    row: region.area.y,
                    modifiers: KeyModifiers::NONE,
                }),
            );
            assert_eq!(app.session_presentation, expected);
        }
        for expected in TuiDensity::ALL {
            let region = app
                .hit_map
                .0
                .iter()
                .find(|region| region.target == HitTarget::SettingsDensity(expected))
                .cloned()
                .expect("density hit target");
            handle_settings_input(
                &mut app,
                &Event::Mouse(MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: region.area.x,
                    row: region.area.y,
                    modifiers: KeyModifiers::NONE,
                }),
            );
            assert_eq!(app.density, expected);
        }

        let apply = app
            .hit_map
            .0
            .iter()
            .find(|region| region.target == HitTarget::SettingsApply)
            .cloned()
            .expect("apply hit target");
        assert_eq!(
            handle_settings_input(
                &mut app,
                &Event::Mouse(MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: apply.area.x,
                    row: apply.area.y,
                    modifiers: KeyModifiers::NONE,
                }),
            ),
            SettingsDecision::Apply
        );
        assert!(matches!(app.mode, Mode::Navigation));
        assert_eq!(app.persisted_state().density, TuiDensity::Compact);
        assert_eq!(
            app.persisted_state().session_presentation,
            TuiSessionPresentation::List
        );

        reduce(&mut app, reducer::Action::Settings);
        handle_settings_input(
            &mut app,
            &Event::Key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)),
        );
        assert_ne!(app.theme, TuiTheme::SolarizedLight);
        handle_settings_input(
            &mut app,
            &Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        );
        handle_settings_input(
            &mut app,
            &Event::Key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)),
        );
        assert_eq!(app.session_presentation, TuiSessionPresentation::Cards);
        assert_eq!(
            handle_settings_input(
                &mut app,
                &Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            ),
            SettingsDecision::Cancel
        );
        assert_eq!(app.theme, TuiTheme::SolarizedLight);
        assert_eq!(app.density, TuiDensity::Compact);
        assert_eq!(app.session_presentation, TuiSessionPresentation::List);

        reduce(&mut app, reducer::Action::Settings);
        handle_settings_input(
            &mut app,
            &Event::Key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)),
        );
        terminal
            .draw(|frame| render::draw(frame, &mut app))
            .unwrap();
        let cancel = app
            .hit_map
            .0
            .iter()
            .find(|region| region.target == HitTarget::SettingsCancel)
            .cloned()
            .expect("cancel hit target");
        assert_eq!(
            handle_settings_input(
                &mut app,
                &Event::Mouse(MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: cancel.area.x,
                    row: cancel.area.y,
                    modifiers: KeyModifiers::NONE,
                }),
            ),
            SettingsDecision::Cancel
        );
        assert_eq!(app.theme, TuiTheme::SolarizedLight);
        assert_eq!(app.density, TuiDensity::Compact);
        assert_eq!(app.session_presentation, TuiSessionPresentation::List);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn density_consistently_changes_chrome_explorer_forms_and_modal_spacing() {
        use ratatui::style::Color;
        use sylvops_core::ui::{TuiDensity, TuiTheme};

        let backend = ratatui::backend::TestBackend::new(120, 32);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new(populated_snapshot(), Vec::new(), None);
        app.theme = TuiTheme::Nord;
        terminal
            .draw(|frame| render::draw(frame, &mut app))
            .unwrap();
        let comfortable_settings_region = app
            .hit_map
            .0
            .iter()
            .find(|region| region.target == HitTarget::Settings)
            .unwrap()
            .area;
        let header_settings = terminal
            .backend()
            .buffer()
            .cell((
                comfortable_settings_region.x.saturating_add(1),
                comfortable_settings_region.y,
            ))
            .unwrap();
        assert_eq!(header_settings.fg, Color::Rgb(136, 192, 208));
        assert_eq!(header_settings.bg, Color::Rgb(46, 52, 64));
        let footer_focus = app
            .hit_map
            .0
            .iter()
            .find(|region| region.target == HitTarget::FocusExplorer && region.area.y == 31)
            .expect("focused footer label");
        assert_eq!(
            terminal
                .backend()
                .buffer()
                .cell((footer_focus.area.x, footer_focus.area.y))
                .unwrap()
                .fg,
            Color::Rgb(136, 192, 208)
        );
        let comfortable_explorer_height = app
            .hit_map
            .0
            .iter()
            .find(|region| matches!(region.target, HitTarget::ExplorerRow(0)))
            .unwrap()
            .area
            .height;
        app.mode = Mode::Modal(Form::worktree(ProjectId::new()));
        terminal
            .draw(|frame| render::draw(frame, &mut app))
            .unwrap();
        let comfortable_fields: Vec<_> = app
            .hit_map
            .0
            .iter()
            .filter_map(|region| match region.target {
                HitTarget::ModalField(index) => Some((index, region.area.y)),
                _ => None,
            })
            .collect();
        let comfortable_submit = app
            .hit_map
            .0
            .iter()
            .find(|region| region.target == HitTarget::ModalSubmit)
            .unwrap()
            .area
            .y;

        app.mode = Mode::Navigation;
        app.density = TuiDensity::Compact;
        terminal
            .draw(|frame| render::draw(frame, &mut app))
            .unwrap();
        let compact_settings_region = app
            .hit_map
            .0
            .iter()
            .find(|region| region.target == HitTarget::Settings)
            .unwrap()
            .area;
        let compact_settings_label = (compact_settings_region.x
            ..compact_settings_region
                .x
                .saturating_add(compact_settings_region.width))
            .map(|column| {
                terminal
                    .backend()
                    .buffer()
                    .cell((column, compact_settings_region.y))
                    .unwrap()
                    .symbol()
            })
            .collect::<String>();
        let compact_explorer_height = app
            .hit_map
            .0
            .iter()
            .find(|region| matches!(region.target, HitTarget::ExplorerRow(0)))
            .unwrap()
            .area
            .height;
        app.mode = Mode::Modal(Form::worktree(ProjectId::new()));
        terminal
            .draw(|frame| render::draw(frame, &mut app))
            .unwrap();
        let compact_fields: Vec<_> = app
            .hit_map
            .0
            .iter()
            .filter_map(|region| match region.target {
                HitTarget::ModalField(index) => Some((index, region.area.y)),
                _ => None,
            })
            .collect();
        let compact_submit = app
            .hit_map
            .0
            .iter()
            .find(|region| region.target == HitTarget::ModalSubmit)
            .unwrap()
            .area
            .y;

        assert_eq!(comfortable_settings_region.y, 0);
        assert_eq!(compact_settings_region.y, 0);
        assert_eq!(compact_settings_label, " [s] Settings ");
        assert_eq!(comfortable_explorer_height, 2);
        assert_eq!(compact_explorer_height, 1);
        assert!(
            comfortable_fields[1].1 - comfortable_fields[0].1
                > compact_fields[1].1 - compact_fields[0].1
        );
        assert!(comfortable_submit > compact_submit);
    }

    #[test]
    fn settings_preview_suspends_pending_debounced_persistence_until_exit() {
        use std::time::Instant;
        use sylvops_core::ui::TuiTheme;

        let mut app = App::new(populated_snapshot(), Vec::new(), None);
        app.navigation_dirty_at = Some(
            Instant::now()
                .checked_sub(SAVE_DEBOUNCE)
                .expect("test duration is smaller than the monotonic clock"),
        );
        assert!(debounced_save_due(&app));

        reduce(&mut app, reducer::Action::Settings);
        handle_settings_input(
            &mut app,
            &Event::Key(crossterm::event::KeyEvent::new(
                KeyCode::Right,
                KeyModifiers::NONE,
            )),
        );
        assert_eq!(app.theme, TuiTheme::Nord);
        assert!(!debounced_save_due(&app));

        handle_settings_input(
            &mut app,
            &Event::Key(crossterm::event::KeyEvent::new(
                KeyCode::Esc,
                KeyModifiers::NONE,
            )),
        );
        assert_eq!(app.theme, TuiTheme::TerminalDefault);
        assert!(debounced_save_due(&app));
    }

    #[test]
    fn settings_expose_github_health_and_project_remote_controls_without_a_session() {
        use sylvops_core::github::{
            GitHubConnection, GitHubConnectionState, GitHubHost, GitHubIntegrationStatus,
            GitHubOwner, GitHubRemote, GitHubRepositoryIdentity, GitHubRepositoryName,
            GitRemoteName,
        };

        let backend = ratatui::backend::TestBackend::new(120, 32);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut snapshot = populated_snapshot();
        snapshot.sessions.clear();
        let mut app = App::new(snapshot, Vec::new(), None);
        app.github_status = Some(GitHubIntegrationStatus {
            executable_path: Some("gh".into()),
            connections: vec![GitHubConnection {
                host: GitHubHost::new("github.example.com").unwrap(),
                state: GitHubConnectionState::Ready,
                diagnostic: "GitHub CLI is ready.".into(),
                checked_at: 1,
            }],
            remotes: vec![GitHubRemote {
                name: GitRemoteName::new("upstream").unwrap(),
                repository: GitHubRepositoryIdentity::new(
                    GitHubHost::new("github.example.com").unwrap(),
                    GitHubOwner::new("acme").unwrap(),
                    GitHubRepositoryName::new("widgets").unwrap(),
                ),
            }],
            association: None,
        });
        app.open_settings();
        terminal
            .draw(|frame| render::draw(frame, &mut app))
            .unwrap();

        let remote = app
            .hit_map
            .0
            .iter()
            .find(|region| region.target == HitTarget::SettingsGithubRemote(0))
            .cloned()
            .expect("GitHub remote control");
        let decision = handle_settings_input(
            &mut app,
            &Event::Mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: remote.area.x,
                row: remote.area.y,
                modifiers: KeyModifiers::NONE,
            }),
        );
        assert_eq!(decision, SettingsDecision::AssociateGitHub(0));
    }

    #[test]
    fn application_theme_does_not_rewrite_provider_terminal_colors() {
        use ratatui::style::Color;
        use sylvops_core::ui::TuiTheme;

        let backend = ratatui::backend::TestBackend::new(120, 32);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new(populated_snapshot(), Vec::new(), None);
        app.set_tab(TuiContentView::Terminal);
        let session_id = app.snapshot.sessions[0].id;
        let mut parser = vt100::Parser::new(24, 80, 0);
        parser.process(b"\x1b[31mR\x1b[0m");
        app.attached = Some(AttachedTerminal {
            session_id,
            role: AttachmentRole::Controller,
            parser,
            last_sequence: 1,
            columns: 80,
            rows: 24,
        });
        app.theme = TuiTheme::SolarizedLight;
        terminal
            .draw(|frame| render::draw(frame, &mut app))
            .unwrap();

        let provider_cell = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .find(|cell| cell.symbol() == "R" && cell.fg == Color::Indexed(1))
            .expect("provider-owned red cell");
        assert_eq!(provider_cell.fg, Color::Indexed(1));
        assert_eq!(provider_cell.bg, Color::Reset);
    }

    #[test]
    fn session_form_renders_claude_from_daemon_capabilities() {
        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut claude = provider_health(ProviderKind::Claude);
        claude.authenticated = false;
        claude.diagnostic = Some(
            "Claude Code is not logged in. Run `claude auth login` yourself, then refresh Provider health."
                .into(),
        );
        let providers = vec![
            provider_health(ProviderKind::Shell),
            provider_health(ProviderKind::Codex),
            claude,
        ];
        let mut app = App::new(DaemonSnapshot::default(), providers.clone(), None);
        let mut form = Form::session(WorktreeId::new(), providers);
        assert!(form.select_provider(ProviderKind::Claude));
        app.mode = Mode::Modal(form);

        terminal
            .draw(|frame| render::draw(frame, &mut app))
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();

        assert!(text.contains("Shell"));
        assert!(text.contains("Codex"));
        assert!(text.contains("Claude Code"));
        assert!(text.contains("setup required"));
        assert!(text.contains("not logged in"));
        assert!(text.contains("Check again"));
    }

    #[test]
    fn wide_layout_exposes_hierarchy_tabs_actions_and_status_text() {
        let backend = ratatui::backend::TestBackend::new(120, 32);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new(populated_snapshot(), Vec::new(), None);
        app.set_tab(TuiContentView::Terminal);
        terminal
            .draw(|frame| render::draw(frame, &mut app))
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        for expected in [
            "Explorer",
            "1 Terminal",
            "2 Changes",
            "3 Details",
            "[a] Sessions",
            "Running",
        ] {
            assert!(
                text.contains(expected),
                "missing {expected:?} in rendered UI"
            );
        }
        assert!(
            app.hit_map
                .0
                .iter()
                .any(|region| matches!(region.target, HitTarget::ExplorerRow(_)))
        );
        assert!(
            app.hit_map
                .0
                .iter()
                .any(|region| region.target == HitTarget::Attach)
        );
    }

    #[test]
    fn session_removal_action_text_distinguishes_stop_from_delete() {
        let backend = ratatui::backend::TestBackend::new(120, 32);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new(populated_snapshot(), Vec::new(), None);

        terminal
            .draw(|frame| render::draw(frame, &mut app))
            .unwrap();
        let running = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(running.contains("[d] Stop Session"));

        app.snapshot.sessions[0].status.lifecycle = SessionLifecycle::Exited;
        terminal
            .draw(|frame| render::draw(frame, &mut app))
            .unwrap();
        let inactive = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(inactive.contains("[d] Delete Session"));
    }

    #[test]
    fn hierarchy_exposes_only_valid_contextual_actions() {
        let snapshot = populated_snapshot();
        let workspace_id = snapshot.workspaces[0].id;
        let project_id = snapshot.projects[0].id;
        let worktree_id = snapshot.worktrees[0].id;
        let session_id = snapshot.sessions[0].id;
        let backend = ratatui::backend::TestBackend::new(120, 32);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new(snapshot, Vec::new(), None);

        for (node, expected_removal, expected_creation, can_rename) in [
            (
                ExplorerNode::Workspace(workspace_id),
                Some("[d] Delete Workspace"),
                Some("[n] Add Project"),
                false,
            ),
            (
                ExplorerNode::Project(project_id),
                Some("[d] Remove Project"),
                Some("[w] New Worktree"),
                true,
            ),
            (
                ExplorerNode::Worktree(worktree_id),
                None,
                Some("[n] New Session"),
                true,
            ),
            (
                ExplorerNode::Session(session_id),
                Some("[d] Stop Session"),
                Some("[Enter] Attach"),
                true,
            ),
        ] {
            app.select_node(node);
            terminal
                .draw(|frame| render::draw(frame, &mut app))
                .unwrap();
            let text = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>();
            if let Some(expected) = expected_removal {
                assert!(text.contains(expected), "missing {expected:?}");
            } else {
                assert!(!text.contains("[d] Remove Worktree"));
            }
            if let Some(expected) = expected_creation {
                assert!(text.contains(expected), "missing {expected:?}");
            }
            assert_eq!(text.contains("[r] Rename"), can_rename);
        }

        app.snapshot.worktrees[0].is_root_checkout = false;
        app.snapshot.sessions[0].status.lifecycle = SessionLifecycle::Exited;
        app.select_node(ExplorerNode::Worktree(worktree_id));
        terminal
            .draw(|frame| render::draw(frame, &mut app))
            .unwrap();
        let removable = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(removable.contains("[d] Remove Worktree"));

        app.snapshot.worktrees[0].status = WorktreeStatus::Missing;
        terminal
            .draw(|frame| render::draw(frame, &mut app))
            .unwrap();
        let missing = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(!missing.contains("[n] New Session"));
        assert!(!missing.contains("[d] Remove Worktree"));
        assert!(missing.contains("[r] Rename"));
    }

    #[test]
    fn worktree_creation_is_available_from_every_project_context() {
        use crossterm::event::{KeyEvent, MouseEvent};

        let snapshot = populated_snapshot();
        let project_id = snapshot.projects[0].id;
        let worktree_id = snapshot.worktrees[0].id;
        let session_id = snapshot.sessions[0].id;
        let backend = ratatui::backend::TestBackend::new(120, 32);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new(snapshot, Vec::new(), None);
        let worktree_key = Event::Key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE));

        for node in [
            ExplorerNode::Project(project_id),
            ExplorerNode::Worktree(worktree_id),
            ExplorerNode::Session(session_id),
        ] {
            app.select_node(node);
            assert_eq!(
                input::navigation_action(&app, &worktree_key),
                Some(reducer::Action::NewWorktree)
            );
            assert_eq!(
                reduce(&mut app, reducer::Action::NewWorktree),
                vec![reducer::Effect::OpenCreateWorktree(project_id)]
            );

            terminal
                .draw(|frame| render::draw(frame, &mut app))
                .unwrap();
            let rendered = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>();
            assert!(rendered.contains("[w] New Worktree"));

            let action = app
                .hit_map
                .0
                .iter()
                .find(|region| region.target == HitTarget::NewWorktree)
                .expect("New Worktree hit target");
            assert_eq!(
                input::navigation_action(
                    &app,
                    &Event::Mouse(MouseEvent {
                        kind: MouseEventKind::Down(MouseButton::Left),
                        column: action.area.x,
                        row: action.area.y,
                        modifiers: KeyModifiers::NONE,
                    }),
                ),
                Some(reducer::Action::NewWorktree)
            );
        }

        app.select_node(ExplorerNode::Worktree(worktree_id));
        assert_eq!(
            input::navigation_action(
                &app,
                &Event::Key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE)),
            ),
            Some(reducer::Action::New)
        );
        open_create_form(&mut app);
        assert!(matches!(
            app.mode,
            Mode::Modal(Form {
                kind: FormKind::CreateSession(id),
                ..
            }) if id == worktree_id
        ));
    }

    #[test]
    fn attention_banner_keeps_the_last_terminal_row_visible() {
        let frame_area = Rect::new(0, 0, 120, 32);
        let normal = render::terminal_panel_area(
            frame_area,
            app::FocusZone::Main,
            DEFAULT_TUI_NAVIGATOR_WIDTH,
            false,
        );
        let attention = render::terminal_panel_area(
            frame_area,
            app::FocusZone::Main,
            DEFAULT_TUI_NAVIGATOR_WIDTH,
            true,
        );
        assert_eq!(normal.height, attention.height.saturating_add(1));

        let mut snapshot = populated_snapshot();
        snapshot.sessions[0] = with_pending_attention(
            snapshot.sessions[0].clone(),
            "shell",
            AttentionReason::InputRequested,
            1,
            10,
        );
        let session_id = snapshot.sessions[0].id;
        let rows = attention.height.saturating_sub(2);
        let columns = attention.width.saturating_sub(2);
        let mut parser = vt100::Parser::new(rows, columns, 0);
        parser.process(format!("\x1b[{rows};1Htyped").as_bytes());
        assert_eq!(
            parser
                .screen()
                .cell(rows.saturating_sub(1), 0)
                .map(vt100::Cell::contents),
            Some("t")
        );
        let mut app = App::new(snapshot, Vec::new(), None);
        app.focus = app::FocusZone::Main;
        app.main_tab = TuiContentView::Terminal;
        app.attached = Some(AttachedTerminal {
            session_id,
            role: AttachmentRole::Controller,
            parser,
            last_sequence: 1,
            columns,
            rows,
        });
        let backend = ratatui::backend::TestBackend::new(frame_area.width, frame_area.height);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| render::draw(frame, &mut app))
            .unwrap();

        let complete_render = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(complete_render.contains("typed"));
        let rendered = (attention.x.saturating_add(1)..attention.x.saturating_add(6))
            .map(|column| {
                terminal
                    .backend()
                    .buffer()
                    .cell((column, attention.bottom().saturating_sub(2)))
                    .unwrap()
                    .symbol()
            })
            .collect::<String>();
        assert_eq!(rendered, "typed");
    }

    #[test]
    fn removal_inspection_requests_are_target_specific() {
        let snapshot = populated_snapshot();
        let workspace_id = snapshot.workspaces[0].id;
        let project_id = snapshot.projects[0].id;
        let worktree_id = snapshot.worktrees[0].id;
        let session_id = snapshot.sessions[0].id;
        let mut app = App::new(snapshot, Vec::new(), None);

        for (node, expected) in [
            (
                ExplorerNode::Workspace(workspace_id),
                ClientRequest::InspectWorkspaceDeletion { workspace_id },
            ),
            (
                ExplorerNode::Project(project_id),
                ClientRequest::InspectProjectRemoval { project_id },
            ),
            (
                ExplorerNode::Worktree(worktree_id),
                ClientRequest::GetWorktreeStatus { worktree_id },
            ),
            (
                ExplorerNode::Session(session_id),
                ClientRequest::InspectSessionDeletion { session_id },
            ),
        ] {
            app.select_node(node);
            assert_eq!(removal_inspection_request(&app), Some(expected));
        }
    }

    #[test]
    fn workspace_inspection_names_target_preservation_and_fallback() {
        let snapshot = populated_snapshot();
        let workspace_id = snapshot.workspaces[0].id;
        let fallback_workspace_id = WorkspaceId::new();
        let mut app = App::new(snapshot, Vec::new(), None);
        app.select_node(ExplorerNode::Workspace(workspace_id));

        apply_removal_inspection(
            &mut app,
            DaemonResponse::WorkspaceDeletionInspected(
                sylvops_core::protocol::WorkspaceDeletionInspection {
                    workspace_id,
                    is_open: true,
                    fallback_workspace_id: Some(fallback_workspace_id),
                    authorization_token: Some("a".repeat(64).try_into().unwrap()),
                    refusals: Vec::new(),
                },
            ),
        );

        let Mode::Confirmation(confirmation) = &app.mode else {
            panic!("authorized deletion should require confirmation");
        };
        assert_eq!(confirmation.title, "Delete Workspace?");
        assert!(confirmation.detail.contains("Local"));
        assert!(confirmation.detail.contains(&workspace_id.to_string()));
        assert!(
            confirmation
                .detail
                .contains("filesystem content is preserved")
        );
        assert!(
            confirmation
                .detail
                .contains(&fallback_workspace_id.to_string())
        );
        assert!(matches!(
            confirmation.action,
            ConfirmAction::DeleteWorkspace { workspace_id: id, .. } if id == workspace_id
        ));
    }

    #[test]
    fn project_worktree_and_session_inspections_describe_exact_effects() {
        let mut snapshot = populated_snapshot();
        let workspace_id = snapshot.workspaces[0].id;
        let project_id = snapshot.projects[0].id;
        let worktree_id = snapshot.worktrees[0].id;
        let session_id = snapshot.sessions[0].id;
        snapshot.worktrees[0].is_root_checkout = false;
        snapshot.worktrees[0].name = "feature".into();
        snapshot.worktrees[0].canonical_path = "/repo/.worktrees/feature".into();
        let mut app = App::new(snapshot, Vec::new(), None);

        app.select_node(ExplorerNode::Project(project_id));
        apply_removal_inspection(
            &mut app,
            DaemonResponse::ProjectRemovalInspected(
                sylvops_core::protocol::ProjectRemovalInspection {
                    project_id,
                    workspace_id,
                    authorization_token: Some("b".repeat(64).try_into().unwrap()),
                    refusals: Vec::new(),
                },
            ),
        );
        let Mode::Confirmation(project) = &app.mode else {
            panic!("authorized Project removal should require confirmation");
        };
        assert_eq!(project.title, "Remove Project?");
        assert!(project.detail.contains("repo"));
        assert!(
            project
                .detail
                .contains("filesystem and Git content are preserved")
        );

        app.snapshot.sessions[0].status.lifecycle = SessionLifecycle::Exited;
        app.select_node(ExplorerNode::Worktree(worktree_id));
        apply_removal_inspection(
            &mut app,
            DaemonResponse::WorktreeStatus(sylvops_core::domain::GitWorktreeState {
                worktree_id,
                head_commit: "abc".into(),
                branch: Some("feature".into()),
                tracked_changes: 0,
                untracked_files: 0,
                ignored_files: 0,
                blocking_paths: Vec::new(),
                blocking_paths_truncated: false,
                clean: true,
                removal_confirmation_token: Some("clean-state-token".into()),
            }),
        );
        let Mode::Confirmation(worktree) = &app.mode else {
            panic!("clean managed Worktree removal should require confirmation");
        };
        assert_eq!(worktree.title, "Remove Worktree?");
        assert!(worktree.detail.contains("/repo/.worktrees/feature"));
        assert!(worktree.detail.contains("filesystem content"));
        assert!(worktree.detail.contains("branch is preserved"));

        app.select_node(ExplorerNode::Session(session_id));
        apply_removal_inspection(
            &mut app,
            DaemonResponse::SessionDeletionInspected(
                sylvops_core::protocol::SessionDeletionInspection {
                    session_id,
                    worktree_id,
                    lifecycle: SessionLifecycle::Running,
                    authorization_token: None,
                    refusal: Some(SessionDeletionRefusal::LiveState),
                },
            ),
        );
        let Mode::Confirmation(stop) = &app.mode else {
            panic!("active Session should offer Stop Session");
        };
        assert_eq!(stop.title, "Stop Session?");
        assert!(stop.detail.contains("descendant processes"));
        assert!(stop.detail.contains("filesystem content are preserved"));

        app.snapshot.sessions[0].status.lifecycle = SessionLifecycle::Exited;
        apply_removal_inspection(
            &mut app,
            DaemonResponse::SessionDeletionInspected(
                sylvops_core::protocol::SessionDeletionInspection {
                    session_id,
                    worktree_id,
                    lifecycle: SessionLifecycle::Exited,
                    authorization_token: Some("c".repeat(64).try_into().unwrap()),
                    refusal: None,
                },
            ),
        );
        let Mode::Confirmation(delete) = &app.mode else {
            panic!("inactive Session should offer Delete Session");
        };
        assert_eq!(delete.title, "Delete Session?");
        assert!(delete.detail.contains("history and metadata are deleted"));
        assert!(delete.detail.contains("filesystem content are preserved"));
    }

    #[test]
    fn removal_refusals_are_bounded_and_include_every_actionable_blocker() {
        let snapshot = populated_snapshot();
        let workspace_id = snapshot.workspaces[0].id;
        let project_id = snapshot.projects[0].id;
        let session_id = snapshot.sessions[0].id;
        let worktree_id = snapshot.worktrees[0].id;
        let mut app = App::new(snapshot, Vec::new(), None);
        app.select_node(ExplorerNode::Project(project_id));

        apply_removal_inspection(
            &mut app,
            DaemonResponse::ProjectRemovalInspected(
                sylvops_core::protocol::ProjectRemovalInspection {
                    project_id,
                    workspace_id,
                    authorization_token: None,
                    refusals: vec![
                        ProjectRemovalRefusal::SessionsRemain {
                            count: 1,
                            truncated: false,
                            ids: vec![session_id],
                        },
                        ProjectRemovalRefusal::ManagedWorktreesRemain {
                            count: 1,
                            truncated: false,
                            ids: vec![worktree_id],
                        },
                    ],
                },
            ),
        );

        let refusal = &app.flash.as_ref().expect("refusal feedback").text;
        assert!(refusal.contains("delete inactive Sessions"));
        assert!(refusal.contains("Remove every managed Worktree"));
        assert!(refusal.chars().count() <= 515);
        assert!(matches!(app.mode, Mode::Navigation));
    }

    #[test]
    fn clean_worktree_is_still_refused_when_live_or_not_active() {
        let mut snapshot = populated_snapshot();
        let worktree_id = snapshot.worktrees[0].id;
        snapshot.worktrees[0].is_root_checkout = false;
        let mut app = App::new(snapshot, Vec::new(), None);
        app.select_node(ExplorerNode::Worktree(worktree_id));
        let clean_status = sylvops_core::domain::GitWorktreeState {
            worktree_id,
            head_commit: "abc".into(),
            branch: Some("feature".into()),
            tracked_changes: 0,
            untracked_files: 0,
            ignored_files: 0,
            blocking_paths: Vec::new(),
            blocking_paths_truncated: false,
            clean: true,
            removal_confirmation_token: Some("clean-state-token".into()),
        };

        apply_removal_inspection(
            &mut app,
            DaemonResponse::WorktreeStatus(clean_status.clone()),
        );
        assert!(
            app.flash
                .as_ref()
                .is_some_and(|flash| flash.text.contains("active Session"))
        );
        assert!(matches!(app.mode, Mode::Navigation));

        app.snapshot.sessions[0].status.lifecycle = SessionLifecycle::Exited;
        app.snapshot.worktrees[0].status = WorktreeStatus::Missing;
        apply_removal_inspection(&mut app, DaemonResponse::WorktreeStatus(clean_status));
        assert!(
            app.flash
                .as_ref()
                .is_some_and(|flash| flash.text.contains("is not active"))
        );
        assert!(matches!(app.mode, Mode::Navigation));
    }

    #[test]
    fn blocked_worktree_dialog_names_ignored_content_and_scrolls_on_narrow_terminals() {
        use sylvops_core::domain::{GitWorktreeState, WorktreeBlockingKind, WorktreeBlockingPath};
        let mut snapshot = populated_snapshot();
        snapshot.sessions[0].status.lifecycle = SessionLifecycle::Exited;
        snapshot.worktrees[0].is_root_checkout = false;
        let worktree_id = snapshot.worktrees[0].id;
        let mut app = App::new(snapshot, Vec::new(), None);
        app.select_node(ExplorerNode::Worktree(worktree_id));
        apply_removal_inspection(
            &mut app,
            DaemonResponse::WorktreeStatus(GitWorktreeState {
                worktree_id,
                head_commit: "abc".into(),
                branch: Some("feature".into()),
                tracked_changes: 0,
                untracked_files: 0,
                ignored_files: 12,
                blocking_paths: vec![WorktreeBlockingPath {
                    kind: WorktreeBlockingKind::Ignored,
                    path: "target/".into(),
                }],
                blocking_paths_truncated: true,
                clean: false,
                removal_confirmation_token: None,
            }),
        );
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render::draw(frame, &mut app))
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(text.contains("Ignored content: target/"));
        assert!(text.contains("Nothing was deleted."));
        assert!(text.contains("and more entries"));

        let mut narrow = Terminal::new(ratatui::backend::TestBackend::new(30, 12)).unwrap();
        narrow.draw(|frame| render::draw(frame, &mut app)).unwrap();
        assert!(matches!(&app.mode, Mode::RemovalBlocked(notice) if notice.max_scroll > 0));
        removal_notice::handle_input(
            &mut app,
            &Event::Key(crossterm::event::KeyEvent::new(
                KeyCode::End,
                KeyModifiers::NONE,
            )),
        );
        assert!(
            matches!(&app.mode, Mode::RemovalBlocked(notice) if notice.scroll == notice.max_scroll)
        );
        removal_notice::handle_input(
            &mut app,
            &Event::Key(crossterm::event::KeyEvent::new(
                KeyCode::Esc,
                KeyModifiers::NONE,
            )),
        );
        assert!(matches!(app.mode, Mode::Navigation));
        assert_eq!(app.selected_worktree_id, Some(worktree_id));
    }

    #[test]
    fn workspace_and_session_refusals_are_actionable() {
        let mut snapshot = populated_snapshot();
        let workspace_id = snapshot.workspaces[0].id;
        let project_id = snapshot.projects[0].id;
        let session_id = snapshot.sessions[0].id;
        let worktree_id = snapshot.worktrees[0].id;
        let mut app = App::new(snapshot.clone(), Vec::new(), None);
        app.select_node(ExplorerNode::Workspace(workspace_id));

        apply_removal_inspection(
            &mut app,
            DaemonResponse::WorkspaceDeletionInspected(
                sylvops_core::protocol::WorkspaceDeletionInspection {
                    workspace_id,
                    is_open: true,
                    fallback_workspace_id: None,
                    authorization_token: None,
                    refusals: vec![WorkspaceDeletionRefusal::ProjectsRemain {
                        count: 1,
                        truncated: false,
                        ids: vec![project_id],
                    }],
                },
            ),
        );
        assert!(
            app.flash
                .as_ref()
                .is_some_and(|flash| flash.text.contains("Remove every Project first"))
        );

        snapshot.sessions[0].status.lifecycle = SessionLifecycle::Exited;
        snapshot.sessions[0].process_id = Some(42);
        app.replace_snapshot(snapshot);
        app.select_node(ExplorerNode::Session(session_id));
        apply_removal_inspection(
            &mut app,
            DaemonResponse::SessionDeletionInspected(
                sylvops_core::protocol::SessionDeletionInspection {
                    session_id,
                    worktree_id,
                    lifecycle: SessionLifecycle::Exited,
                    authorization_token: None,
                    refusal: Some(SessionDeletionRefusal::OwnedProcess),
                },
            ),
        );
        assert!(
            app.flash
                .as_ref()
                .is_some_and(|flash| flash.text.contains("still owns a process tree"))
        );
    }

    #[test]
    fn removal_keyboard_and_cancellation_preserve_selection() {
        use crossterm::event::{KeyEvent, KeyModifiers};

        let snapshot = populated_snapshot();
        let project_id = snapshot.projects[0].id;
        let mut app = App::new(snapshot.clone(), Vec::new(), None);
        app.select_node(ExplorerNode::Project(project_id));
        let selection = app.selected_node();

        assert_eq!(
            input::navigation_action(
                &app,
                &Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)),
            ),
            Some(reducer::Action::Delete)
        );
        assert_eq!(
            reduce(&mut app, reducer::Action::Delete),
            vec![Effect::OpenDelete]
        );
        assert_eq!(
            confirmation_decision(
                &app,
                &Event::Key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))
            ),
            ConfirmationDecision::Confirm
        );
        for code in [KeyCode::Char('n'), KeyCode::Esc] {
            assert_eq!(
                confirmation_decision(&app, &Event::Key(KeyEvent::new(code, KeyModifiers::NONE))),
                ConfirmationDecision::Cancel
            );
        }

        app.mode = Mode::Confirmation(Confirmation {
            title: "Remove Project?".into(),
            detail: "exact target".into(),
            action: ConfirmAction::RemoveProject {
                project_id,
                token: "d".repeat(64).try_into().unwrap(),
            },
        });
        dismiss_confirmation(&mut app);

        assert!(matches!(app.mode, Mode::Navigation));
        assert_eq!(app.selected_node(), selection);
        assert_eq!(app.snapshot, snapshot);
    }

    #[test]
    fn confirmation_render_is_textual_and_names_the_target() {
        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new(populated_snapshot(), Vec::new(), None);
        let session_id = app.snapshot.sessions[0].id;
        app.mode = Mode::Confirmation(Confirmation {
            title: "Delete Session?".into(),
            detail: format!("Delete exact Session {session_id}; filesystem content is preserved."),
            action: ConfirmAction::DeleteSession {
                session_id,
                token: "f".repeat(64).try_into().unwrap(),
            },
        });

        terminal
            .draw(|frame| render::draw(frame, &mut app))
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(text.contains("Delete Session?"));
        assert!(text.contains(&session_id.to_string()));
        assert!(text.contains("filesystem"));
        assert!(text.contains("preserved"));
        assert!(text.contains("Confirm · y"));
        assert!(text.contains("Cancel · Esc"));
    }

    #[test]
    fn narrow_header_keeps_contextual_removal_guidance_visible() {
        let snapshot = populated_snapshot();
        let workspace_id = snapshot.workspaces[0].id;
        let backend = ratatui::backend::TestBackend::new(40, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new(snapshot, Vec::new(), None);
        app.select_node(ExplorerNode::Workspace(workspace_id));

        terminal
            .draw(|frame| render::draw(frame, &mut app))
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(text.contains("[d] Delete Workspace"));
    }

    #[test]
    fn confirmation_requests_keep_authorization_bound_to_each_target() {
        let workspace_id = WorkspaceId::new();
        let project_id = ProjectId::new();
        let worktree_id = WorktreeId::new();
        let session_id = SessionId::new();

        assert!(matches!(
            confirmation_request(&ConfirmAction::DeleteWorkspace {
                workspace_id,
                token: "a".repeat(64).try_into().unwrap(),
            }),
            ClientRequest::DeleteWorkspace { workspace_id: id, .. } if id == workspace_id
        ));
        assert!(matches!(
            confirmation_request(&ConfirmAction::RemoveProject {
                project_id,
                token: "b".repeat(64).try_into().unwrap(),
            }),
            ClientRequest::RemoveProject { project_id: id, .. } if id == project_id
        ));
        assert!(matches!(
            confirmation_request(&ConfirmAction::RemoveWorktree {
                worktree_id,
                token: "clean".into(),
            }),
            ClientRequest::RemoveWorktree { worktree_id: id, .. } if id == worktree_id
        ));
        assert_eq!(
            confirmation_request(&ConfirmAction::StopSession(session_id)),
            ClientRequest::StopSession { session_id }
        );
        assert!(matches!(
            confirmation_request(&ConfirmAction::DeleteSession {
                session_id,
                token: "c".repeat(64).try_into().unwrap(),
            }),
            ClientRequest::DeleteSession { session_id: id, .. } if id == session_id
        ));
    }

    #[test]
    fn confirmation_success_requires_the_matching_daemon_response() {
        let session_id = SessionId::new();
        let worktree_id = WorktreeId::new();
        let action = ConfirmAction::DeleteSession {
            session_id,
            token: "e".repeat(64).try_into().unwrap(),
        };

        assert_eq!(
            confirmation_success_message(
                &action,
                DaemonResponse::SessionDeleted {
                    revision: 2,
                    session_id,
                    worktree_id,
                }
            ),
            Ok("Session deleted.")
        );
        assert!(
            confirmation_success_message(&action, DaemonResponse::Acknowledged)
                .expect_err("wrong response must not be reported as success")
                .contains("unexpected")
        );
        assert_eq!(
            confirmation_success_message(
                &action,
                DaemonResponse::Error(sylvops_core::protocol::ProtocolFailure {
                    code: "conflict".into(),
                    message: "deletion authorization is stale".into(),
                    retryable: false,
                })
            ),
            Err("deletion authorization is stale".into())
        );
    }

    #[test]
    fn removal_snapshot_convergence_selects_survivors_and_closes_attachment() {
        let mut snapshot = populated_snapshot();
        let removed_session_id = snapshot.sessions[0].id;
        let project_id = snapshot.projects[0].id;
        let workspace_id = snapshot.workspaces[0].id;
        let mut sibling = snapshot.sessions[0].clone();
        sibling.id = SessionId::new();
        sibling.display_name = "surviving session".into();
        sibling.last_activity_at = 0;
        let sibling_id = sibling.id;
        snapshot.sessions.push(sibling);
        let mut app = App::new(snapshot.clone(), Vec::new(), None);
        app.select_node(ExplorerNode::Session(removed_session_id));
        app.attached = Some(AttachedTerminal {
            session_id: removed_session_id,
            role: AttachmentRole::Observer,
            parser: vt100::Parser::new(24, 80, 0),
            last_sequence: 0,
            columns: 80,
            rows: 24,
        });
        app.mode = Mode::TerminalAttached;

        snapshot
            .sessions
            .retain(|item| item.id != removed_session_id);
        app.replace_snapshot(snapshot.clone());
        assert_eq!(app.selected_session_id, Some(sibling_id));
        assert!(app.attached.is_none());
        assert!(matches!(app.mode, Mode::Navigation));

        snapshot.sessions.clear();
        snapshot.worktrees[0].status = WorktreeStatus::Removed;
        app.replace_snapshot(snapshot.clone());
        assert_eq!(app.selected_worktree_id, None);
        assert_eq!(app.selected_project_id, Some(project_id));
        assert_eq!(app.selected_node(), Some(ExplorerNode::Project(project_id)));

        snapshot.projects.clear();
        snapshot.worktrees.clear();
        app.replace_snapshot(snapshot.clone());
        assert_eq!(app.selected_project_id, None);
        assert_eq!(
            app.selected_node(),
            Some(ExplorerNode::Workspace(workspace_id))
        );

        let fallback_id = WorkspaceId::new();
        snapshot.workspaces = vec![Workspace {
            id: fallback_id,
            name: "Fallback".into(),
            created_at: 2,
            updated_at: 2,
            last_opened_at: Some(2),
            is_open: true,
        }];
        app.replace_snapshot(snapshot);
        assert_eq!(
            app.active_workspace().map(|item| item.id),
            Some(fallback_id)
        );
        assert_eq!(
            app.selected_node(),
            Some(ExplorerNode::Workspace(fallback_id))
        );
        assert_ne!(fallback_id, workspace_id);
    }

    #[test]
    fn removal_snapshot_selects_the_adjacent_surviving_sibling() {
        let mut snapshot = populated_snapshot();
        let mut middle = snapshot.sessions[0].clone();
        middle.id = SessionId::new();
        middle.display_name = "middle".into();
        middle.last_activity_at = 2;
        let middle_id = middle.id;
        let mut oldest = snapshot.sessions[0].clone();
        oldest.id = SessionId::new();
        oldest.display_name = "oldest".into();
        oldest.last_activity_at = 1;
        let oldest_id = oldest.id;
        snapshot.sessions[0].display_name = "newest".into();
        snapshot.sessions[0].last_activity_at = 3;
        let newest_id = snapshot.sessions[0].id;
        snapshot.sessions.extend([middle, oldest]);
        let mut app = App::new(snapshot.clone(), Vec::new(), None);
        app.select_node(ExplorerNode::Session(middle_id));

        snapshot.sessions.retain(|session| session.id != middle_id);
        app.replace_snapshot(snapshot);

        assert_eq!(app.selected_session_id, Some(oldest_id));
        assert_ne!(app.selected_session_id, Some(newest_id));
    }

    #[test]
    fn removed_worktree_closes_attachment_even_when_session_history_remains() {
        let mut snapshot = populated_snapshot();
        let session_id = snapshot.sessions[0].id;
        snapshot.sessions[0].status.lifecycle = SessionLifecycle::Exited;
        snapshot.sessions[0].process_id = None;
        let mut app = App::new(snapshot.clone(), Vec::new(), None);
        app.attached = Some(AttachedTerminal {
            session_id,
            role: AttachmentRole::Observer,
            parser: vt100::Parser::new(24, 80, 0),
            last_sequence: 0,
            columns: 80,
            rows: 24,
        });
        app.mode = Mode::TerminalAttached;

        snapshot.worktrees[0].status = WorktreeStatus::Removed;
        app.replace_snapshot(snapshot);

        assert!(app.attached.is_none());
        assert!(matches!(app.mode, Mode::Navigation));
        assert!(app.selected_session_id.is_none());
        assert_eq!(app.selected_project_id, Some(app.snapshot.projects[0].id));
    }

    #[test]
    fn non_removed_worktree_states_remain_selected_during_snapshot_refresh() {
        for status in [
            WorktreeStatus::Creating,
            WorktreeStatus::Removing,
            WorktreeStatus::Missing,
            WorktreeStatus::Invalid,
        ] {
            let mut snapshot = populated_snapshot();
            let worktree_id = snapshot.worktrees[0].id;
            snapshot.worktrees[0].status = status;
            let mut app = App::new(snapshot.clone(), Vec::new(), None);
            app.set_tab(TuiContentView::Details);
            app.select_node(ExplorerNode::Worktree(worktree_id));

            app.replace_snapshot(snapshot);

            assert_eq!(
                app.selected_node(),
                Some(ExplorerNode::Worktree(worktree_id)),
                "selection changed for {status}"
            );
        }
    }

    #[test]
    fn every_removal_event_converges_through_an_authoritative_snapshot() {
        let mut snapshot = populated_snapshot();
        let workspace_id = snapshot.workspaces[0].id;
        let project_id = snapshot.projects[0].id;
        let worktree_id = snapshot.worktrees[0].id;
        let session_id = snapshot.sessions[0].id;
        let mut app = App::new(snapshot.clone(), Vec::new(), None);

        let session_event = DaemonEvent::SessionDeleted {
            revision: 2,
            session_id,
            worktree_id,
        };
        assert!(event_requires_snapshot_refresh(&session_event));
        snapshot.sessions.clear();
        app.replace_snapshot(snapshot.clone());
        assert_eq!(
            app.selected_node(),
            Some(ExplorerNode::Worktree(worktree_id))
        );

        let mut removed_worktree = snapshot.worktrees[0].clone();
        removed_worktree.status = WorktreeStatus::Removed;
        let worktree_event = DaemonEvent::WorktreeRemoved {
            revision: 3,
            worktree: removed_worktree.clone(),
        };
        assert!(event_requires_snapshot_refresh(&worktree_event));
        snapshot.worktrees[0] = removed_worktree;
        app.replace_snapshot(snapshot.clone());
        assert_eq!(app.selected_node(), Some(ExplorerNode::Project(project_id)));

        let project_event = DaemonEvent::ProjectRemoved {
            revision: 4,
            project_id,
            workspace_id,
        };
        assert!(event_requires_snapshot_refresh(&project_event));
        snapshot.projects.clear();
        snapshot.worktrees.clear();
        app.replace_snapshot(snapshot.clone());
        assert_eq!(
            app.selected_node(),
            Some(ExplorerNode::Workspace(workspace_id))
        );

        let fallback_id = WorkspaceId::new();
        let workspace_event = DaemonEvent::WorkspaceDeleted {
            revision: 5,
            workspace_id,
            opened_workspace_id: Some(fallback_id),
        };
        assert!(event_requires_snapshot_refresh(&workspace_event));
        snapshot.workspaces = vec![Workspace {
            id: fallback_id,
            name: "Fallback".into(),
            created_at: 2,
            updated_at: 2,
            last_opened_at: Some(2),
            is_open: true,
        }];
        app.replace_snapshot(snapshot);
        assert_eq!(
            app.selected_node(),
            Some(ExplorerNode::Workspace(fallback_id))
        );
    }

    #[test]
    fn narrow_layout_switches_one_area_at_a_time() {
        let backend = ratatui::backend::TestBackend::new(60, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new(populated_snapshot(), Vec::new(), None);
        terminal
            .draw(|frame| render::draw(frame, &mut app))
            .unwrap();
        let explorer = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(explorer.contains("Explorer"));
        assert!(!explorer.contains("1 Terminal"));
        app.focus = app::FocusZone::Main;
        terminal
            .draw(|frame| render::draw(frame, &mut app))
            .unwrap();
        let main = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(main.contains("1 Terminal"));
    }

    #[test]
    fn status_is_not_encoded_by_color_alone() {
        let needs_input = SessionStatus {
            lifecycle: SessionLifecycle::Running,
            activity: ProviderActivity::AwaitingInput,
            attention: None,
        };
        assert_eq!(render::status_symbol(needs_input, 0), "!");
        assert_eq!(render::status_label(needs_input), "Needs input");
        let disconnected = SessionStatus {
            lifecycle: SessionLifecycle::Disconnected,
            ..SessionStatus::default()
        };
        assert_eq!(render::status_symbol(disconnected, 0), "—");
        assert_eq!(render::status_label(disconnected), "Disconnected");
    }

    #[test]
    fn synchronized_frame_encloses_text_and_final_cursor_position() {
        let mut bytes = Vec::<u8>::new();
        let backend = CrosstermBackend::new(&mut bytes);
        let mut terminal = Terminal::with_options(
            backend,
            ratatui::TerminalOptions {
                viewport: ratatui::Viewport::Fixed(Rect::new(0, 0, 20, 4)),
            },
        )
        .unwrap();
        draw_synchronized(&mut terminal, |frame| {
            frame.render_widget(ratatui::widgets::Paragraph::new("echo"), frame.area());
            frame.set_cursor_position((4, 0));
        })
        .unwrap();
        drop(terminal);
        assert!(bytes.starts_with(b"\x1b[?2026h"));
        assert!(bytes.ends_with(b"\x1b[?2026l"));
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("echo"));
        assert!(text.contains("\x1b[1;5H"));
    }

    #[test]
    fn terminal_background_tracks_named_theme_changes_and_resets_to_default() {
        let mut sync = TerminalBackgroundSync::default();
        assert_eq!(
            sync.update(TuiTheme::Nord).as_deref(),
            Some("\x1b]11;rgb:2e/34/40\x1b\\")
        );
        assert_eq!(sync.update(TuiTheme::Nord), None);
        assert_eq!(
            sync.update(TuiTheme::TokyoNight).as_deref(),
            Some("\x1b]11;rgb:1a/1b/26\x1b\\")
        );
        assert_eq!(
            sync.update(TuiTheme::TerminalDefault).as_deref(),
            Some(RESET_TERMINAL_BACKGROUND)
        );
    }

    #[test]
    fn terminal_wheel_coordinates_are_scoped_to_the_rendered_viewport() {
        use crossterm::event::MouseEvent;

        let mut app = App::new(populated_snapshot(), Vec::new(), None);
        app.hit_map
            .push(Rect::new(10, 5, 40, 12), HitTarget::TerminalViewport);
        let inside = MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 13,
            row: 7,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(terminal_wheel_position(&app, inside), Some((true, 2, 3)));

        let outside = MouseEvent {
            column: 3,
            row: 2,
            ..inside
        };
        assert_eq!(terminal_wheel_position(&app, outside), None);
    }

    #[test]
    fn running_and_starting_statuses_use_a_spinning_ring() {
        let running = SessionStatus {
            lifecycle: SessionLifecycle::Running,
            activity: ProviderActivity::Working,
            attention: None,
        };
        let frames = (0..4)
            .map(|frame| render::status_symbol(running, frame))
            .collect::<Vec<_>>();
        assert_eq!(frames, ["◐", "◓", "◑", "◒"]);
        let starting = SessionStatus {
            lifecycle: SessionLifecycle::Starting,
            ..SessionStatus::default()
        };
        assert_eq!(
            render::status_symbol(starting, 5),
            render::status_symbol(running, 1)
        );
    }

    #[test]
    fn attention_navigation_uses_pending_events_instead_of_legacy_state() {
        let mut snapshot = populated_snapshot();
        let mut app = App::new(snapshot.clone(), Vec::new(), None);
        for lifecycle in [
            SessionLifecycle::Starting,
            SessionLifecycle::Running,
            SessionLifecycle::Failed,
            SessionLifecycle::Exited,
        ] {
            snapshot.sessions[0].status.lifecycle = lifecycle;
            snapshot.sessions[0].status.activity = ProviderActivity::None;
            snapshot.sessions[0].status.attention = None;
            app.replace_snapshot(snapshot.clone());
            assert_eq!(app.attention_count(), 0, "{lifecycle:?}");
            assert!(app.select_attention().is_none(), "{lifecycle:?}");
        }

        let pending = with_pending_attention(
            snapshot.sessions[0].clone(),
            "shell",
            AttentionReason::InputRequested,
            1,
            10,
        );
        let session_id = pending.id;
        snapshot.sessions[0] = pending;
        app.replace_snapshot(snapshot);
        assert_eq!(app.attention_count(), 1);
        assert_eq!(
            app.select_attention().as_deref(),
            Some("shell: needs attention")
        );
        assert_eq!(app.selected_session_id, Some(session_id));
    }

    #[test]
    fn attention_banner_copy_uses_correct_grammar() {
        assert!(
            render::attention_message(1).contains("1 session needs attention"),
            "singular attention copy"
        );
        assert!(
            render::attention_message(2).contains("2 sessions need attention"),
            "plural attention copy"
        );
    }

    #[test]
    fn attention_notifications_fire_once_per_transition_and_skip_startup_history() {
        let mut snapshot = populated_snapshot();
        snapshot.sessions[0] = with_pending_attention(
            snapshot.sessions[0].clone(),
            "shell",
            AttentionReason::InputRequested,
            1,
            10,
        );
        let mut tracker = notifications::AttentionTracker::new(&snapshot.sessions);
        assert!(tracker.update(&snapshot.sessions).is_none());
        snapshot.sessions[0].status.activity = ProviderActivity::Working;
        snapshot.sessions[0].status.attention = None;
        assert!(tracker.update(&snapshot.sessions).is_none());
        snapshot.sessions[0] = with_pending_attention(
            snapshot.sessions[0].clone(),
            "shell",
            AttentionReason::InputRequested,
            2,
            11,
        );
        assert_eq!(
            tracker.update(&snapshot.sessions).as_deref(),
            Some("shell needs your input or approval.")
        );
        assert!(tracker.update(&snapshot.sessions).is_none());
        snapshot.sessions[0].display_name = "renamed".into();
        assert!(tracker.update(&snapshot.sessions).is_none());
        snapshot.sessions[0].status.lifecycle = SessionLifecycle::Exited;
        snapshot.sessions[0].status.activity = ProviderActivity::None;
        snapshot.sessions[0].status.attention = None;
        assert!(tracker.update(&snapshot.sessions).is_none());
    }

    #[test]
    fn attention_notification_batches_are_bounded_and_do_not_include_terminal_text() {
        let mut snapshot = populated_snapshot();
        let mut tracker = notifications::AttentionTracker::new(&snapshot.sessions);
        snapshot.sessions[0] = with_pending_attention(
            snapshot.sessions[0].clone(),
            &format!("\x1b{}\n", "界".repeat(1000)),
            AttentionReason::InputRequested,
            1,
            10,
        );
        let mut another = snapshot.sessions[0].clone();
        another.id = SessionId::new();
        snapshot.sessions.push(another);
        let message = tracker.update(&snapshot.sessions).unwrap();
        assert!(message.starts_with("2 sessions need your input or approval"));
        assert!(!message.chars().any(char::is_control));
        assert!(message.chars().count() < 160);
        assert!(tracker.update(&snapshot.sessions).is_none());
    }

    #[test]
    fn terminal_default_attention_surfaces_are_visible_and_selected() {
        let terminal_default = theme::Theme::resolve(TuiTheme::TerminalDefault);
        for reason in [
            AttentionReason::InputRequested,
            AttentionReason::RecoverableFailure,
            AttentionReason::TerminalFailure,
            AttentionReason::SuccessfulCompletion,
        ] {
            let normal = terminal_default.attention_surface(reason, false);
            let selected = terminal_default.attention_surface(reason, true);
            assert!(normal.bg.is_some());
            assert_ne!(normal.bg, selected.bg);
            assert_ne!(normal, terminal_default.text);
        }
    }

    #[test]
    fn hit_map_prefers_topmost_region() {
        let mut map = app::HitMap::default();
        map.push(Rect::new(0, 0, 10, 10), HitTarget::FocusMain);
        map.push(Rect::new(2, 2, 2, 2), HitTarget::Attach);
        assert_eq!(map.target_at(2, 2), Some(HitTarget::Attach));
    }

    #[test]
    fn bounded_paste_preserves_utf8_boundaries() {
        let mut value = "a".repeat(MAX_FORM_VALUE_BYTES - 2);
        push_bounded(&mut value, "éx");
        assert!(value.ends_with('é'));
        assert_eq!(value.len(), MAX_FORM_VALUE_BYTES);
    }

    #[test]
    fn github_shortcuts_cover_nested_navigation_filters_pagination_and_browser_identity() {
        use crossterm::event::KeyEvent;

        let key = |code| Event::Key(KeyEvent::new(code, KeyModifiers::NONE));
        let project_id = ProjectId::new();
        let (mut modal, _) = GitHubModal::open(project_id);
        modal.receive_list(GitHubListPage {
            items: vec![
                github_summary(GitHubItemKind::Issue, 10),
                github_summary(GitHubItemKind::Issue, 11),
            ],
            next_cursor: Some(GitHubCursor::new("page-2").unwrap()),
        });

        assert_eq!(
            github_input_decision(&mut modal, &key(KeyCode::Char('j')), None),
            GitHubDecision::None
        );
        assert_eq!(modal.selected_index, 1);
        github_input_decision(&mut modal, &key(KeyCode::Char('k')), None);
        assert_eq!(modal.selected_index, 0);

        let browser = github_input_decision(&mut modal, &key(KeyCode::Char('o')), None);
        assert_eq!(
            browser,
            GitHubDecision::Browser(github_identity(GitHubItemKind::Issue, 10))
        );
        modal.browser_opened();

        assert!(matches!(
            github_input_decision(&mut modal, &key(KeyCode::Char('n')), None),
            GitHubDecision::List(_)
        ));
        modal.receive_list(GitHubListPage {
            items: vec![github_summary(GitHubItemKind::Issue, 12)],
            next_cursor: None,
        });
        assert!(matches!(
            github_input_decision(&mut modal, &key(KeyCode::Char('b')), None),
            GitHubDecision::List(_)
        ));

        modal.receive_list(GitHubListPage {
            items: vec![github_summary(GitHubItemKind::Issue, 10)],
            next_cursor: None,
        });
        assert!(matches!(
            github_input_decision(&mut modal, &key(KeyCode::Enter), None),
            GitHubDecision::Detail {
                append_comments: false,
                ..
            }
        ));
        modal.receive_detail(github_detail(GitHubItemKind::Issue, 10));
        assert!(matches!(
            github_input_decision(&mut modal, &key(KeyCode::Char('c')), None),
            GitHubDecision::Detail {
                append_comments: true,
                ..
            }
        ));
        assert_eq!(
            github_input_decision(&mut modal, &key(KeyCode::Esc), None),
            GitHubDecision::None
        );
        assert_eq!(modal.depth, GitHubDepth::List);

        github_input_decision(&mut modal, &key(KeyCode::Char('/')), None);
        assert!(modal.filter_editing);
        github_input_decision(&mut modal, &key(KeyCode::Char('b')), None);
        github_input_decision(&mut modal, &key(KeyCode::Char('u')), None);
        github_input_decision(&mut modal, &key(KeyCode::Char('g')), None);
        assert!(matches!(
            github_input_decision(&mut modal, &key(KeyCode::Enter), None),
            GitHubDecision::List(_)
        ));
        assert_eq!(modal.filters.text.as_ref().unwrap().as_str(), "bug");

        assert!(matches!(
            github_input_decision(&mut modal, &key(KeyCode::Char('p')), None),
            GitHubDecision::List(_)
        ));
        assert_eq!(modal.kind, GitHubItemKind::PullRequest);
        modal.receive_list(GitHubListPage {
            items: Vec::new(),
            next_cursor: None,
        });
        assert!(matches!(
            github_input_decision(&mut modal, &key(KeyCode::Char('r')), None),
            GitHubDecision::List(_)
        ));
        assert!(matches!(
            github_input_decision(&mut modal, &key(KeyCode::Char('i')), None),
            GitHubDecision::List(_)
        ));
        assert_eq!(
            github_input_decision(&mut modal, &key(KeyCode::Esc), None),
            GitHubDecision::Close
        );
    }

    #[test]
    fn github_boundaries_filter_recovery_and_click_dispatch_are_state_bound() {
        use crossterm::event::KeyEvent;

        let key = |code| Event::Key(KeyEvent::new(code, KeyModifiers::NONE));
        let (mut modal, _) = GitHubModal::open(ProjectId::new());
        modal.receive_list(GitHubListPage {
            items: vec![
                github_summary(GitHubItemKind::Issue, 1),
                github_summary(GitHubItemKind::Issue, 2),
            ],
            next_cursor: None,
        });
        modal.move_selection(-1);
        assert_eq!(modal.selected_index, 0);
        modal.move_selection(99);
        assert_eq!(modal.selected_index, 1);
        assert!(modal.previous_page().is_none());
        assert!(modal.next_page().is_none());

        github_input_decision(&mut modal, &key(KeyCode::Char('/')), None);
        for character in "draft:true".chars() {
            github_input_decision(&mut modal, &key(KeyCode::Char(character)), None);
        }
        assert_eq!(
            github_input_decision(&mut modal, &key(KeyCode::Enter), None),
            GitHubDecision::None
        );
        assert!(modal.filter_error.is_some());
        github_input_decision(&mut modal, &key(KeyCode::Esc), None);
        assert!(!modal.filter_editing);

        let clicked = github_input_decision(
            &mut modal,
            &Event::FocusGained,
            Some(HitTarget::GitHubRow(0)),
        );
        assert!(matches!(
            clicked,
            GitHubDecision::Detail {
                append_comments: false,
                ..
            }
        ));
        modal.receive_detail(github_detail(GitHubItemKind::Issue, 1));
        assert_eq!(modal.detail_scroll, 0);
        github_input_decision(
            &mut modal,
            &Event::FocusGained,
            Some(HitTarget::GitHubScroll(1)),
        );
        assert_eq!(modal.detail_scroll, 1);
    }

    #[test]
    fn github_async_results_keep_loading_visible_ignore_stale_data_and_recover() {
        let snapshot = populated_snapshot();
        let project_id = snapshot.projects[0].id;
        let (mut modal, _) = GitHubModal::open(project_id);
        modal.register_list_request(1);
        let mut app = App::new(snapshot, Vec::new(), None);
        app.mode = Mode::GitHub(Box::new(modal));

        let render_text = |app: &mut App| {
            let backend = ratatui::backend::TestBackend::new(100, 24);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|frame| render::draw(frame, app)).unwrap();
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>()
        };
        assert!(render_text(&mut app).contains("Loading GitHub items"));

        {
            let Mode::GitHub(modal) = &mut app.mode else {
                panic!("GitHub modal expected");
            };
            modal.switch_kind(GitHubItemKind::PullRequest).unwrap();
            modal.register_list_request(2);
        }
        apply_github_result(
            &mut app,
            GitHubAsyncResult::List {
                request_id: 1,
                project_id,
                response: Ok(DaemonResponse::GitHubItems(GitHubListPage {
                    items: vec![github_summary(GitHubItemKind::Issue, 99)],
                    next_cursor: None,
                })),
            },
        )
        .unwrap();
        assert!(render_text(&mut app).contains("Loading GitHub items"));

        apply_github_result(
            &mut app,
            GitHubAsyncResult::List {
                request_id: 2,
                project_id,
                response: Ok(DaemonResponse::GitHubBrowseFailure(
                    GitHubBrowseFailure::new(GitHubBrowseFailureKind::Network),
                )),
            },
        )
        .unwrap();
        assert!(render_text(&mut app).contains("GitHub is offline"));

        {
            let Mode::GitHub(modal) = &mut app.mode else {
                panic!("GitHub modal expected");
            };
            match modal.refresh().unwrap() {
                GitHubQuery::List(_) => {}
                GitHubQuery::Detail(_) => panic!("list recovery expected"),
            }
            modal.register_list_request(3);
        }
        apply_github_result(
            &mut app,
            GitHubAsyncResult::List {
                request_id: 3,
                project_id,
                response: Ok(DaemonResponse::GitHubItems(GitHubListPage {
                    items: Vec::new(),
                    next_cursor: None,
                })),
            },
        )
        .unwrap();
        assert!(render_text(&mut app).contains("No GitHub items"));
    }

    #[test]
    fn github_request_ids_reject_an_older_result_with_the_same_query() {
        let snapshot = populated_snapshot();
        let project_id = snapshot.projects[0].id;
        let (mut modal, _) = GitHubModal::open(project_id);
        modal.receive_list(GitHubListPage {
            items: Vec::new(),
            next_cursor: None,
        });
        assert!(matches!(modal.refresh(), Some(GitHubQuery::List(_))));
        modal.register_list_request(4);
        modal.register_list_request(5);
        let mut app = App::new(snapshot, Vec::new(), None);
        app.mode = Mode::GitHub(Box::new(modal));

        apply_github_result(
            &mut app,
            GitHubAsyncResult::List {
                request_id: 4,
                project_id,
                response: Ok(DaemonResponse::GitHubItems(GitHubListPage {
                    items: vec![github_summary(GitHubItemKind::PullRequest, 88)],
                    next_cursor: None,
                })),
            },
        )
        .unwrap();
        let Mode::GitHub(modal) = &app.mode else {
            panic!("GitHub modal expected");
        };
        assert!(modal.list.is_loading());
        assert_eq!(modal.list.ready().unwrap().items, []);

        apply_github_result(
            &mut app,
            GitHubAsyncResult::List {
                request_id: 5,
                project_id,
                response: Ok(DaemonResponse::GitHubItems(GitHubListPage {
                    items: Vec::new(),
                    next_cursor: None,
                })),
            },
        )
        .unwrap();
        let Mode::GitHub(modal) = &app.mode else {
            panic!("GitHub modal expected");
        };
        assert!(!modal.list.is_loading());
    }

    #[test]
    fn github_browser_request_uses_only_typed_identity_and_list_failures_are_visible() {
        let snapshot = populated_snapshot();
        let project_id = snapshot.projects[0].id;
        let identity = github_identity(GitHubItemKind::Issue, 126);
        assert_eq!(
            github_browser_request(project_id, identity.clone()),
            ClientRequest::OpenGitHubItem {
                project_id,
                identity: identity.clone(),
            }
        );

        let (mut modal, _) = GitHubModal::open(project_id);
        modal.receive_list(GitHubListPage {
            items: vec![github_summary(GitHubItemKind::Issue, 126)],
            next_cursor: None,
        });
        assert_eq!(modal.begin_browser_open(), Some(identity.clone()));
        modal.register_browser_request(7);
        let mut app = App::new(snapshot, Vec::new(), None);
        app.mode = Mode::GitHub(Box::new(modal));
        apply_github_result(
            &mut app,
            GitHubAsyncResult::Browser {
                request_id: 7,
                project_id,
                response: Err("platform browser unavailable".into()),
            },
        )
        .unwrap();
        let backend = ratatui::backend::TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render::draw(frame, &mut app))
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(text.contains("Browser action failed: platform browser unavailable"));
    }

    #[tokio::test]
    async fn github_request_tasks_are_bounded_and_superseded_work_is_cancelled() {
        let mut tasks = GitHubTasks::default();
        assert_eq!(tasks.next_request_id(), 1);
        assert_eq!(tasks.next_request_id(), 2);

        let first = tokio::spawn(std::future::pending::<()>());
        tasks.replace_list(first.abort_handle());
        let replacement = tokio::spawn(std::future::pending::<()>());
        tasks.replace_list(replacement.abort_handle());
        assert!(first.await.unwrap_err().is_cancelled());

        let detail = tokio::spawn(std::future::pending::<()>());
        tasks.replace_detail(detail.abort_handle());
        let browser = tokio::spawn(std::future::pending::<()>());
        tasks.replace_browser(browser.abort_handle());
        tasks.cancel_all();
        assert!(replacement.await.unwrap_err().is_cancelled());
        assert!(detail.await.unwrap_err().is_cancelled());
        assert!(browser.await.unwrap_err().is_cancelled());
    }

    #[test]
    fn github_modal_renders_contextual_actions_metadata_and_hit_targets() {
        let project_id = populated_snapshot().projects[0].id;
        let (mut modal, _) = GitHubModal::open(project_id);
        modal.receive_list(GitHubListPage {
            items: vec![github_summary(GitHubItemKind::Issue, 126)],
            next_cursor: Some(GitHubCursor::new("next").unwrap()),
        });
        let mut app = App::new(populated_snapshot(), Vec::new(), None);
        app.mode = Mode::GitHub(Box::new(modal));
        let backend = ratatui::backend::TestBackend::new(120, 32);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render::draw(frame, &mut app))
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();

        assert!(text.contains("GitHub · Issues"));
        assert!(text.contains("#126 Bounded item 126"));
        assert!(text.contains("[Enter] Detail"));
        assert!(text.contains("[n] Next"));
        for target in [
            HitTarget::GitHubIssues,
            HitTarget::GitHubPullRequests,
            HitTarget::GitHubRow(0),
            HitTarget::GitHubFilter,
            HitTarget::GitHubRefresh,
            HitTarget::GitHubOpen,
            HitTarget::GitHubNext,
            HitTarget::GitHubBack,
        ] {
            assert!(app.hit_map.0.iter().any(|region| region.target == target));
        }
        assert!(
            !app.hit_map
                .0
                .iter()
                .any(|region| region.target == HitTarget::GitHubPrevious)
        );
    }

    #[test]
    fn github_detail_renders_pull_request_state_and_only_available_actions() {
        let project_id = populated_snapshot().projects[0].id;
        let (mut modal, _) = GitHubModal::open(project_id);
        modal.switch_kind(GitHubItemKind::PullRequest);
        modal.receive_detail(github_detail(GitHubItemKind::PullRequest, 42));
        let mut app = App::new(populated_snapshot(), Vec::new(), None);
        app.mode = Mode::GitHub(Box::new(modal));
        let backend = ratatui::backend::TestBackend::new(120, 32);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render::draw(frame, &mut app))
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();

        assert!(text.contains("feature → main"));
        assert!(text.contains("review required"));
        assert!(text.contains("pending (3)"));
        assert!(text.contains("4 files (+12 −3)"));
        assert!(text.contains("Labels: ready · Assignees: maintainer"));
        assert!(text.contains("[c] More comments"));
        assert!(!text.contains("[n] Next"));

        let Mode::GitHub(modal) = &mut app.mode else {
            panic!("GitHub modal expected");
        };
        assert!(matches!(modal.refresh(), Some(GitHubQuery::Detail(_))));
        terminal
            .draw(|frame| render::draw(frame, &mut app))
            .unwrap();
        let refreshing = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(refreshing.contains("Refreshing GitHub detail"));
        assert!(!refreshing.contains("[c] More comments"));
    }

    #[test]
    fn github_failure_states_have_distinct_rendered_recovery_copy() {
        let cases = [
            (
                GitHubBrowseFailureKind::CliUnavailable,
                "GitHub CLI unavailable",
            ),
            (
                GitHubBrowseFailureKind::Authentication,
                "GitHub authentication required",
            ),
            (
                GitHubBrowseFailureKind::Permission,
                "GitHub Repository access denied",
            ),
            (
                GitHubBrowseFailureKind::RateLimited,
                "GitHub rate limit reached",
            ),
            (GitHubBrowseFailureKind::Network, "GitHub is offline"),
            (
                GitHubBrowseFailureKind::TimedOut,
                "GitHub request timed out",
            ),
            (
                GitHubBrowseFailureKind::MalformedResponse,
                "GitHub returned malformed data",
            ),
            (
                GitHubBrowseFailureKind::OversizedResponse,
                "GitHub response exceeded safety limits",
            ),
            (
                GitHubBrowseFailureKind::UnavailableItem,
                "This GitHub item is unavailable",
            ),
        ];
        for (kind, expected) in cases {
            let project_id = populated_snapshot().projects[0].id;
            let (mut modal, _) = GitHubModal::open(project_id);
            let failure = GitHubBrowseFailure::new(kind);
            modal.fail(
                GitHubRequestTarget::List,
                failure.clone(),
                failure.diagnostic,
            );
            let mut app = App::new(populated_snapshot(), Vec::new(), None);
            app.mode = Mode::GitHub(Box::new(modal));
            let backend = ratatui::backend::TestBackend::new(100, 24);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| render::draw(frame, &mut app))
                .unwrap();
            let text = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>();
            assert!(text.contains(expected), "missing {expected} for {kind:?}");
            assert!(text.contains("[r] Refresh"));
        }
    }

    #[test]
    fn github_issue_start_work_preview_is_keyboard_native_and_pull_requests_exclude_it() {
        use crossterm::event::KeyEvent;

        let key = |code| Event::Key(KeyEvent::new(code, KeyModifiers::NONE));
        let snapshot = populated_snapshot();
        let project_id = snapshot.projects[0].id;
        let (mut modal, _) = GitHubModal::open(project_id);
        modal.receive_list(GitHubListPage {
            items: vec![github_summary(GitHubItemKind::Issue, 129)],
            next_cursor: None,
        });
        assert_eq!(
            github_input_decision(&mut modal, &key(KeyCode::Char('s')), None),
            GitHubDecision::PrepareStartWork {
                issue_number: GitHubItemNumber::new(129).unwrap(),
                start_another_worktree: false,
            }
        );

        let mut preparation = start_work_preparation(&snapshot, 129);
        preparation.issue.body = GitHubBody::new("bounded untrusted line\n".repeat(200)).unwrap();
        modal.receive_start_work_preparation(preparation, false);
        let form = modal.start_work.as_mut().unwrap();
        form.session_name = "Issue 129 TUI".into();
        form.instructions = "Keep the public IPC seam.".into();
        form.error = Some(StartWorkFailure::new(
            StartWorkFailureKind::InvalidRequest,
            "Visible validation feedback.",
        ));
        form.focus = StartWorkFocus::Confirm;
        form.scroll_context(5);
        let mut app = App::new(snapshot, Vec::new(), None);
        app.mode = Mode::GitHub(Box::new(modal));
        let backend = ratatui::backend::TestBackend::new(120, 36);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render::draw(frame, &mut app))
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        for expected in [
            "untrusted reference material",
            "Confirmed base: main @ 0123456789abcdef",
            "Session name (optional): Issue 129 TUI",
            "Additional instructions: Keep the public IPC seam.",
            "Visible validation feedback.",
        ] {
            assert!(text.contains(expected), "missing {expected}");
        }
        let Mode::GitHub(modal) = &mut app.mode else {
            panic!("GitHub modal expected")
        };
        let decision = github_input_decision(modal, &key(KeyCode::Enter), None);
        let GitHubDecision::StartWork(ClientRequest::StartWork {
            provider,
            display_name,
            additional_instructions,
            start_another_worktree,
            ..
        }) = decision
        else {
            panic!("confirmation must use the shared Start Work request")
        };
        assert_eq!(provider, ProviderKind::Codex);
        assert_eq!(display_name.as_deref(), Some("Issue 129 TUI"));
        assert_eq!(
            additional_instructions.unwrap().as_str(),
            "Keep the public IPC seam."
        );
        assert!(!start_another_worktree);
        assert_eq!(
            github_input_decision(modal, &key(KeyCode::Esc), None),
            GitHubDecision::None
        );
        assert_eq!(modal.depth, GitHubDepth::StartWork);

        let (mut pull_request, _) = GitHubModal::open(project_id);
        pull_request.switch_kind(GitHubItemKind::PullRequest);
        pull_request.receive_list(GitHubListPage {
            items: vec![github_summary(GitHubItemKind::PullRequest, 129)],
            next_cursor: None,
        });
        assert_eq!(
            github_input_decision(&mut pull_request, &key(KeyCode::Char('s')), None),
            GitHubDecision::None
        );
        assert_eq!(pull_request.depth, GitHubDepth::List);
    }

    #[test]
    fn github_issue_local_status_drives_duplicate_and_explicit_parallel_actions() {
        use crossterm::event::KeyEvent;

        let key = |code| Event::Key(KeyEvent::new(code, KeyModifiers::NONE));
        let mut snapshot = populated_snapshot();
        let (worktree, source) = sourced_worktree(&snapshot, 129);
        let mut session = snapshot.sessions[0].clone();
        session.id = SessionId::new();
        session.worktree_id = worktree.id;
        session.provider_kind = ProviderKind::Codex;
        session.display_name = "Issue #129".into();
        snapshot.worktrees.push(worktree.clone());
        snapshot.work_sources.push(source);
        snapshot.sessions.push(session);
        let (mut modal, _) = GitHubModal::open(snapshot.projects[0].id);
        modal.sync_local_work(&snapshot);
        let mut summary = github_summary(GitHubItemKind::Issue, 129);
        summary.state = GitHubItemState::Closed;
        modal.receive_list(GitHubListPage {
            items: vec![summary],
            next_cursor: None,
        });
        assert_eq!(
            modal
                .status_for(&github_identity(GitHubItemKind::Issue, 129))
                .display_name(),
            "Session active"
        );
        assert_eq!(
            modal
                .remote_status_for(&github_identity(GitHubItemKind::Issue, 129))
                .unwrap()
                .display_name(),
            "Closed"
        );
        modal.open_selected().unwrap();
        let unavailable = GitHubBrowseFailure::new(GitHubBrowseFailureKind::UnavailableItem);
        modal.fail(
            GitHubRequestTarget::Detail,
            unavailable.clone(),
            unavailable.diagnostic,
        );
        assert_eq!(
            modal
                .remote_status_for(&github_identity(GitHubItemKind::Issue, 129))
                .unwrap()
                .display_name(),
            "Unavailable"
        );
        modal.escape();
        let sourced_session = snapshot.sessions.len() - 1;
        snapshot.sessions[sourced_session].status.lifecycle = SessionLifecycle::Exited;
        snapshot.sessions[sourced_session].ended_at = Some(3);
        modal.sync_local_work(&snapshot);
        assert_eq!(
            modal
                .status_for(&github_identity(GitHubItemKind::Issue, 129))
                .display_name(),
            "Session stopped"
        );
        snapshot.sessions.remove(sourced_session);
        modal.sync_local_work(&snapshot);
        assert_eq!(
            modal
                .status_for(&github_identity(GitHubItemKind::Issue, 129))
                .display_name(),
            "Worktree ready"
        );
        let (mut newest_worktree, mut newest_source) = sourced_worktree(&snapshot, 129);
        newest_worktree.created_at = 9;
        newest_source.created_at = 9;
        snapshot.worktrees.push(newest_worktree.clone());
        snapshot.work_sources.push(newest_source);
        modal.sync_local_work(&snapshot);
        assert_eq!(
            github_input_decision(&mut modal, &key(KeyCode::Char('s')), None),
            GitHubDecision::OpenWorktree(newest_worktree.id)
        );
        assert_eq!(
            github_input_decision(&mut modal, &key(KeyCode::Char('a')), None),
            GitHubDecision::PrepareStartWork {
                issue_number: GitHubItemNumber::new(129).unwrap(),
                start_another_worktree: true,
            }
        );
    }

    #[test]
    fn github_repository_changed_and_recovery_confirmation_use_explicit_copy() {
        let snapshot = populated_snapshot();
        let project_id = snapshot.projects[0].id;
        let identity = github_identity(GitHubItemKind::Issue, 129);
        let (mut modal, _) = GitHubModal::open(project_id);
        modal.open_identity(&identity);
        modal.mark_repository_changed(identity);
        let mut app = App::new(snapshot.clone(), Vec::new(), None);
        app.mode = Mode::GitHub(Box::new(modal));
        let backend = ratatui::backend::TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render::draw(frame, &mut app))
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(text.contains("Repository changed"));
        assert!(text.contains("historical Work Source is preserved"));

        let (worktree, _) = sourced_worktree(&snapshot, 129);
        app.snapshot.worktrees.push(worktree.clone());
        app.mode = Mode::Confirmation(Confirmation {
            title: "Remove Worktree?".into(),
            detail: "generic copy".into(),
            action: ConfirmAction::RemoveWorktree {
                worktree_id: worktree.id,
                token: "bounded-token".into(),
            },
        });
        apply_checkout_recovery_confirmation_copy(&mut app, worktree.id);
        let Mode::Confirmation(confirmation) = &app.mode else {
            panic!("checkout confirmation expected")
        };
        assert_eq!(confirmation.title, "Delete checkout?");
        assert!(confirmation.detail.contains("removed without force"));
        assert!(confirmation.detail.contains("Git branch is preserved"));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn github_start_work_failures_and_partial_success_remain_actionable() {
        use crossterm::event::KeyEvent;

        let key = |code| Event::Key(KeyEvent::new(code, KeyModifiers::NONE));
        let snapshot = populated_snapshot();
        let project_id = snapshot.projects[0].id;
        let (mut modal, _) = GitHubModal::open(project_id);
        modal.receive_list(GitHubListPage {
            items: vec![github_summary(GitHubItemKind::Issue, 129)],
            next_cursor: None,
        });
        modal.open_selected().unwrap();
        modal.receive_detail(github_detail(GitHubItemKind::Issue, 129));
        modal.begin_start_work(false).unwrap();
        modal.receive_start_work_preparation(start_work_preparation(&snapshot, 129), false);
        modal.register_start_work_request(10);
        let mut app = App::new(snapshot.clone(), Vec::new(), None);
        app.mode = Mode::GitHub(Box::new(modal));

        apply_github_result(
            &mut app,
            GitHubAsyncResult::StartWork {
                request_id: 10,
                project_id,
                provider: ProviderKind::Codex,
                response: Ok(DaemonResponse::StartWorkFailure(StartWorkFailure::new(
                    StartWorkFailureKind::BaseMoved,
                    "The confirmed base moved.",
                ))),
            },
        )
        .unwrap();
        let Mode::GitHub(modal) = &mut app.mode else {
            panic!("GitHub modal expected")
        };
        assert_eq!(
            modal
                .start_work
                .as_ref()
                .unwrap()
                .error
                .as_ref()
                .unwrap()
                .kind,
            StartWorkFailureKind::BaseMoved
        );

        modal.register_start_work_request(11);
        apply_github_result(
            &mut app,
            GitHubAsyncResult::StartWork {
                request_id: 11,
                project_id,
                provider: ProviderKind::Codex,
                response: Ok(DaemonResponse::StartWorkFailure(StartWorkFailure::new(
                    StartWorkFailureKind::BranchConflict,
                    "The selected branch already exists.",
                ))),
            },
        )
        .unwrap();
        let Mode::GitHub(modal) = &mut app.mode else {
            panic!("GitHub modal expected")
        };
        assert!(modal.start_work.as_ref().unwrap().branch_conflict.is_some());
        let form = modal.start_work.as_mut().unwrap();
        form.branch = "issue/129-custom-conflict".into();
        modal.register_start_work_request(111);
        apply_github_result(
            &mut app,
            GitHubAsyncResult::StartWork {
                request_id: 111,
                project_id,
                provider: ProviderKind::Codex,
                response: Ok(DaemonResponse::StartWorkFailure(StartWorkFailure::new(
                    StartWorkFailureKind::BranchConflict,
                    "The selected branch already exists.",
                ))),
            },
        )
        .unwrap();
        let Mode::GitHub(modal) = &mut app.mode else {
            panic!("GitHub modal expected")
        };
        let form = modal.start_work.as_mut().unwrap();
        assert_eq!(
            form.branch_conflict.as_deref(),
            Some("issue/129-custom-conflict")
        );
        assert!(form.request(project_id).is_err());
        form.focus = StartWorkFocus::Branch;
        form.push_character('x');
        assert!(form.branch_conflict.is_none());

        modal.register_start_work_request(12);
        let (worktree, source) = sourced_worktree(&snapshot, 129);
        apply_github_result(
            &mut app,
            GitHubAsyncResult::StartWork {
                request_id: 12,
                project_id,
                provider: ProviderKind::Codex,
                response: Ok(DaemonResponse::StartWorkCompleted(
                    StartWorkOutcome::WorktreeCreated {
                        revision: 2,
                        worktree: worktree.clone(),
                        source,
                        failure: StartWorkFailure::new(
                            StartWorkFailureKind::ProviderLaunchFailed,
                            "Provider launch failed.",
                        ),
                    },
                )),
            },
        )
        .unwrap();
        let Mode::GitHub(modal) = &mut app.mode else {
            panic!("recovery modal expected")
        };
        assert_eq!(modal.depth, GitHubDepth::Recovery);
        modal.recovery.as_mut().unwrap().pending = true;
        assert_eq!(
            github_input_decision(modal, &key(KeyCode::Esc), None),
            GitHubDecision::None
        );
        assert_eq!(modal.depth, GitHubDepth::Recovery);
        modal.recovery.as_mut().unwrap().pending = false;
        assert_eq!(
            github_input_decision(modal, &key(KeyCode::Char('s')), None),
            GitHubDecision::RetrySession {
                worktree_id: worktree.id,
                provider: ProviderKind::Codex,
            }
        );
        assert_eq!(
            github_input_decision(modal, &key(KeyCode::Char('o')), None),
            GitHubDecision::OpenWorktree(worktree.id)
        );
        assert_eq!(
            github_input_decision(modal, &key(KeyCode::Char('d')), None),
            GitHubDecision::RemoveWorktree(worktree.id)
        );
        modal.register_start_work_request(13);
        let mut session = snapshot.sessions[0].clone();
        session.id = SessionId::new();
        session.worktree_id = worktree.id;
        session.provider_kind = ProviderKind::Codex;
        let source = app.snapshot.work_sources[0].clone();
        let effect = apply_github_result(
            &mut app,
            GitHubAsyncResult::RetrySession {
                request_id: 13,
                project_id,
                response: Ok(DaemonResponse::StartWorkCompleted(
                    StartWorkOutcome::Started {
                        revision: 3,
                        worktree,
                        source,
                        session: Box::new(session.clone()),
                    },
                )),
            },
        )
        .unwrap();
        assert_eq!(
            effect,
            GitHubResultEffect::Attach {
                session_id: session.id
            }
        );
    }

    #[test]
    fn successful_start_work_selects_the_session_and_requests_terminal_attachment() {
        let snapshot = populated_snapshot();
        let project_id = snapshot.projects[0].id;
        let (worktree, source) = sourced_worktree(&snapshot, 129);
        let mut session = snapshot.sessions[0].clone();
        session.id = SessionId::new();
        session.worktree_id = worktree.id;
        session.provider_kind = ProviderKind::Codex;
        session.display_name = "Issue #129".into();

        let (mut modal, _) = GitHubModal::open(project_id);
        modal.receive_list(GitHubListPage {
            items: vec![github_summary(GitHubItemKind::Issue, 129)],
            next_cursor: None,
        });
        modal.open_selected().unwrap();
        modal.receive_detail(github_detail(GitHubItemKind::Issue, 129));
        modal.begin_start_work(false).unwrap();
        modal.receive_start_work_preparation(start_work_preparation(&snapshot, 129), false);
        modal.start_work.as_mut().unwrap().session_name = "TUI Start Work".into();
        modal.register_start_work_request(20);
        let mut app = App::new(snapshot, Vec::new(), None);
        app.mode = Mode::GitHub(Box::new(modal));

        let effect = apply_github_result(
            &mut app,
            GitHubAsyncResult::StartWork {
                request_id: 20,
                project_id,
                provider: ProviderKind::Codex,
                response: Ok(DaemonResponse::StartWorkCompleted(
                    StartWorkOutcome::Started {
                        revision: 3,
                        worktree: worktree.clone(),
                        source,
                        session: Box::new(session.clone()),
                    },
                )),
            },
        )
        .unwrap();

        assert_eq!(
            effect,
            GitHubResultEffect::Attach {
                session_id: session.id
            }
        );
        assert!(matches!(app.mode, Mode::Navigation));
        assert_eq!(app.selected_worktree_id, Some(worktree.id));
        assert_eq!(app.selected_session_id, Some(session.id));
        assert_eq!(app.main_tab, TuiContentView::Terminal);
        assert!(
            app.snapshot
                .work_sources
                .iter()
                .any(|item| item.worktree_id == worktree.id)
        );
        app.main_tab = TuiContentView::Details;
        let backend = ratatui::backend::TestBackend::new(100, 28);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render::draw(frame, &mut app))
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(text.contains("Work Source"));
        assert!(text.contains("#129"));
        assert!(text.contains("press G to return to Issue detail"));
    }

    #[test]
    fn github_filters_are_excluded_from_persisted_tui_navigation() {
        let snapshot = populated_snapshot();
        let project_id = snapshot.projects[0].id;
        let mut app = App::new(snapshot, Vec::new(), None);
        let persisted_before = app.persisted_state();
        let (mut modal, _) = GitHubModal::open(project_id);
        modal.filter_input = "state:closed label:bug scheduler".into();
        modal.apply_filter().unwrap();
        app.mode = Mode::GitHub(Box::new(modal));

        assert_eq!(app.persisted_state(), persisted_before);
    }

    #[tokio::test]
    async fn directory_browser_cancel_restores_the_form_after_a_filesystem_error() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let workspace_id = populated_snapshot().workspaces[0].id;
        let mut form = Form::repository(workspace_id);
        form.set_repository_path("keep-this-value".into());
        let mut browser = DirectoryBrowser::new(form, temporary.path().into()).await;
        browser.navigate(temporary.path().join("removed")).await;
        assert!(browser.error.is_some());

        let mut app = App::new_at(
            populated_snapshot(),
            Vec::new(),
            None,
            temporary.path().into(),
        );
        app.mode = Mode::DirectoryBrowser(browser);
        restore_directory_browser_form(&mut app, None);

        let Mode::Modal(form) = app.mode else {
            panic!("cancel must restore the registration form");
        };
        assert_eq!(form.repository_path(), Some("keep-this-value"));

        let browser = DirectoryBrowser::new(form, temporary.path().into()).await;
        app.mode = Mode::DirectoryBrowser(browser);
        select_browser_current_directory(&mut app);
        let Mode::Modal(form) = app.mode else {
            panic!("selection must return to the registration form");
        };
        assert_eq!(
            form.repository_path(),
            std::path::absolute(temporary.path())
                .expect("absolute temporary path")
                .to_str()
        );
    }

    #[test]
    fn tui_claude_session_request_keeps_advanced_options_cli_only() {
        let worktree_id = WorktreeId::new();
        let mut form = Form::session(
            worktree_id,
            vec![
                provider_health(ProviderKind::Shell),
                provider_health(ProviderKind::Claude),
            ],
        );
        assert!(form.select_provider(ProviderKind::Claude));

        assert_eq!(
            form.session_request(80, 24),
            Some(ClientRequest::CreateSession {
                worktree_id,
                provider: ProviderKind::Claude,
                display_name: None,
                model: None,
                effort: None,
                initial_prompt: None,
                columns: 80,
                rows: 24,
            })
        );
    }
}
