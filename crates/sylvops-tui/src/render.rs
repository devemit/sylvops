use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::Modifier,
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Tabs, Wrap},
};
use sylvops_core::{
    domain::{Session, SessionState},
    provider::session_can_resume,
    ui::{TuiContentView, TuiDensity, TuiSessionPresentation, TuiTheme},
};

use crate::{
    app::{App, ExplorerNode, FocusZone, HitMap, HitTarget, Mode, SettingsFocus},
    forms::Form,
    theme::Theme,
};

#[derive(Clone, Copy, Debug)]
pub(crate) struct ViewAreas {
    pub explorer: Option<Rect>,
    pub main: Option<Rect>,
}

const NEW_LABEL: &str = " [n] New ";
const RENAME_LABEL: &str = " [r] Rename ";
const SETTINGS_LABEL: &str = " [s] Settings ";
const COMMANDS_LABEL: &str = " [/] Commands  ";

pub(crate) fn draw(frame: &mut Frame<'_>, app: &mut App) {
    app.hit_map = HitMap::default();
    let theme = Theme::resolve(app.theme);
    let area = frame.area();
    frame.render_widget(Block::default().style(theme.background), area);
    let attention_height = u16::from(app.attention_count() > 0);
    let (header_height, footer_height) = match app.density {
        TuiDensity::Comfortable => (2, 2),
        TuiDensity::Compact => (1, 1),
    };
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_height),
            Constraint::Length(attention_height),
            Constraint::Min(3),
            Constraint::Length(footer_height),
        ])
        .split(area);
    draw_header(frame, app, vertical[0], theme);
    if attention_height > 0 {
        draw_attention(frame, app, vertical[1], theme);
    }
    let view = view_areas(vertical[2], app.focus);
    if let Some(explorer) = view.explorer {
        draw_explorer(frame, app, explorer, theme);
    }
    if let Some(main) = view.main {
        draw_main(frame, app, main, theme);
    }
    draw_footer(frame, app, vertical[3], theme);
    if app.help {
        draw_help(frame, area, theme);
    }
    match app.mode.clone() {
        Mode::Modal(form) => draw_form(frame, &form, &mut app.hit_map, theme, app.density),
        Mode::Confirmation(confirm) => {
            draw_confirmation(frame, &confirm, &mut app.hit_map, theme, app.density);
        }
        Mode::CommandPalette(palette) => draw_palette(frame, app, &palette, theme),
        Mode::Settings(settings) => draw_settings(frame, app, settings, theme),
        Mode::Navigation | Mode::TerminalAttached => {}
    }
}

pub(crate) fn view_areas(area: Rect, focus: FocusZone) -> ViewAreas {
    if area.width < 72 {
        return match focus {
            FocusZone::Explorer => ViewAreas {
                explorer: Some(area),
                main: None,
            },
            FocusZone::Main => ViewAreas {
                explorer: None,
                main: Some(area),
            },
        };
    }
    let explorer_width = if area.width >= 96 { 34 } else { 26 };
    let split = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(explorer_width.min(area.width / 2)),
            Constraint::Min(30),
        ])
        .split(area);
    ViewAreas {
        explorer: Some(split[0]),
        main: Some(split[1]),
    }
}

fn draw_header(frame: &mut Frame<'_>, app: &mut App, area: Rect, theme: Theme) {
    let workspace = app
        .active_workspace()
        .map_or("No workspace", |item| item.name.as_str());
    let repository = app
        .selected_project()
        .map_or("No repository", |item| item.name.as_str());
    let branch = app
        .selected_worktree()
        .and_then(|item| item.branch.as_deref())
        .unwrap_or("detached");
    let status = format!(
        "● Connected  {} active Sessions  {} attention",
        app.active_sessions().len(),
        app.attention_count()
    );
    let removal_action = match app.selected_node() {
        Some(ExplorerNode::Workspace(_)) => "Delete Workspace",
        Some(ExplorerNode::Project(_)) => "Remove Project",
        Some(ExplorerNode::Worktree(_)) => "Remove Worktree",
        Some(ExplorerNode::Session(_)) => {
            app.selected_session().map_or("Delete Session", |session| {
                if matches!(
                    session.state,
                    SessionState::Fresh
                        | SessionState::Starting
                        | SessionState::Running
                        | SessionState::NeedsFeedback
                ) {
                    "Stop Session"
                } else {
                    "Delete Session"
                }
            })
        }
        None => "Stop/Remove",
    };
    let delete_label = format!(" [d] {removal_action} ");
    if app.density == TuiDensity::Compact {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" SylvOps ", theme.focus),
                Span::raw(format!("{workspace} / {repository} / {branch}")),
            ]))
            .style(theme.text),
            area,
        );
        frame.render_widget(
            Paragraph::new(SETTINGS_LABEL)
                .alignment(Alignment::Right)
                .style(theme.focus),
            area,
        );
        let settings_width = u16::try_from(SETTINGS_LABEL.len())
            .unwrap_or(u16::MAX)
            .min(area.width);
        app.hit_map.push(
            Rect::new(
                area.x
                    .saturating_add(area.width.saturating_sub(settings_width)),
                area.y,
                settings_width,
                1,
            ),
            HitTarget::Settings,
        );
    } else {
        let rows = vec![
            Line::from(vec![
                Span::styled(" SylvOps ", theme.focus),
                Span::raw(format!("{workspace}  /  {repository}  /  {branch}")),
            ]),
            Line::from(vec![
                Span::styled(delete_label.as_str(), theme.danger),
                Span::styled(NEW_LABEL, theme.success),
                Span::raw(RENAME_LABEL),
                Span::styled(SETTINGS_LABEL, theme.focus),
                Span::raw(COMMANDS_LABEL),
                Span::styled(status, theme.muted),
            ]),
        ];
        frame.render_widget(Paragraph::new(rows).style(theme.text), area);
        let mut x = area.x;
        for (label, target) in [
            (delete_label.as_str(), HitTarget::Delete),
            (NEW_LABEL, HitTarget::New),
            (RENAME_LABEL, HitTarget::Rename),
            (SETTINGS_LABEL, HitTarget::Settings),
        ] {
            let width = u16::try_from(label.len())
                .unwrap_or(u16::MAX)
                .min(area.x.saturating_add(area.width).saturating_sub(x));
            app.hit_map
                .push(Rect::new(x, area.y.saturating_add(1), width, 1), target);
            x = x.saturating_add(width);
        }
    }
}

