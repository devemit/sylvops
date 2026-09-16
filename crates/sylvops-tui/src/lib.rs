//! Replaceable four-panel terminal client backed entirely by daemon IPC.

use std::{io::stdout, time::Duration};

use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
};
use sylvops_core::{
    domain::{DaemonSnapshot, Session, SessionState, Worktree, WorktreeStatus, attention_priority},
    ids::SessionId,
    protocol::{ClientRequest, DaemonResponse},
};
use sylvops_daemon::{DaemonError, client::DaemonClient, runtime::RuntimePaths};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TuiExit {
    Quit,
    Attach(SessionId),
}

#[derive(Debug)]
struct App {
    snapshot: DaemonSnapshot,
    panel: usize,
    selections: [usize; 3],
    preview: String,
    help: bool,
}

impl App {
    fn new(snapshot: DaemonSnapshot) -> Self {
        Self {
            snapshot,
            panel: 0,
            selections: [0; 3],
            preview: "Select a worktree and press g to inspect its bounded Git diff.".into(),
            help: false,
        }
    }

    fn selected_project_id(&self) -> Option<sylvops_core::ids::ProjectId> {
        self.snapshot
            .projects
            .get(self.selections[0])
            .map(|project| project.id)
    }

    fn visible_worktrees(&self) -> Vec<&Worktree> {
        let selected_project = self.selected_project_id();
        self.snapshot
            .worktrees
            .iter()
            .filter(|worktree| {
                Some(worktree.project_id) == selected_project
                    && worktree.status != WorktreeStatus::Removed
            })
            .collect()
    }

    fn selected_worktree(&self) -> Option<&Worktree> {
        self.visible_worktrees().get(self.selections[1]).copied()
    }

    fn visible_sessions(&self) -> Vec<&Session> {
        let selected_worktree = self.selected_worktree().map(|worktree| worktree.id);
        self.snapshot
            .sessions
            .iter()
            .filter(|session| Some(session.worktree_id) == selected_worktree)
            .collect()
    }

    fn selected_session(&self) -> Option<&Session> {
        self.visible_sessions().get(self.selections[2]).copied()
    }

    fn move_selection(&mut self, delta: isize) {
        let length = match self.panel {
            0 => self.snapshot.projects.len(),
            1 => self.visible_worktrees().len(),
            2 => self.visible_sessions().len(),
            _ => 0,
        };
        if length == 0 || self.panel > 2 {
            return;
        }
        let current = self.selections[self.panel];
        let next = if delta < 0 {
            current.saturating_sub(delta.unsigned_abs())
        } else {
            current.saturating_add(delta.unsigned_abs()).min(length - 1)
        };
        self.selections[self.panel] = next;
        if next != current {
            if self.panel == 0 {
                self.selections[1] = 0;
                self.selections[2] = 0;
            } else if self.panel == 1 {
                self.selections[2] = 0;
            }
        }
    }

    fn select_attention(&mut self) {
        let Some((target_session_id, target_worktree_id)) = self
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
            .map(|session| (session.id, session.worktree_id))
        else {
            return;
        };
        let Some((target_project_id, target_worktree_id)) = self
            .snapshot
            .worktrees
            .iter()
            .find(|worktree| worktree.id == target_worktree_id)
            .map(|worktree| (worktree.project_id, worktree.id))
        else {
            return;
        };
        let Some(project_index) = self
            .snapshot
            .projects
            .iter()
            .position(|project| project.id == target_project_id)
        else {
            return;
        };
        self.selections[0] = project_index;
        self.selections[1] = self
            .visible_worktrees()
            .iter()
            .position(|candidate| candidate.id == target_worktree_id)
            .unwrap_or(0);
        self.selections[2] = self
            .visible_sessions()
            .iter()
            .position(|candidate| candidate.id == target_session_id)
            .unwrap_or(0);
        self.panel = 2;
    }
}

