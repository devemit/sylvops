//! Hierarchical, keyboard-and-mouse mission control backed exclusively by daemon IPC.

mod app;
mod forms;
mod input;
mod reducer;
mod render;
mod terminal;
mod theme;

use std::{io::stdout, time::Duration};

use app::{App, ConfirmAction, Confirmation, ExplorerNode, Flash, FlashKind, HitTarget, Mode};
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
        MouseButton, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use forms::{Form, FormKind};
use ratatui::{Terminal, backend::CrosstermBackend, layout::Rect};
use reducer::{Effect, reduce};
use sylvops_core::{
    domain::{AttachmentRole, DaemonSnapshot, ProviderKind, SessionState},
    ids::{ProjectId, SessionId, WorkspaceId, WorktreeId},
    protocol::{
        ClientRequest, DaemonEvent, DaemonResponse, MAX_PTY_CHUNK_SIZE, MAX_TERMINAL_COLUMNS,
        MAX_TERMINAL_ROWS, MIN_TERMINAL_COLUMNS, MIN_TERMINAL_ROWS,
    },
    ui::{MainTab, TuiState},
};
use sylvops_daemon::{DaemonError, client::DaemonClient, runtime::RuntimePaths};
use terminal::{AttachedTerminal, encode_key, is_detach_key};

const TICK: Duration = Duration::from_millis(60);
const SAVE_DEBOUNCE: Duration = Duration::from_millis(500);
const MAX_FORM_VALUE_BYTES: usize = 8 * 1024;

struct FormOutcome {
    message: String,
    selection: Option<ExplorerNode>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TuiExit {
    Quit,
}

#[derive(Clone, Debug)]
pub(crate) enum PaletteItem {
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
    let client = DaemonClient::connect(paths, "sylvops-tui").await?;
    let mut app = App::new(
        snapshot(&client).await?,
        list_providers(&client).await?,
        load_tui_state(&client).await?,
    );
    let mut events = client.subscribe();
    let _guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))
        .map_err(|error| DaemonError::Lifecycle(format!("cannot initialize TUI: {error}")))?;

    loop {
        app.expire_flash();
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
            app.restore_selection();
        }
        save_debounced(&client, &mut app).await?;
        if matches!(app.mode, Mode::TerminalAttached) {
            let size = terminal
                .size()
                .map_err(|error| DaemonError::Lifecycle(error.to_string()))?;
            let focus = app.focus;
            resize_attached(
                &client,
                &mut app,
                terminal_content_area(Rect::new(0, 0, size.width, size.height), focus),
            )
            .await?;
        }
        terminal
            .draw(|frame| render::draw(frame, &mut app))
            .map_err(|error| DaemonError::Lifecycle(format!("cannot draw TUI: {error}")))?;
        if !event::poll(TICK)
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
        let quit = match app.mode {
            Mode::Navigation => handle_navigation(&client, &mut app, input).await?,
            Mode::Modal(_) => {
                handle_form(&client, &mut app, input).await?;
                false
            }
            Mode::Confirmation(_) => {
                handle_confirmation(&client, &mut app, input).await?;
                false
            }
            Mode::CommandPalette(_) => {
                handle_palette(&client, &mut app, input).await?;
                false
            }
            Mode::TerminalAttached => {
                handle_terminal(&client, &mut app, input).await?;
                false
            }
        };
        if quit {
            save_tui_state(&client, &app).await?;
            return Ok(TuiExit::Quit);
        }
    }
}

async fn handle_navigation(
    client: &DaemonClient,
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
    execute_effects(client, app, effects).await
}

async fn execute_effects(
    client: &DaemonClient,
    app: &mut App,
    effects: Vec<Effect>,
) -> Result<bool, DaemonError> {
    for effect in effects {
        match effect {
            Effect::Attach => attach_selected(client, app).await?,
            Effect::Resume(session_id) => resume_session(client, app, session_id).await?,
            Effect::OpenCreate => open_create_form(app),
            Effect::OpenRename => open_rename_form(app),
            Effect::OpenDelete => open_destructive_confirmation(client, app).await?,
            Effect::LoadDiff => show_diff(client, app).await?,
            Effect::Quit => return Ok(true),
        }
    }
    Ok(false)
}