fn draw_attention(frame: &mut Frame<'_>, app: &mut App, area: Rect, theme: Theme) {
    frame.render_widget(
        Paragraph::new(format!(
            " ! {} session(s) need attention · Shift+A or click to open next",
            app.attention_count()
        ))
        .style(theme.attention),
        area,
    );
    app.hit_map.push(area, HitTarget::Attention);
}

fn draw_explorer(frame: &mut Frame<'_>, app: &mut App, area: Rect, theme: Theme) {
    app.hit_map.push(area, HitTarget::FocusExplorer);
    let rows = app.explorer_rows();
    let items = if rows.is_empty() {
        vec![ListItem::new("No repositories yet. Click New or press n.")]
    } else {
        rows.iter()
            .map(|row| {
                let (label, suffix) = explorer_label(app, row.node);
                let disclosure = if row.expandable {
                    if row.expanded { "▼" } else { "▶" }
                } else {
                    " "
                };
                let line = Line::from(vec![
                    Span::raw(format!(
                        "{}{} {label}",
                        "  ".repeat(usize::from(row.depth)),
                        disclosure
                    )),
                    Span::styled(suffix, theme.muted),
                ]);
                if app.density == TuiDensity::Comfortable {
                    ListItem::new(vec![line, Line::from("")])
                } else {
                    ListItem::new(line)
                }
            })
            .collect()
    };
    let focused = app.focus == FocusZone::Explorer;
    let block = Block::default()
        .title(if focused {
            " Explorer [active] "
        } else {
            " Explorer "
        })
        .borders(Borders::ALL)
        .border_style(if focused { theme.focus } else { theme.border });
    let mut state =
        ListState::default().with_selected((!rows.is_empty()).then_some(app.explorer_index));
    frame.render_stateful_widget(
        List::new(items)
            .block(block)
            .style(theme.text)
            .highlight_symbol("› ")
            .highlight_style(theme.selected),
        area,
        &mut state,
    );
    let inner = area.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 1,
    });
    let row_height = if app.density == TuiDensity::Comfortable {
        2
    } else {
        1
    };
    for (index, _) in rows
        .iter()
        .take(usize::from(inner.height / row_height))
        .enumerate()
    {
        app.hit_map.push(
            Rect::new(
                inner.x,
                inner.y.saturating_add(
                    u16::try_from(index)
                        .unwrap_or(u16::MAX)
                        .saturating_mul(row_height),
                ),
                inner.width,
                row_height,
            ),
            HitTarget::ExplorerRow(index),
        );
    }
}

fn explorer_label(app: &App, node: ExplorerNode) -> (String, String) {
    match node {
        ExplorerNode::Workspace(id) => app
            .snapshot
            .workspaces
            .iter()
            .find(|item| item.id == id)
            .map_or(("Missing workspace".into(), String::new()), |item| {
                (item.name.clone(), "  workspace".into())
            }),
        ExplorerNode::Project(id) => app
            .snapshot
            .projects
            .iter()
            .find(|item| item.id == id)
            .map_or(("Missing project".into(), String::new()), |item| {
                (item.name.clone(), String::new())
            }),
        ExplorerNode::Worktree(id) => app
            .snapshot
            .worktrees
            .iter()
            .find(|item| item.id == id)
            .map_or(("Missing worktree".into(), String::new()), |item| {
                (
                    item.name.clone(),
                    item.branch
                        .as_ref()
                        .map_or(String::new(), |branch| format!("  {branch}")),
                )
            }),
        ExplorerNode::Session(id) => app
            .snapshot
            .sessions
            .iter()
            .find(|item| item.id == id)
            .map_or(("Missing session".into(), String::new()), |item| {
                (
                    format!("{} {}", status_symbol(item.state), item.display_name),
                    format!("  {}", status_label(item.state)),
                )
            }),
    }
}

