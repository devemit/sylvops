use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Tabs, Wrap},
};
use sylvops_core::{domain::SessionState, ui::MainTab};

use crate::{
    app::{App, ExplorerNode, FocusZone, HitMap, HitTarget, Mode},
    forms::Form,
    theme::Theme,
};

#[derive(Clone, Copy, Debug)]
pub(crate) struct ViewAreas {
    pub explorer: Option<Rect>,
    pub main: Option<Rect>,
}

pub(crate) fn draw(frame: &mut Frame<'_>, app: &mut App) {
    app.hit_map = HitMap::default();
    let theme = Theme::default();
    let area = frame.area();
    let attention_height = u16::from(app.attention_count() > 0);
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(attention_height),
            Constraint::Min(3),
            Constraint::Length(2),
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
        draw_help(frame, area);
    }
    match app.mode.clone() {
        Mode::Modal(form) => draw_form(frame, &form, &mut app.hit_map, theme),
        Mode::Confirmation(confirm) => {
            draw_confirmation(frame, &confirm, &mut app.hit_map, theme);
        }
        Mode::CommandPalette(palette) => draw_palette(frame, app, &palette, theme),
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
        "● Connected  {} sessions  {} attention",
        app.snapshot.sessions.len(),
        app.attention_count()
    );
    let rows = vec![
        Line::from(vec![
            Span::styled(" SylvOps ", theme.focus),
            Span::raw(format!("{workspace}  /  {repository}  /  {branch}")),
        ]),
        Line::from(vec![
            Span::styled(" [n] New ", theme.success),
            Span::raw(" [r] Rename  [d] Stop/Remove  [/ ] Commands  "),
            Span::styled(status, theme.muted),
        ]),
    ];
    frame.render_widget(Paragraph::new(rows), area);
    let button_width = area.width.min(35);
    app.hit_map.push(
        Rect::new(area.x, area.y.saturating_add(1), button_width / 3, 1),
        HitTarget::New,
    );
    app.hit_map.push(
        Rect::new(
            area.x.saturating_add(button_width / 3),
            area.y.saturating_add(1),
            button_width / 3,
            1,
        ),
        HitTarget::Rename,
    );
    app.hit_map.push(
        Rect::new(
            area.x.saturating_add((button_width / 3) * 2),
            area.y.saturating_add(1),
            button_width / 3,
            1,
        ),
        HitTarget::Delete,
    );
}

fn draw_attention(frame: &mut Frame<'_>, app: &mut App, area: Rect, theme: Theme) {
    frame.render_widget(
        Paragraph::new(format!(
            " ! {} session(s) need attention · Shift+A or click to open next",
            app.attention_count()
        ))
        .style(theme.warning),
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
                let (label, suffix) = match row.node {
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
                };
                let disclosure = if row.expandable {
                    if row.expanded { "▼" } else { "▶" }
                } else {
                    " "
                };
                ListItem::new(Line::from(vec![
                    Span::raw(format!(
                        "{}{} {label}",
                        "  ".repeat(usize::from(row.depth)),
                        disclosure
                    )),
                    Span::styled(suffix, theme.muted),
                ]))
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
        .border_style(if focused {
            theme.focus
        } else {
            Style::default()
        });
    let mut state =
        ListState::default().with_selected((!rows.is_empty()).then_some(app.explorer_index));
    frame.render_stateful_widget(
        List::new(items)
            .block(block)
            .highlight_symbol("› ")
            .highlight_style(theme.selected),
        area,
        &mut state,
    );
    let inner = area.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 1,
    });
    for (index, _) in rows.iter().take(usize::from(inner.height)).enumerate() {
        app.hit_map.push(
            Rect::new(
                inner.x,
                inner
                    .y
                    .saturating_add(u16::try_from(index).unwrap_or(u16::MAX)),
                inner.width,
                1,
            ),
            HitTarget::ExplorerRow(index),
        );
    }
}

fn draw_main(frame: &mut Frame<'_>, app: &mut App, area: Rect, theme: Theme) {
    app.hit_map.push(area, HitTarget::FocusMain);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(2)])
        .split(area);
    let titles: Vec<_> = ["1 Terminal", "2 Changes", "3 Details"]
        .into_iter()
        .map(Line::from)
        .collect();
    let selected = match app.main_tab {
        MainTab::Terminal => 0,
        MainTab::Changes => 1,
        MainTab::Details => 2,
    };
    let selected_session = app.selected_session();
    let title = selected_session.map_or(" Session workspace".into(), |session| {
        format!(
            " {} · {} · {}",
            session.display_name,
            session.provider_kind,
            status_label(session.state)
        )
    });
    frame.render_widget(
        Tabs::new(titles)
            .select(selected)
            .block(Block::default().title(title).borders(Borders::ALL))
            .highlight_style(theme.focus),
        chunks[0],
    );
    let tab_width = chunks[0].width.saturating_sub(2) / 3;
    for (index, tab) in [MainTab::Terminal, MainTab::Changes, MainTab::Details]
        .into_iter()
        .enumerate()
    {
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
        MainTab::Terminal => draw_terminal(frame, app, chunks[1], theme),
        MainTab::Changes => draw_changes(frame, app, chunks[1]),
        MainTab::Details => draw_details(frame, app, chunks[1], theme),
    }
}