fn selected_node(app: &App) -> Option<ExplorerNode> {
    app.explorer_rows()
        .get(app.explorer_index)
        .map(|row| row.node)
}

fn open_create_form(app: &mut App) {
    app.mode = match selected_node(app) {
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
        None => None,
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
    match selected_node(app) {
        Some(ExplorerNode::Project(_)) => {
            app.flash = Some(Flash::error(
                "Project deletion is intentionally unavailable.",
            ));
        }
        Some(ExplorerNode::Worktree(id)) => {
            let Some(worktree) = app
                .snapshot
                .worktrees
                .iter()
                .find(|item| item.id == id)
                .cloned()
            else {
                return Ok(());
            };
            if worktree.is_root_checkout {
                app.flash = Some(Flash::error(
                    "The root checkout cannot be removed by SylvOps.",
                ));
                return Ok(());
            }
            match client
                .request(&ClientRequest::GetWorktreeStatus { worktree_id: id })
                .await?
            {
                DaemonResponse::WorktreeStatus(status) if status.clean => {
                    if let Some(token) = status.removal_confirmation_token {
                        app.mode = Mode::Confirmation(Confirmation {
                            title: "Remove clean worktree?".into(),
                            detail: format!(
                                "Remove exactly {}? The Git branch is preserved.",
                                worktree.canonical_path
                            ),
                            action: ConfirmAction::RemoveWorktree {
                                worktree_id: id,
                                token,
                            },
                        });
                    } else {
                        app.flash = Some(Flash::error("No clean-state removal token was issued."));
                    }
                }
                DaemonResponse::WorktreeStatus(status) => {
                    app.flash = Some(Flash::error(format!(
                        "Removal refused: {} tracked, {} untracked, {} ignored entries. Clean the worktree and try again.",
                        status.tracked_changes, status.untracked_files, status.ignored_files
                    )));
                }
                response => {
                    app.flash = Some(Flash::error(response_message("worktree status", response)));
                }
            }
        }
        Some(ExplorerNode::Session(id)) => {
            let Some(session) = app.snapshot.sessions.iter().find(|item| item.id == id) else {
                return Ok(());
            };
            if matches!(
                session.state,
                SessionState::Fresh
                    | SessionState::Starting
                    | SessionState::Running
                    | SessionState::NeedsFeedback
            ) {
                app.mode = Mode::Confirmation(Confirmation {
                    title: "Stop session process tree?".into(),
                    detail: format!(
                        "Stop '{}' and all descendant processes? Session history remains visible.",
                        session.display_name
                    ),
                    action: ConfirmAction::StopSession(id),
                });
            } else {
                app.flash = Some(Flash::transient(
                    FlashKind::Info,
                    "That session is inactive; its history is retained.",
                ));
            }
        }
        None => app.flash = Some(Flash::error("Nothing is selected.")),
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
async fn handle_form(
    client: &DaemonClient,
    app: &mut App,
    input: Event,
) -> Result<(), DaemonError> {
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
    let mut submit = matches!(mouse_target, Some(HitTarget::ModalSubmit));
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
    let form = match &app.mode {
        Mode::Modal(form) => form.clone(),
        _ => return Ok(()),
    };
    match perform_form(client, &form).await {
        Ok(outcome) => {
            app.mode = Mode::Navigation;
            app.flash = Some(Flash::transient(FlashKind::Success, outcome.message));
            app.snapshot = snapshot(client).await?;
            app.restore_selection();
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
                    ExplorerNode::Project(_) => {}
                }
                app.select_node(selection);
            }
        }
        Err(error) => {
            if let Mode::Modal(current) = &mut app.mode {
                current.submission_error = Some(error.to_string());
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
async fn perform_form(client: &DaemonClient, form: &Form) -> Result<FormOutcome, DaemonError> {
    let optional = |index: usize| {
        form.fields
            .get(index)
            .map(|field| field.value.trim())
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
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
            "Repository registered",
            client
                .request(&ClientRequest::AddProject {
                    workspace_id,
                    repository_path: form.fields[0].value.trim().into(),
                })
                .await?,
        ),
        FormKind::CreateWorktree(project_id) => (
            "Worktree created",
            client
                .request(&ClientRequest::CreateWorktree {
                    project_id,
                    branch: form.fields[0].value.trim().into(),
                    name: optional(1),
                    base_ref: optional(2),
                })
                .await?,
        ),
        FormKind::CreateSession(worktree_id) => {
            let provider = form
                .provider()
                .ok_or_else(|| DaemonError::Lifecycle("no provider selected".into()))?
                .kind;
            (
                "Session created",
                client
                    .request(&ClientRequest::CreateSession {
                        worktree_id,
                        provider,
                        display_name: optional(0),
                        model: None,
                        effort: None,
                        initial_prompt: None,
                        columns: 80,
                        rows: 24,
                    })
                    .await?,
            )
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
    let selection = match &response {
        DaemonResponse::ProjectAdded { root_worktree, .. } => {
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
        _ => Ok(FormOutcome {
            message: message.into(),
            selection,
        }),
    }
}

async fn handle_confirmation(
    client: &DaemonClient,
    app: &mut App,
    input: Event,
) -> Result<(), DaemonError> {
    let clicked = if let Event::Mouse(mouse) = &input
        && mouse.kind == MouseEventKind::Down(MouseButton::Left)
    {
        app.hit_map.target_at(mouse.column, mouse.row)
    } else {
        None
    };
    let confirm = matches!(clicked, Some(HitTarget::Confirm))
        || matches!(&input, Event::Key(key) if key.code == KeyCode::Char('y'));
    let cancel = matches!(clicked, Some(HitTarget::Cancel))
        || matches!(&input, Event::Key(key) if matches!(key.code, KeyCode::Esc | KeyCode::Char('n')));
    if cancel {
        app.mode = Mode::Navigation;
        return Ok(());
    }
    if !confirm {
        return Ok(());
    }
    let Mode::Confirmation(dialog) = app.mode.clone() else {
        return Ok(());
    };
    let response = match dialog.action {
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
        app.flash = Some(Flash::error(error.message));
    } else {
        app.flash = Some(Flash::transient(FlashKind::Success, "Operation completed."));
        app.snapshot = snapshot(client).await?;
        app.restore_selection();
    }
    app.mode = Mode::Navigation;
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
    let mut items = vec![PaletteItem::CreateWorkspace, PaletteItem::RegisterProject];
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
                app.snapshot = snapshot(client).await?;
                app.selected_project_id = None;
                app.selected_worktree_id = None;
                app.selected_session_id = None;
                app.restore_selection();
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
    app.set_tab(MainTab::Changes);
    Ok(())
}

async fn attach_selected(client: &DaemonClient, app: &mut App) -> Result<(), DaemonError> {
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
            app.main_tab = MainTab::Terminal;
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

async fn resume_session(
    client: &DaemonClient,
    app: &mut App,
    source_session_id: SessionId,
) -> Result<(), DaemonError> {
    let response = client
        .request(&ClientRequest::ResumeSession {
            session_id: source_session_id,
            columns: 80,
            rows: 24,
        })
        .await;
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            app.resume_pending = None;
            return Err(error);
        }
    };
    let Some(session) = apply_resume_response(app, response) else {
        return Ok(());
    };
    app.snapshot = snapshot(client).await?;
    select_resumed_session(app, &session);
    app.flash = Some(Flash::transient(
        FlashKind::Success,
        "Session resumed into a new history record.",
    ));
    Ok(())
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
    app.set_tab(MainTab::Terminal);
}

async fn handle_terminal(
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
    let clicked_detach = matches!(&input, Event::Mouse(mouse) if mouse.kind == MouseEventKind::Down(MouseButton::Left) && matches!(app.hit_map.target_at(mouse.column, mouse.row), Some(HitTarget::Detach)));
    match input {
        Event::Key(key) if is_detach_key(key) => detach(client, app, session_id).await,
        Event::Mouse(_) if clicked_detach => detach(client, app, session_id).await,
        Event::Key(key) if role == AttachmentRole::Controller => {
            if let Some(bytes) = encode_key(key) {
                let response = client
                    .request(&ClientRequest::SessionInput { session_id, bytes })
                    .await?;
                if !matches!(response, DaemonResponse::Acknowledged) {
                    app.flash = Some(Flash::error(response_message("terminal input", response)));
                }
            }
            Ok(())
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
                    app.flash = Some(Flash::error(response_message("terminal paste", response)));
                    break;
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
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
        attached.parser = vt100::Parser::new(attached.rows, attached.columns, 0);
        attached.parser.process(&snapshot);
        attached.last_sequence = replay_through_sequence;
    }
    Ok(())
}

fn terminal_content_area(area: Rect, focus: app::FocusZone) -> Rect {
    let body = Rect::new(
        area.x,
        area.y.saturating_add(2),
        area.width,
        area.height.saturating_sub(4),
    );
    let main = render::view_areas(body, focus).main.unwrap_or(body);
    Rect::new(
        main.x,
        main.y.saturating_add(3),
        main.width,
        main.height.saturating_sub(4),
    )
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
            .filter(|provider| matches!(provider.kind, ProviderKind::Shell | ProviderKind::Codex))
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
    if app
        .navigation_dirty_at
        .is_some_and(|changed| changed.elapsed() >= SAVE_DEBOUNCE)
    {
        save_tui_state(client, app).await?;
        app.navigation_dirty_at = None;
    }
    Ok(())
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

#[derive(Debug)]
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self, DaemonError> {
        enable_raw_mode().map_err(|error| DaemonError::Lifecycle(error.to_string()))?;
        if let Err(error) = execute!(stdout(), EnterAlternateScreen, EnableMouseCapture) {
            let _ = disable_raw_mode();
            return Err(DaemonError::Lifecycle(error.to_string()));
        }
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(stdout(), DisableMouseCapture, LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sylvops_core::domain::{Project, Session, Workspace, Worktree, WorktreeStatus};

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
    fn successful_resume_selects_successor_and_preserves_source_history() {
        let mut snapshot = populated_snapshot();
        snapshot.sessions[0].provider_kind = ProviderKind::Codex;
        snapshot.sessions[0].state = SessionState::FinishedSeen;
        snapshot.sessions[0].process_id = None;
        snapshot.sessions[0].external_session_id = Some("verified-id".into());
        snapshot.sessions[0].ended_at = Some(2);
        let source = snapshot.sessions[0].clone();
        let mut successor = source.clone();
        successor.id = SessionId::new();
        successor.created_at = 3;
        successor.display_name = "Codex (resumed)".into();
        successor.state = SessionState::Running;
        successor.ended_at = None;
        let mut app = App::new(snapshot, Vec::new(), None);
        app.snapshot.sessions.push(successor.clone());

        select_resumed_session(&mut app, &successor);

        assert_eq!(app.selected_session_id, Some(successor.id));
        assert_eq!(app.main_tab, MainTab::Terminal);
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
        assert!(!sylvops_core::domain::session_can_resume(
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
    fn persisted_selection_is_restored_and_stale_ids_fall_back() {
        let snapshot = populated_snapshot();
        let expected = snapshot.sessions[0].id;
        let app = App::new(
            snapshot,
            Vec::new(),
            Some(TuiState {
                selected_project_id: Some(ProjectId::new()),
                selected_worktree_id: Some(WorktreeId::new()),
                selected_session_id: Some(SessionId::new()),
                selected_main_tab: MainTab::Details,
            }),
        );
        assert_eq!(app.selected_session_id, Some(expected));
        assert_eq!(app.main_tab, MainTab::Details);
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
        }
    }

    #[test]
    fn wide_layout_exposes_hierarchy_tabs_actions_and_status_text() {
        let backend = ratatui::backend::TestBackend::new(120, 32);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new(populated_snapshot(), Vec::new(), None);
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
        assert_eq!(render::status_symbol(SessionState::NeedsFeedback), "!");
        assert_eq!(
            render::status_label(SessionState::NeedsFeedback),
            "Needs feedback"
        );
        assert_eq!(render::status_symbol(SessionState::Disconnected), "—");
        assert_eq!(
            render::status_label(SessionState::Disconnected),
            "Disconnected"
        );
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
    fn tui_session_form_keeps_advanced_codex_options_cli_only() {
        let source = include_str!("lib.rs");
        let create_session = source
            .split_once("        FormKind::CreateSession(worktree_id) => {")
            .and_then(|(_, tail)| tail.split_once("        FormKind::RenameProject"))
            .map(|(body, _)| body)
            .expect("TUI session submission source");

        assert!(create_session.contains("display_name: optional(0)"));
        assert!(create_session.contains("model: None"));
        assert!(create_session.contains("effort: None"));
        assert!(create_session.contains("initial_prompt: None"));
    }
}