fn draw_main(frame: &mut Frame<'_>, app: &mut App, area: Rect, theme: Theme) {
    app.hit_map.push(area, HitTarget::FocusMain);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(2)])
        .split(area);
    let titles: Vec<_> = TuiContentView::ALL
        .into_iter()
        .map(|view| Line::from(format!("{} {}", view.shortcut(), view.label())))
        .collect();
    let selected = TuiContentView::ALL
        .iter()
        .position(|view| *view == app.main_tab)
        .unwrap_or(0);
    let selected_session = app.selected_session();
    let title = selected_session.map_or(" Session workspace".into(), |session| {
        format!(
            " {} · {} · {}",
            session.display_name,
            session.provider_kind.display_name(),
            status_label(session.state)
        )
    });
    frame.render_widget(
        Tabs::new(titles)
            .select(selected)
            .block(
                Block::default()
                    .title(title)
                    .borders(Borders::ALL)
                    .border_style(theme.border),
            )
            .style(theme.text)
            .highlight_style(theme.focus),
        chunks[0],
    );
    let tab_count = u16::try_from(TuiContentView::ALL.len()).unwrap_or(1);
    let tab_width = chunks[0].width.saturating_sub(2) / tab_count;
    for (index, tab) in TuiContentView::ALL.into_iter().enumerate() {
        app.hit_map.push(
            Rect::new(
                chunks[0].x.saturating_add(
                    1 + tab_width.saturating_mul(u16::try_from(index).unwrap_or(0)),
                ),
                chunks[0].y.saturating_add(1),
                tab_width,
                1,
            ),
            HitTarget::Tab(tab),
        );
    }
    match app.main_tab {
        TuiContentView::Terminal => draw_terminal(frame, app, chunks[1], theme),
        TuiContentView::Changes => draw_changes(frame, app, chunks[1], theme),
        TuiContentView::Details => draw_details(frame, app, chunks[1], theme),
        TuiContentView::Sessions => draw_sessions(frame, app, chunks[1], theme),
    }
}

struct ActiveSessionItem {
    id: sylvops_core::ids::SessionId,
    name: String,
    state: SessionState,
    provider: &'static str,
    project: String,
    worktree: String,
    last_activity_at: i64,
}

fn draw_sessions(frame: &mut Frame<'_>, app: &mut App, area: Rect, theme: Theme) {
    let sessions = app
        .active_sessions()
        .into_iter()
        .map(|session| {
            let worktree = app
                .snapshot
                .worktrees
                .iter()
                .find(|worktree| worktree.id == session.worktree_id);
            let project = worktree.and_then(|worktree| {
                app.snapshot
                    .projects
                    .iter()
                    .find(|project| project.id == worktree.project_id)
            });
            ActiveSessionItem {
                id: session.id,
                name: session.display_name.clone(),
                state: session.state,
                provider: session.provider_kind.display_name(),
                project: project
                    .map_or("Unknown Project", |project| project.name.as_str())
                    .to_owned(),
                worktree: worktree
                    .map_or("Unknown Worktree", |worktree| worktree.name.as_str())
                    .to_owned(),
                last_activity_at: session.last_activity_at,
            }
        })
        .collect::<Vec<_>>();
    let title = format!(
        " Active Sessions · {} · {} ",
        sessions.len(),
        app.session_presentation
    );
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(theme.border);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if sessions.is_empty() {
        draw_sessions_empty(frame, app, inner, theme);
        return;
    }

    let columns = session_columns(inner.width, app.session_presentation);
    let gap = u16::from(columns == 2);
    let card_width = inner
        .width
        .saturating_sub(gap)
        .checked_div(columns)
        .unwrap_or(inner.width);
    let card_height = 7;
    let selected_index = app
        .selected_session_id
        .and_then(|selected| sessions.iter().position(|session| session.id == selected))
        .unwrap_or(0);
    let selected_row = selected_index / usize::from(columns);
    let visible_rows = usize::from(inner.height / card_height).max(1);
    let first_row = selected_row.saturating_sub(visible_rows.saturating_sub(1));
    let first_index = first_row.saturating_mul(usize::from(columns));
    let visible_count = visible_rows.saturating_mul(usize::from(columns));

    for (visible_index, session) in sessions
        .iter()
        .enumerate()
        .skip(first_index)
        .take(visible_count)
    {
        let visible_row = (visible_index - first_index) / usize::from(columns);
        let column = (visible_index - first_index) % usize::from(columns);
        let x = inner.x.saturating_add(
            u16::try_from(column)
                .unwrap_or(0)
                .saturating_mul(card_width.saturating_add(gap)),
        );
        let y = inner.y.saturating_add(
            u16::try_from(visible_row)
                .unwrap_or(u16::MAX)
                .saturating_mul(card_height),
        );
        let card = Rect::new(
            x,
            y,
            card_width.min(inner.x.saturating_add(inner.width).saturating_sub(x)),
            card_height.min(inner.y.saturating_add(inner.height).saturating_sub(y)),
        );
        draw_active_session(
            frame,
            app,
            card,
            session,
            app.selected_session_id == Some(session.id),
            theme,
        );
    }
}