/// Opens mission control and returns when the user quits or asks to attach a session.
///
/// # Errors
///
/// Returns an error when daemon IPC or terminal setup, input, drawing, or cleanup fails.
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
        loop {
            match events.try_recv() {
                Ok(_) | Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {
                    refresh = true;
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                    return Err(DaemonError::Lifecycle("daemon event stream closed".into()));
                }
            }
        }
        if refresh {
            app.snapshot = snapshot(&client).await?;
            clamp_selections(&mut app);
        }
        terminal
            .draw(|frame| draw(frame, &app))
            .map_err(|error| DaemonError::Lifecycle(format!("cannot draw TUI: {error}")))?;
        if !event::poll(Duration::from_millis(75))
            .map_err(|error| DaemonError::Lifecycle(format!("cannot poll input: {error}")))?
        {
            continue;
        }
        let Event::Key(key) = event::read()
            .map_err(|error| DaemonError::Lifecycle(format!("cannot read input: {error}")))?
        else {
            continue;
        };
        if key.code == KeyCode::Char('?') {
            app.help = !app.help;
            continue;
        }
        if app.help {
            if matches!(key.code, KeyCode::Esc | KeyCode::Enter) {
                app.help = false;
            }
            continue;
        }
        match key.code {
            KeyCode::Char('q') => return Ok(TuiExit::Quit),
            KeyCode::Tab | KeyCode::Char('l') => app.panel = (app.panel + 1) % 4,
            KeyCode::BackTab | KeyCode::Char('h') => app.panel = (app.panel + 3) % 4,
            KeyCode::Down | KeyCode::Char('j') => app.move_selection(1),
            KeyCode::Up | KeyCode::Char('k') => app.move_selection(-1),
            KeyCode::Char('A') if key.modifiers.contains(KeyModifiers::SHIFT) => {
                app.select_attention();
            }
            KeyCode::Enter if app.panel == 2 => {
                if let Some(session) = app.selected_session() {
                    return Ok(TuiExit::Attach(session.id));
                }
            }
            KeyCode::Char('g') => {
                if let Some(worktree) = app.selected_worktree() {
                    app.preview = match client
                        .request(&ClientRequest::GetDiff {
                            worktree_id: worktree.id,
                        })
                        .await?
                    {
                        DaemonResponse::Diff(diff) if diff.text.is_empty() => {
                            "No tracked changes.".into()
                        }
                        DaemonResponse::Diff(diff) => diff.text,
                        DaemonResponse::Error(error) => {
                            format!("Diff unavailable: {}", error.message)
                        }
                        response => format!("Unexpected diff response: {response:?}"),
                    };
                    app.panel = 3;
                }
            }
            _ => {}
        }
    }
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

fn clamp_selections(app: &mut App) {
    let lengths = [
        app.snapshot.projects.len(),
        app.visible_worktrees().len(),
        app.visible_sessions().len(),
    ];
    for (selection, length) in app.selections.iter_mut().zip(lengths) {
        *selection = (*selection).min(length.saturating_sub(1));
    }
}

fn draw(frame: &mut ratatui::Frame<'_>, app: &App) {
    if app.help {
        frame.render_widget(
            Paragraph::new("Tab / Shift+Tab or h / l: panels\nj / k: selection\nEnter: attach selected session\ng: bounded Git diff\nShift+A: attention item\nCtrl+] while attached: detach\nq: close UI; sessions continue\n?: close help")
                .block(Block::default().title(" SylvOps shortcuts ").borders(Borders::ALL))
                .wrap(Wrap { trim: false }),
            centered(frame.area(), 70, 60),
        );
        return;
    }

    let area = frame.area();
    let panels = Layout::default()
        .direction(if area.width < 90 {
            Direction::Vertical
        } else {
            Direction::Horizontal
        })
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);
    let navigation = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(34),
            Constraint::Percentage(33),
            Constraint::Percentage(33),
        ])
        .split(panels[0]);

    let projects = app
        .snapshot
        .projects
        .iter()
        .map(|project| ListItem::new(project.name.clone()))
        .collect();
    render_list(
        frame,
        navigation[0],
        "Projects",
        projects,
        app.panel == 0,
        app.selections[0],
    );

    let worktrees = app
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
        navigation[1],
        "Worktrees",
        worktrees,
        app.panel == 1,
        app.selections[1],
    );

    let sessions = app
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
        navigation[2],
        "Sessions",
        sessions,
        app.panel == 2,
        app.selections[2],
    );

    frame.render_widget(
        Paragraph::new(app.preview.as_str())
            .block(
                Block::default()
                    .title(if app.panel == 3 {
                        "Preview [active]"
                    } else {
                        "Preview"
                    })
                    .borders(Borders::ALL),
            )
            .wrap(Wrap { trim: false }),
        panels[1],
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
}