fn draw_terminal(frame: &mut Frame<'_>, app: &mut App, area: Rect, theme: Theme) {
    let (title, content, action) = if let Some(attached) = app.attached.as_ref() {
        let role = if attached.role == sylvops_core::domain::AttachmentRole::Controller {
            "controller"
        } else {
            "read-only observer"
        };
        (
            format!(" Terminal · attached as {role} "),
            attached.parser.screen().contents(),
            Some(("[ Detach · Ctrl+] ]", HitTarget::Detach)),
        )
    } else if let Some(session) = app.selected_session() {
        (
            " Terminal · detached ".into(),
            format!(
                "{} is {}.\n\nSelecting a session never attaches automatically.\nPress Enter or click Attach to connect.",
                session.display_name,
                status_label(session.state)
            ),
            Some(("[ Attach ]", HitTarget::Attach)),
        )
    } else {
        (
            " Terminal ".into(),
            "No session selected.\n\nPress n or click New to create a shell session.".into(),
            Some(("[ New session · n ]", HitTarget::New)),
        )
    };
    let inner = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);
    frame.render_widget(
        Paragraph::new(content)
            .block(Block::default().title(title).borders(Borders::ALL))
            .wrap(Wrap { trim: false }),
        inner[0],
    );
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

fn draw_changes(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let content = app
        .diff
        .as_deref()
        .unwrap_or("Press g to load the bounded daemon-owned diff for this worktree.");
    frame.render_widget(
        Paragraph::new(content)
            .block(
                Block::default()
                    .title(" Changes · read-only ")
                    .borders(Borders::ALL),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_details(frame: &mut Frame<'_>, app: &App, area: Rect, theme: Theme) {
    let content = if let Some(session) = app.selected_session() {
        vec![
            Line::from(vec![
                Span::styled("Session      ", theme.muted),
                Span::raw(&session.display_name),
            ]),
            Line::from(vec![
                Span::styled("Provider     ", theme.muted),
                Span::raw(session.provider_kind.to_string()),
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
            Line::from(vec![
                Span::styled("Started      ", theme.muted),
                Span::raw(
                    session
                        .started_at
                        .map_or_else(|| "Not started".into(), |value| value.to_string()),
                ),
            ]),
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
                Span::raw(if session.external_session_id.is_some() {
                    "Available"
                } else {
                    "Unavailable"
                }),
            ]),
            Line::from(""),
            Line::from(
                "Safe actions: Attach, Rename, Stop. Commit, push, merge, and PR actions are not available.",
            ),
        ]
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
    } else {
        vec![Line::from(
            "Select an item in the explorer to view its details.",
        )]
    };
    frame.render_widget(
        Paragraph::new(content)
            .block(Block::default().title(" Details ").borders(Borders::ALL))
            .wrap(Wrap { trim: false }),
        area,
    );
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

fn draw_help(frame: &mut Frame<'_>, area: Rect) {
    let popup = centered(area, 78, 70);
    frame.render_widget(Clear, popup);
    frame.render_widget(Paragraph::new("Tab / Shift+Tab: Explorer ↔ Main\n↑ ↓ or j k: navigate hierarchy\n← →: collapse / expand\n1 / 2 / 3: Terminal / Changes / Details\nEnter: explicit primary action\nn: create in context\nr: rename metadata\nd: stop session or remove clean worktree\ng: bounded read-only Git diff\n/: commands and entity search\nShift+A: next attention item\nCtrl+] while attached: detach\nq: close UI; sessions continue\nMouse: select rows, tabs, actions, controls, and scroll")
        .block(Block::default().title(" Shortcuts ").borders(Borders::ALL)).wrap(Wrap { trim: false }), popup);
}

#[allow(clippy::too_many_lines)]
fn draw_form(frame: &mut Frame<'_>, form: &Form, hits: &mut HitMap, theme: Theme) {
    let area = centered(frame.area(), 76, 70);
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
                        provider.kind,
                        if provider.available {
                            ""
                        } else {
                            " (unavailable)"
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
            lines.push(Line::from(Span::styled(
                format!("  {error}"),
                theme.failure,
            )));
            row = row.saturating_add(1);
        }
    }
    if let Some(error) = &form.submission_error {
        lines.push(Line::from(Span::styled(error, theme.failure)));
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
                    .borders(Borders::ALL),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_confirmation(
    frame: &mut Frame<'_>,
    confirm: &crate::app::Confirmation,
    hits: &mut HitMap,
    theme: Theme,
) {
    let area = centered(frame.area(), 72, 35);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(confirm.detail.as_str()),
            Line::from(""),
            Line::from(vec![
                Span::styled("[ Confirm · y ]", theme.failure),
                Span::raw("   [ Cancel · Esc ]"),
            ]),
        ])
        .block(
            Block::default()
                .title(format!(" {} ", confirm.title))
                .borders(Borders::ALL),
        )
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
                .borders(Borders::ALL),
        )
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