const fn session_columns(width: u16, presentation: TuiSessionPresentation) -> u16 {
    if matches!(presentation, TuiSessionPresentation::Cards) && width.saturating_sub(1) / 2 >= 36 {
        2
    } else {
        1
    }
}

fn draw_active_session(
    frame: &mut Frame<'_>,
    app: &mut App,
    area: Rect,
    session: &ActiveSessionItem,
    selected: bool,
    theme: Theme,
) {
    let border = if selected { theme.focus } else { theme.border };
    let lines = vec![
        Line::from(vec![
            Span::styled(
                format!("{} {}", status_symbol(session.state), session.name),
                Modifier::BOLD,
            ),
            Span::raw(format!(" · {}", status_label(session.state))),
        ]),
        Line::from(format!(
            "Provider: {} · Project: {}",
            session.provider, session.project
        )),
        Line::from(format!("Worktree: {}", session.worktree)),
        Line::from(format!(
            "Activity: {}",
            relative_activity(app.snapshot.captured_at, session.last_activity_at)
        )),
        Line::from("[ Open terminal · Enter ]  [ Stop · d ]"),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).border_style(border))
            .style(if selected { theme.selected } else { theme.text })
            .wrap(Wrap { trim: true }),
        area,
    );
    app.hit_map.push(area, HitTarget::ActiveSession(session.id));
    if area.height >= 2 {
        let actions_y = area.y.saturating_add(area.height.saturating_sub(2));
        let open_width = 25.min(area.width.saturating_sub(2));
        app.hit_map.push(
            Rect::new(area.x.saturating_add(1), actions_y, open_width, 1),
            HitTarget::ActiveSessionOpen(session.id),
        );
        let stop_x = area
            .x
            .saturating_add(28)
            .min(area.x.saturating_add(area.width));
        app.hit_map.push(
            Rect::new(
                stop_x,
                actions_y,
                12.min(area.x.saturating_add(area.width).saturating_sub(stop_x)),
                1,
            ),
            HitTarget::ActiveSessionStop(session.id),
        );
    }
}

fn draw_sessions_empty(frame: &mut Frame<'_>, app: &mut App, area: Rect, theme: Theme) {
    let project = app
        .selected_project()
        .map_or("No Project selected", |project| project.name.as_str());
    let worktree = app
        .selected_worktree()
        .map_or("No Worktree selected", |worktree| worktree.name.as_str());
    let can_create = app
        .selected_worktree()
        .is_some_and(|worktree| worktree.status == sylvops_core::domain::WorktreeStatus::Active);
    let guidance = if can_create {
        "[ New Session · n ]"
    } else {
        "Select an active Worktree in the Explorer to start a Session."
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from("No Active Sessions in this Workspace."),
            Line::from(format!("Project: {project}")),
            Line::from(format!("Worktree: {worktree}")),
            Line::from(""),
            Line::from(guidance),
        ])
        .style(theme.text)
        .wrap(Wrap { trim: true }),
        area,
    );
    if can_create {
        app.hit_map.push(
            Rect::new(area.x, area.y.saturating_add(4), 19.min(area.width), 1),
            HitTarget::New,
        );
    }
}

fn relative_activity(captured_at: i64, activity_at: i64) -> String {
    let elapsed = captured_at.saturating_sub(activity_at).max(0);
    match elapsed {
        0..60_000 => "just now".into(),
        60_000..3_600_000 => format!("{}m ago", elapsed / 60_000),
        3_600_000..86_400_000 => format!("{}h ago", elapsed / 3_600_000),
        _ => format!("{}d ago", elapsed / 86_400_000),
    }
}

fn draw_terminal(frame: &mut Frame<'_>, app: &mut App, area: Rect, theme: Theme) {
    let provider_owned = app.attached.is_some();
    let (title, content, action) = if let Some(attached) = app.attached.as_ref() {
        let role = if attached.role == sylvops_core::domain::AttachmentRole::Controller {
            "controller"
        } else {
            "read-only observer"
        };
        (
            format!(" Terminal · attached as {role} "),
            terminal_text(attached.parser.screen()),
            Some(("[ Detach · Ctrl+] ]", HitTarget::Detach)),
        )
    } else if let Some(session) = app.selected_session() {
        let can_resume = session_can_resume(session, &app.snapshot.sessions, &app.providers);
        (
            " Terminal · detached ".into(),
            Text::from(format!(
                "{} is {}.\n\nSelecting a session never attaches automatically.\n{}",
                session.display_name,
                status_label(session.state),
                if can_resume {
                    "Press Enter to inspect retained output, or resume to continue in a new session."
                } else {
                    "Press Enter or click Attach to connect."
                }
            )),
            Some(if can_resume {
                ("[ Resume · u ]", HitTarget::Resume)
            } else {
                ("[ Attach ]", HitTarget::Attach)
            }),
        )
    } else {
        (
            " Terminal ".into(),
            Text::from("No session selected.\n\nPress n or click New to create a shell session."),
            Some(("[ New session · n ]", HitTarget::New)),
        )
    };
    let inner = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);
    let mut terminal = Paragraph::new(content)
        .block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(theme.border),
        )
        .wrap(Wrap { trim: false });
    if !provider_owned {
        terminal = terminal.style(theme.text);
    }
    frame.render_widget(terminal, inner[0]);
    if let Some((label, target)) = action {
        frame.render_widget(
            Paragraph::new(label)
                .alignment(Alignment::Right)
                .style(theme.focus),
            inner[1],
        );
        app.hit_map.push(inner[1], target);
    }
}

fn terminal_text(screen: &vt100::Screen) -> Text<'static> {
    let (rows, columns) = screen.size();
    let mut lines = Vec::with_capacity(usize::from(rows));
    for row in 0..rows {
        let mut spans = Vec::new();
        let mut current_style = None;
        let mut current = String::new();
        for column in 0..columns {
            let Some(cell) = screen.cell(row, column) else {
                continue;
            };
            if cell.is_wide_continuation() {
                continue;
            }
            let style = terminal_cell_style(cell);
            if current_style.is_some_and(|existing| existing != style) {
                spans.push(Span::styled(
                    std::mem::take(&mut current),
                    current_style.unwrap(),
                ));
            }
            current_style = Some(style);
            if cell.has_contents() {
                current.push_str(cell.contents());
            } else {
                current.push(' ');
            }
        }
        if let Some(style) = current_style {
            spans.push(Span::styled(current, style));
        }
        lines.push(Line::from(spans));
    }
    Text::from(lines)
}

fn terminal_cell_style(cell: &vt100::Cell) -> ratatui::style::Style {
    let mut style = ratatui::style::Style::default()
        .fg(terminal_color(cell.fgcolor()))
        .bg(terminal_color(cell.bgcolor()));
    let mut modifiers = Modifier::empty();
    if cell.bold() {
        modifiers.insert(Modifier::BOLD);
    }
    if cell.dim() {
        modifiers.insert(Modifier::DIM);
    }
    if cell.italic() {
        modifiers.insert(Modifier::ITALIC);
    }
    if cell.underline() {
        modifiers.insert(Modifier::UNDERLINED);
    }
    if cell.inverse() {
        modifiers.insert(Modifier::REVERSED);
    }
    style = style.add_modifier(modifiers);
    style
}

const fn terminal_color(color: vt100::Color) -> ratatui::style::Color {
    match color {
        vt100::Color::Default => ratatui::style::Color::Reset,
        vt100::Color::Idx(index) => ratatui::style::Color::Indexed(index),
        vt100::Color::Rgb(red, green, blue) => ratatui::style::Color::Rgb(red, green, blue),
    }
}

fn draw_changes(frame: &mut Frame<'_>, app: &App, area: Rect, theme: Theme) {
    let content = app
        .diff
        .as_deref()
        .unwrap_or("Press g to load the bounded daemon-owned diff for this worktree.");
    frame.render_widget(
        Paragraph::new(content)
            .block(
                Block::default()
                    .title(" Changes · read-only ")
                    .borders(Borders::ALL)
                    .border_style(theme.border),
            )
            .style(theme.text)
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_details(frame: &mut Frame<'_>, app: &App, area: Rect, theme: Theme) {
    let content = if let Some(session) = app.selected_session() {
        session_details(app, session, theme)
    } else if let Some(worktree) = app.selected_worktree() {
        vec![
            Line::from(format!("Worktree: {}", worktree.name)),
            Line::from(format!("Path: {}", worktree.canonical_path)),
            Line::from(format!(
                "Branch: {}",
                worktree.branch.as_deref().unwrap_or("detached")
            )),
        ]
    } else if let Some(project) = app.selected_project() {
        vec![
            Line::from(format!("Repository: {}", project.name)),
            Line::from(format!("Path: {}", project.canonical_repository_path)),
        ]
    } else if app.selected_workspace {
        app.active_workspace().map_or_else(
            || vec![Line::from("No Workspace is open.")],
            |workspace| vec![Line::from(format!("Workspace: {}", workspace.name))],
        )
    } else {
        vec![Line::from(
            "Select an item in the explorer to view its details.",
        )]
    };
    frame.render_widget(
        Paragraph::new(content)
            .block(
                Block::default()
                    .title(" Details ")
                    .borders(Borders::ALL)
                    .border_style(theme.border),
            )
            .style(theme.text)
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn session_details<'a>(app: &'a App, session: &'a Session, theme: Theme) -> Vec<Line<'a>> {
    vec![
        Line::from(vec![
            Span::styled("Session      ", theme.muted),
            Span::raw(&session.display_name),
        ]),
        Line::from(vec![
            Span::styled("Provider     ", theme.muted),
            Span::raw(session.provider_kind.display_name()),
        ]),
        Line::from(vec![
            Span::styled("State        ", theme.muted),
            Span::raw(format!(
                "{} {}",
                status_symbol(session.state),
                status_label(session.state)
            )),
        ]),
        Line::from(vec![
            Span::styled("Worktree     ", theme.muted),
            Span::raw(
                app.selected_worktree()
                    .map_or("Unknown", |item| item.name.as_str()),
            ),
        ]),
        owned_detail_line(
            "Started      ",
            optional_number(session.started_at, "Not started"),
            theme,
        ),
        owned_detail_line(
            "Ended        ",
            optional_number(session.ended_at, "Not ended"),
            theme,
        ),
        owned_detail_line(
            "Exit code    ",
            optional_number(session.exit_code, "Not recorded"),
            theme,
        ),
        owned_detail_line(
            "Reason       ",
            session
                .failure_reason
                .clone()
                .unwrap_or_else(|| "None".into()),
            theme,
        ),
        owned_detail_line(
            "Output       ",
            "Memory-only; lost when the daemon restarts".into(),
            theme,
        ),
        Line::from(vec![
            Span::styled("Attachment   ", theme.muted),
            Span::raw(app.attached.as_ref().map_or("Detached", |item| {
                if item.role == sylvops_core::domain::AttachmentRole::Controller {
                    "Controller"
                } else {
                    "Read-only observer"
                }
            })),
        ]),
        Line::from(vec![
            Span::styled("Resume       ", theme.muted),
            Span::raw(
                if session_can_resume(session, &app.snapshot.sessions, &app.providers) {
                    "Available · press u"
                } else {
                    "Unavailable"
                },
            ),
        ]),
        Line::from(""),
        Line::from(
            "Safe actions: Attach, Resume, Rename, Stop. Commit, push, merge, and PR actions are not available.",
        ),
    ]
}

fn owned_detail_line(label: &'static str, value: String, theme: Theme) -> Line<'static> {
    Line::from(vec![Span::styled(label, theme.muted), Span::raw(value)])
}

fn optional_number<T: ToString>(value: Option<T>, fallback: &str) -> String {
    value.map_or_else(|| fallback.to_owned(), |value| value.to_string())
}

fn draw_footer(frame: &mut Frame<'_>, app: &App, area: Rect, theme: Theme) {
    let message = app.flash.as_ref().map_or_else(
        || "Tab switch area · arrows navigate · Enter primary action · ? help · q close UI".into(),
        |flash| flash.text.clone(),
    );
    let style = app
        .flash
        .as_ref()
        .map_or(theme.muted, |flash| theme.flash(flash.kind));
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(message),
            Line::from("Sessions keep running after the UI closes."),
        ])
        .style(style),
        area,
    );
}

fn draw_help(frame: &mut Frame<'_>, area: Rect, theme: Theme) {
    let popup = centered(area, 78, 70);
    frame.render_widget(Clear, popup);
    frame.render_widget(Paragraph::new("Tab / Shift+Tab: Explorer ↔ Main\n↑ ↓ or j k: navigate hierarchy or Active Sessions\n← →: collapse / expand or move Active Session selection\n1 / 2 / 3 / 4: Terminal / Changes / Details / Sessions\nEnter: explicit primary action\nu: resume eligible session\nn: create in context\nr: rename metadata\nd: stop session or remove clean worktree\ng: bounded read-only Git diff\ns: TUI settings\n/: commands and entity search\nShift+A: next attention item\nCtrl+] while attached: detach\nq: close UI; sessions continue\nMouse: select rows, tabs, actions, controls, and scroll")
        .block(Block::default().title(" Shortcuts ").borders(Borders::ALL).border_style(theme.border)).style(theme.text).wrap(Wrap { trim: false }), popup);
}

#[allow(clippy::too_many_lines)]
fn draw_form(
    frame: &mut Frame<'_>,
    form: &Form,
    hits: &mut HitMap,
    theme: Theme,
    density: TuiDensity,
) {
    let area = centered(
        frame.area(),
        76,
        if density == TuiDensity::Comfortable {
            76
        } else {
            62
        },
    );
    frame.render_widget(Clear, area);
    let mut lines = Vec::new();
    let mut row = area.y.saturating_add(1);
    if form.is_session() {
        lines.push(Line::from(Span::styled("Provider", Modifier::BOLD)));
        row = row.saturating_add(1);
        let providers = if form.providers.is_empty() {
            "No providers available".into()
        } else {
            form.providers
                .iter()
                .enumerate()
                .map(|(index, provider)| {
                    format!(
                        "{} {}{}",
                        if index == form.provider_index {
                            "●"
                        } else {
                            "○"
                        },
                        provider.kind.display_name(),
                        if !provider.available {
                            " (unavailable)"
                        } else if !provider.can_start_interactive_session() {
                            " (setup required)"
                        } else {
                            ""
                        }
                    )
                })
                .collect::<Vec<_>>()
                .join("   ")
        };
        lines.push(Line::from(providers));
        hits.push(
            Rect::new(
                area.x.saturating_add(1),
                row,
                area.width.saturating_sub(2),
                1,
            ),
            HitTarget::ModalProvider(0),
        );
        row = row.saturating_add(1);
        if let Some(error) = form
            .provider()
            .and_then(sylvops_core::provider::ProviderHealth::session_start_error)
        {
            lines.push(Line::from(Span::styled(error, theme.danger)));
            row = row.saturating_add(1);
            lines.push(Line::from(Span::styled(
                "[ Check again · Ctrl+R ]",
                theme.focus,
            )));
            hits.push(
                Rect::new(
                    area.x.saturating_add(1),
                    row,
                    area.width.saturating_sub(2),
                    1,
                ),
                HitTarget::ModalProviderProbe,
            );
            row = row.saturating_add(1);
        }
    }
    for (visible_position, index) in form.visible_field_indices().into_iter().enumerate() {
        let field = &form.fields[index];
        lines.push(Line::from(Span::styled(
            format!(
                "{} {}",
                if visible_position == form.active {
                    "›"
                } else {
                    " "
                },
                field.label
            ),
            Modifier::BOLD,
        )));
        row = row.saturating_add(1);
        lines.push(Line::from(format!("  {}", field.value)));
        hits.push(
            Rect::new(
                area.x.saturating_add(1),
                row,
                area.width.saturating_sub(2),
                1,
            ),
            HitTarget::ModalField(visible_position),
        );
        row = row.saturating_add(1);
        if let Some(error) = &field.error {
            lines.push(Line::from(Span::styled(format!("  {error}"), theme.danger)));
            row = row.saturating_add(1);
        }
        if density == TuiDensity::Comfortable {
            lines.push(Line::from(""));
            row = row.saturating_add(1);
        }
    }
    if let Some(error) = &form.submission_error {
        lines.push(Line::from(Span::styled(error, theme.danger)));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("[ Create / Save ]", theme.success),
        Span::raw("   "),
        Span::styled("[ Cancel ]", theme.muted),
    ]));
    let button_row = area.y.saturating_add(area.height.saturating_sub(2));
    hits.push(
        Rect::new(area.x.saturating_add(2), button_row, 18, 1),
        HitTarget::ModalSubmit,
    );
    hits.push(
        Rect::new(area.x.saturating_add(22), button_row, 12, 1),
        HitTarget::ModalCancel,
    );
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title(format!(" {} · Enter next/submit · Esc cancel ", form.title))
                    .borders(Borders::ALL)
                    .border_style(theme.border),
            )
            .style(theme.text)
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_confirmation(
    frame: &mut Frame<'_>,
    confirm: &crate::app::Confirmation,
    hits: &mut HitMap,
    theme: Theme,
    density: TuiDensity,
) {
    let area = centered(
        frame.area(),
        72,
        if density == TuiDensity::Comfortable {
            40
        } else {
            30
        },
    );
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(confirm.detail.as_str()),
            Line::from(""),
            Line::from(vec![
                Span::styled("[ Confirm · y ]", theme.danger),
                Span::raw("   [ Cancel · Esc ]"),
            ]),
        ])
        .block(
            Block::default()
                .title(format!(" {} ", confirm.title))
                .borders(Borders::ALL)
                .border_style(theme.border),
        )
        .style(theme.text)
        .wrap(Wrap { trim: false }),
        area,
    );
    let row = area.y.saturating_add(area.height.saturating_sub(2));
    hits.push(
        Rect::new(area.x.saturating_add(2), row, 18, 1),
        HitTarget::Confirm,
    );
    hits.push(
        Rect::new(area.x.saturating_add(22), row, 18, 1),
        HitTarget::Cancel,
    );
}

#[allow(clippy::too_many_lines)]
fn draw_settings(
    frame: &mut Frame<'_>,
    app: &mut App,
    settings: crate::app::Settings,
    theme: Theme,
) {
    let area = centered(
        frame.area(),
        82,
        if app.density == TuiDensity::Comfortable {
            82
        } else {
            68
        },
    );
    frame.render_widget(Clear, area);
    let inner = area.inner(ratatui::layout::Margin {
        horizontal: 2,
        vertical: 1,
    });
    let mut row = inner.y;
    let mut lines = Vec::new();
    lines.push(Line::from(Span::styled(
        if settings.focus == SettingsFocus::Theme {
            "› Theme"
        } else {
            "  Theme"
        },
        Modifier::BOLD,
    )));
    row = row.saturating_add(1);
    for choice in TuiTheme::ALL {
        let selected = app.theme == choice;
        lines.push(Line::from(Span::styled(
            format!("  {} {choice}", if selected { "●" } else { "○" }),
            if selected { theme.selected } else { theme.text },
        )));
        app.hit_map.push(
            Rect::new(inner.x, row, inner.width, 1),
            HitTarget::SettingsTheme(choice),
        );
        row = row.saturating_add(1);
    }
    if app.density == TuiDensity::Comfortable {
        lines.push(Line::from(""));
        row = row.saturating_add(1);
    }
    lines.push(Line::from(Span::styled(
        if settings.focus == SettingsFocus::Presentation {
            "› Sessions presentation"
        } else {
            "  Sessions presentation"
        },
        Modifier::BOLD,
    )));
    row = row.saturating_add(1);
    for choice in TuiSessionPresentation::ALL {
        let selected = app.session_presentation == choice;
        lines.push(Line::from(Span::styled(
            format!("  {} {choice}", if selected { "●" } else { "○" }),
            if selected { theme.selected } else { theme.text },
        )));
        app.hit_map.push(
            Rect::new(inner.x, row, inner.width, 1),
            HitTarget::SettingsPresentation(choice),
        );
        row = row.saturating_add(1);
    }
    if app.density == TuiDensity::Comfortable {
        lines.push(Line::from(""));
        row = row.saturating_add(1);
    }
    lines.push(Line::from(Span::styled(
        if settings.focus == SettingsFocus::Density {
            "› Density"
        } else {
            "  Density"
        },
        Modifier::BOLD,
    )));
    row = row.saturating_add(1);
    for choice in TuiDensity::ALL {
        let selected = app.density == choice;
        lines.push(Line::from(Span::styled(
            format!("  {} {choice}", if selected { "●" } else { "○" }),
            if selected { theme.selected } else { theme.text },
        )));
        app.hit_map.push(
            Rect::new(inner.x, row, inner.width, 1),
            HitTarget::SettingsDensity(choice),
        );
        row = row.saturating_add(1);
    }
    lines.push(Line::from(""));
    row = row.saturating_add(1);
    lines.push(Line::from(Span::styled(
        "Preview is local until Apply. Escape and Cancel restore all choices.",
        theme.muted,
    )));
    row = row.saturating_add(2);
    lines.push(Line::from(vec![
        Span::styled(
            if settings.focus == SettingsFocus::Apply {
                "› [ Apply ]"
            } else {
                "  [ Apply ]"
            },
            theme.success,
        ),
        Span::raw("    "),
        Span::styled(
            if settings.focus == SettingsFocus::Cancel {
                "› [ Cancel ]"
            } else {
                "  [ Cancel ]"
            },
            theme.muted,
        ),
    ]));
    app.hit_map
        .push(Rect::new(inner.x, row, 13, 1), HitTarget::SettingsApply);
    app.hit_map.push(
        Rect::new(inner.x.saturating_add(17), row, 14, 1),
        HitTarget::SettingsCancel,
    );
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title(" Settings · ←/→ preview · ↑/↓ navigate ")
                    .borders(Borders::ALL)
                    .border_style(theme.border),
            )
            .style(theme.text)
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_palette(frame: &mut Frame<'_>, app: &mut App, palette: &crate::app::Palette, theme: Theme) {
    let area = centered(frame.area(), 76, 65);
    frame.render_widget(Clear, area);
    let items = crate::palette_items(app, &palette.query);
    let mut state =
        ListState::default().with_selected((!items.is_empty()).then_some(palette.selected));
    frame.render_stateful_widget(
        List::new(
            items
                .iter()
                .map(|item| ListItem::new(item.label()))
                .collect::<Vec<_>>(),
        )
        .block(
            Block::default()
                .title(format!(
                    " Search commands and entities · /{} ",
                    palette.query
                ))
                .borders(Borders::ALL)
                .border_style(theme.border),
        )
        .style(theme.text)
        .highlight_symbol("› ")
        .highlight_style(theme.selected),
        area,
        &mut state,
    );
    let inner = area.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 1,
    });
    for index in 0..items.len().min(usize::from(inner.height)) {
        app.hit_map.push(
            Rect::new(
                inner.x,
                inner
                    .y
                    .saturating_add(u16::try_from(index).unwrap_or(u16::MAX)),
                inner.width,
                1,
            ),
            HitTarget::PaletteRow(index),
        );
    }
}

pub(crate) fn status_symbol(state: SessionState) -> &'static str {
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

pub(crate) fn status_label(state: SessionState) -> &'static str {
    match state {
        SessionState::Fresh => "Fresh",
        SessionState::Starting => "Starting",
        SessionState::Running => "Running",
        SessionState::NeedsFeedback => "Needs feedback",
        SessionState::FinishedUnseen => "Finished unseen",
        SessionState::FinishedSeen => "Finished seen",
        SessionState::Failed => "Failed",
        SessionState::Terminated => "Stopped",
        SessionState::Disconnected => "Disconnected",
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
