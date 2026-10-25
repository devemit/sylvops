use std::{borrow::Cow, fmt::Write as _};

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Tabs, Wrap},
};
use sylvops_core::{
    diff::{self, DiffLineKind},
    domain::{Session, WorktreeStatus},
    github::{
        GitHubBrowseFailure, GitHubBrowseFailureKind, GitHubCheckState, GitHubItemKind,
        GitHubItemState, GitHubReviewState,
    },
    session_state::{
        AttentionReason, ProviderActivity, SessionLifecycle, SessionStatus, lifecycle_is_active,
    },
    ui::{
        MAX_TUI_NAVIGATOR_WIDTH, MIN_TUI_NAVIGATOR_WIDTH, TuiContentView, TuiDensity,
        TuiNavigatorView, TuiSessionPresentation, TuiTheme,
    },
};

use crate::{
    app::{
        App, ExplorerNode, FocusZone, HitMap, HitTarget, MAIN_VIEWS, Mode, NavigatorResizeState,
        SettingsFocus,
    },
    directory_browser::{DirectoryBrowser, display_path},
    forms::{Form, FormKind},
    github_modal::{GitHubDepth, GitHubLoad, GitHubModal, StartWorkFocus, StartWorkForm},
    theme::Theme,
};

#[derive(Clone, Copy, Debug)]
pub(crate) struct ViewAreas {
    pub explorer: Option<Rect>,
    pub divider: Option<Rect>,
    pub main: Option<Rect>,
}

#[derive(Clone, Copy, Debug)]
struct ScreenAreas {
    header: Rect,
    attention: Option<Rect>,
    body: Rect,
    footer: Rect,
}

#[derive(Clone, Copy, Debug)]
struct MainAreas {
    tabs: Rect,
    workspace: Rect,
}

const SETTINGS_LABEL: &str = " [s] Settings ";

pub(crate) fn draw_startup(frame: &mut Frame<'_>, choice: TuiTheme, ready: bool) {
    let theme = Theme::resolve(choice);
    let area = frame.area();
    frame.render_widget(Block::default().style(theme.background), area);
    let wordmark = sylvops_core::ui::STARTUP_WORDMARK;
    let width = wordmark.lines().map(str::len).max().unwrap_or(0);
    let wide = usize::from(area.width) >= width && area.height >= 9;
    let mut lines = if wide {
        wordmark
            .lines()
            .map(|line| Line::styled(format!("{line:width$}"), theme.focus))
            .collect::<Vec<_>>()
    } else {
        vec![Line::styled("SYLVOPS", theme.focus)]
    };
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        if ready {
            "Ready"
        } else {
            "Loading workspace…"
        },
        theme.muted,
    ));
    let height = u16::try_from(lines.len())
        .unwrap_or(u16::MAX)
        .min(area.height);
    let content = Rect::new(
        area.x,
        area.y + area.height.saturating_sub(height) / 2,
        area.width,
        height,
    );
    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), content);
}

const fn focus_marker(focused: bool) -> &'static str {
    if focused { "● " } else { "  " }
}

struct ActionItem {
    label: String,
    target: HitTarget,
    style: Style,
}

pub(crate) fn draw(frame: &mut Frame<'_>, app: &mut App) {
    app.hit_map = HitMap::default();
    let theme = Theme::resolve(app.theme);
    let area = frame.area();
    frame.render_widget(Block::default().style(theme.background), area);
    let areas = screen_areas(area, app.attention_count() > 0);
    draw_header(frame, app, areas.header, theme);
    if let Some(attention) = areas.attention {
        draw_attention(frame, app, attention, theme);
    }
    let view = view_areas(areas.body, app.focus, app.navigator_width);
    if let Some(explorer) = view.explorer {
        draw_navigator(frame, app, explorer, theme);
    }
    if let Some(divider) = view.divider {
        draw_navigator_divider(frame, app, divider, theme);
    }
    if let Some(main) = view.main {
        draw_main(frame, app, main, theme);
    }
    draw_footer(frame, app, areas.footer, theme);
    if app.help {
        draw_help(frame, area, theme);
    }
    match app.mode.clone() {
        Mode::Modal(form) => draw_form(frame, &form, &mut app.hit_map, theme, app.density),
        Mode::DirectoryBrowser(browser) => {
            draw_directory_browser(frame, &browser, &mut app.hit_map, theme);
        }
        Mode::Confirmation(confirm) => {
            draw_confirmation(frame, &confirm, &mut app.hit_map, theme, app.density);
        }
        Mode::RemovalBlocked(mut notice) => {
            draw_removal_notice(frame, &mut notice, &mut app.hit_map, theme);
            app.mode = Mode::RemovalBlocked(notice);
        }
        Mode::CommandPalette(palette) => draw_palette(frame, app, &palette, theme),
        Mode::Settings(settings) => draw_settings(frame, app, settings, theme),
        Mode::GitHub(modal) => draw_github(frame, &modal, &mut app.hit_map, theme),
        Mode::Navigation | Mode::TerminalAttached => {}
    }
}

fn screen_areas(area: Rect, attention_visible: bool) -> ScreenAreas {
    let attention_height = u16::from(attention_visible);
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(attention_height),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(area);
    ScreenAreas {
        header: vertical[0],
        attention: attention_visible.then_some(vertical[1]),
        body: vertical[2],
        footer: vertical[3],
    }
}

fn main_areas(area: Rect) -> MainAreas {
    let primary = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(2)])
        .split(area);
    MainAreas {
        tabs: primary[0],
        workspace: primary[1],
    }
}

pub(crate) fn terminal_panel_area(
    area: Rect,
    focus: FocusZone,
    requested_width: u16,
    attention_visible: bool,
) -> Rect {
    let body = screen_areas(area, attention_visible).body;
    let main = view_areas(body, focus, requested_width)
        .main
        .unwrap_or(body);
    main_areas(main).workspace
}

pub(crate) fn terminal_content_area(
    area: Rect,
    focus: FocusZone,
    requested_width: u16,
    attention_visible: bool,
) -> Rect {
    let pane = terminal_panel_area(area, focus, requested_width, attention_visible);
    Block::default().borders(Borders::ALL).inner(pane)
}

pub(crate) fn view_areas(area: Rect, focus: FocusZone, requested_width: u16) -> ViewAreas {
    if area.width < 72 {
        return match focus {
            FocusZone::Explorer => ViewAreas {
                explorer: Some(area),
                divider: None,
                main: None,
            },
            FocusZone::Main => ViewAreas {
                explorer: None,
                divider: None,
                main: Some(area),
            },
        };
    }
    let maximum = area.width.saturating_sub(41).min(MAX_TUI_NAVIGATOR_WIDTH);
    let minimum = MIN_TUI_NAVIGATOR_WIDTH.min(maximum);
    let explorer_width = requested_width.clamp(minimum, maximum);
    let split = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(explorer_width),
            Constraint::Length(1),
            Constraint::Min(40),
        ])
        .split(area);
    ViewAreas {
        explorer: Some(split[0]),
        divider: Some(split[1]),
        main: Some(split[2]),
    }
}

fn draw_navigator_divider(frame: &mut Frame<'_>, app: &mut App, area: Rect, theme: Theme) {
    frame.render_widget(
        Paragraph::new(Text::from(
            (0..area.height)
                .map(|_| Line::from("│"))
                .collect::<Vec<_>>(),
        ))
        .style(
            if app.navigator_resizing == NavigatorResizeState::Dragging {
                theme.focus
            } else {
                theme.border
            },
        ),
        area,
    );
    app.hit_map.push(area, HitTarget::NavigatorDivider);
}

fn draw_navigator(frame: &mut Frame<'_>, app: &mut App, area: Rect, theme: Theme) {
    app.hit_map.push(area, HitTarget::FocusExplorer);
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(2)])
        .split(area);
    let session_count = app.session_overview().len();
    let navigator_focused = app.focus == FocusZone::Explorer;
    let explorer_selected = app.navigator_view == TuiNavigatorView::Explorer;
    let active_selected = app.navigator_view == TuiNavigatorView::Active;
    let explorer_label = format!(
        "{}[e] Explorer",
        focus_marker(navigator_focused && explorer_selected)
    );
    let active_label = format!(
        "{}[a] Sessions ({session_count})",
        focus_marker(navigator_focused && active_selected)
    );
    let explorer_style = if navigator_focused && explorer_selected {
        theme.focus
    } else if explorer_selected {
        theme.selected
    } else {
        theme.text
    };
    let active_style = if navigator_focused && active_selected {
        theme.focus
    } else if active_selected {
        theme.selected
    } else {
        theme.text
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(format!("{explorer_label}  "), explorer_style),
            Span::styled(active_label.clone(), active_style),
        ])),
        sections[0],
    );
    let explorer_width = text_width(&explorer_label).saturating_add(2);
    app.hit_map.push(
        Rect::new(sections[0].x, sections[0].y, explorer_width, 1),
        HitTarget::NavigatorExplorer,
    );
    app.hit_map.push(
        Rect::new(
            sections[0].x.saturating_add(explorer_width),
            sections[0].y,
            text_width(&active_label),
            1,
        ),
        HitTarget::NavigatorActive,
    );
    match app.navigator_view {
        TuiNavigatorView::Explorer => draw_explorer(frame, app, sections[1], theme),
        TuiNavigatorView::Active => draw_sessions(frame, app, sections[1], theme),
    }
}

fn draw_header(frame: &mut Frame<'_>, app: &mut App, area: Rect, theme: Theme) {
    let workspace = app
        .active_workspace()
        .map_or_else(|| "No Workspace".to_owned(), |item| item.name.clone());
    let attention = app.attention_count();
    let active = app.active_sessions().len();
    let full = app.density == TuiDensity::Comfortable && area.width >= 72;
    let context = if full {
        format!(" SylvOps · Workspace: {workspace}")
    } else {
        format!(" W:{workspace}")
    };
    let summary = if attention > 0 {
        if full {
            if attention == 1 {
                "1 needs attention".to_owned()
            } else {
                format!("{attention} need attention")
            }
        } else {
            format!("!{attention}")
        }
    } else if full {
        format!("{active} active Sessions")
    } else {
        format!("{active} active")
    };
    let connectivity = if full { "● Connected" } else { "●" };
    let status = format!(" {connectivity} · {summary} ");
    let settings_width = text_width(SETTINGS_LABEL).min(area.width);
    let status_width = text_width(&status).min(area.width.saturating_sub(settings_width));
    let reserved = status_width.saturating_add(settings_width);
    let context_area = Rect::new(area.x, area.y, area.width.saturating_sub(reserved), 1);
    let status_area = Rect::new(
        area.x.saturating_add(context_area.width),
        area.y,
        status_width,
        1,
    );
    let settings_area = Rect::new(
        area.x
            .saturating_add(area.width.saturating_sub(settings_width)),
        area.y,
        settings_width,
        1,
    );
    frame.render_widget(
        Paragraph::new(truncate_text(&context, context_area.width)).style(theme.text),
        context_area,
    );
    frame.render_widget(
        Paragraph::new(status)
            .alignment(Alignment::Right)
            .style(theme.muted),
        status_area,
    );
    frame.render_widget(
        Paragraph::new(SETTINGS_LABEL)
            .alignment(Alignment::Right)
            .style(theme.focus),
        settings_area,
    );
    app.hit_map.push(settings_area, HitTarget::Settings);
}

fn draw_attention(frame: &mut Frame<'_>, app: &mut App, area: Rect, theme: Theme) {
    frame.render_widget(
        Paragraph::new(attention_message(app.attention_count())).style(theme.attention),
        area,
    );
    app.hit_map.push(area, HitTarget::Attention);
}

pub(crate) fn attention_message(count: usize) -> String {
    let subject = if count == 1 {
        "1 session needs attention".to_owned()
    } else {
        format!("{count} sessions need attention")
    };
    format!(" ! {subject} · Shift+A or click to open next")
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
    let block = Block::default()
        .title(" Explorer ")
        .borders(Borders::ALL)
        .border_style(theme.border);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let mut state =
        ListState::default().with_selected((!rows.is_empty()).then_some(app.explorer_index));
    frame.render_stateful_widget(
        List::new(items)
            .style(theme.text)
            .highlight_symbol("› ")
            .highlight_style(theme.selected),
        inner,
        &mut state,
    );
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
                    format!(
                        "{} {}",
                        status_symbol(item.status, app.status_animation_frame()),
                        app.session_row_name(item)
                    ),
                    format!("  {}", status_label(item.status)),
                )
            }),
    }
}

fn draw_main(frame: &mut Frame<'_>, app: &mut App, area: Rect, theme: Theme) {
    app.hit_map.push(area, HitTarget::FocusMain);
    let areas = main_areas(area);
    let titles: Vec<_> = MAIN_VIEWS
        .into_iter()
        .map(|view| Line::from(format!("{} {}", view.shortcut(), view.label())))
        .collect();
    let selected = MAIN_VIEWS
        .iter()
        .position(|view| *view == app.main_tab)
        .unwrap_or(0);
    let focus_label = format!(" {}Main", focus_marker(app.focus == FocusZone::Main));
    let title = app.selected_session().map_or_else(
        || format!("{focus_label} · Session workspace "),
        |session| {
            format!(
                "{focus_label} · {} · {} · {} ",
                app.session_row_name(session),
                session.provider_kind.display_name(),
                status_label(session.status)
            )
        },
    );
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
            .highlight_style(theme.focus.add_modifier(Modifier::REVERSED)),
        areas.tabs,
    );
    let tab_count = u16::try_from(MAIN_VIEWS.len()).unwrap_or(1);
    let tab_width = areas.tabs.width.saturating_sub(2) / tab_count;
    for (index, tab) in MAIN_VIEWS.into_iter().enumerate() {
        app.hit_map.push(
            Rect::new(
                areas.tabs.x.saturating_add(
                    1 + tab_width.saturating_mul(u16::try_from(index).unwrap_or(0)),
                ),
                areas.tabs.y.saturating_add(1),
                tab_width,
                1,
            ),
            HitTarget::Tab(tab),
        );
    }
    match app.main_tab {
        TuiContentView::Terminal | TuiContentView::Sessions => {
            draw_terminal(frame, app, areas.workspace, theme);
        }
        TuiContentView::Changes => draw_changes(frame, app, areas.workspace, theme),
        TuiContentView::Details => draw_details(frame, app, areas.workspace, theme),
    }
}

struct SessionOverviewItem {
    id: sylvops_core::ids::SessionId,
    name: String,
    status: SessionStatus,
    provider: &'static str,
    project: String,
    worktree: String,
    last_activity_at: i64,
    attention: Option<AttentionReason>,
}

const SESSION_CARD_HEIGHT: u16 = 7;
const SESSION_LIST_ROW_HEIGHT: u16 = 1;

fn draw_sessions(frame: &mut Frame<'_>, app: &mut App, area: Rect, theme: Theme) {
    let attention = app
        .needs_attention_sessions()
        .into_iter()
        .map(|session| {
            session_item(
                app,
                session,
                session.status.attention.map(|event| event.reason),
            )
        })
        .collect::<Vec<_>>();
    let active = app
        .active_sessions()
        .into_iter()
        .map(|session| session_item(app, session, None))
        .collect::<Vec<_>>();

    if attention.is_empty() && active.is_empty() {
        let block = Block::default()
            .title(format!(
                " Active Sessions · 0 · {} ",
                app.session_presentation
            ))
            .borders(Borders::ALL)
            .border_style(theme.border);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        draw_sessions_empty(frame, app, inner, theme);
        return;
    }

    if attention.is_empty() {
        draw_session_group(frame, app, area, "Active Sessions", &active, theme);
    } else if active.is_empty() {
        draw_session_group(frame, app, area, "Needs Attention", &attention, theme);
    } else {
        let attention_height = session_group_height(
            attention.len(),
            area.width.saturating_sub(2),
            app.session_presentation,
        )
        .min(area.height.saturating_sub(3))
        .max(3);
        let groups = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(attention_height), Constraint::Min(3)])
            .split(area);
        draw_session_group(frame, app, groups[0], "Needs Attention", &attention, theme);
        draw_session_group(frame, app, groups[1], "Active Sessions", &active, theme);
    }
}

fn session_item(
    app: &App,
    session: &Session,
    attention: Option<AttentionReason>,
) -> SessionOverviewItem {
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
    SessionOverviewItem {
        id: session.id,
        name: app.session_row_name(session),
        status: session.status,
        provider: session.provider_kind.display_name(),
        project: project
            .map_or("Unknown Project", |project| project.name.as_str())
            .to_owned(),
        worktree: worktree
            .map_or("Unknown Worktree", |worktree| worktree.name.as_str())
            .to_owned(),
        last_activity_at: session.last_activity_at,
        attention,
    }
}

fn session_group_height(count: usize, width: u16, presentation: TuiSessionPresentation) -> u16 {
    let columns = session_columns(width, presentation);
    let rows = count.div_ceil(usize::from(columns));
    u16::try_from(rows)
        .unwrap_or(u16::MAX)
        .saturating_mul(session_item_height(presentation))
        .saturating_add(2)
}

fn draw_session_group(
    frame: &mut Frame<'_>,
    app: &mut App,
    area: Rect,
    label: &str,
    sessions: &[SessionOverviewItem],
    theme: Theme,
) {
    let block = Block::default()
        .title(format!(
            " {label} · {} · {} ",
            sessions.len(),
            app.session_presentation
        ))
        .borders(Borders::ALL)
        .border_style(theme.border);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let columns = session_columns(inner.width, app.session_presentation);
    let item_height = session_item_height(app.session_presentation);
    let gap = u16::from(columns == 2);
    let item_width = inner
        .width
        .saturating_sub(gap)
        .checked_div(columns)
        .unwrap_or(inner.width);
    let selected_index = app
        .selected_session_id
        .and_then(|selected| sessions.iter().position(|session| session.id == selected))
        .unwrap_or(0);
    let selected_row = selected_index / usize::from(columns);
    let visible_rows = usize::from(inner.height / item_height).max(1);
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
                .saturating_mul(item_width.saturating_add(gap)),
        );
        let y = inner.y.saturating_add(
            u16::try_from(visible_row)
                .unwrap_or(u16::MAX)
                .saturating_mul(item_height),
        );
        let item_area = Rect::new(
            x,
            y,
            item_width.min(inner.x.saturating_add(inner.width).saturating_sub(x)),
            item_height.min(inner.y.saturating_add(inner.height).saturating_sub(y)),
        );
        draw_session_overview_item(
            frame,
            app,
            item_area,
            session,
            app.selected_session_id == Some(session.id),
            theme,
        );
    }
}

const fn session_item_height(presentation: TuiSessionPresentation) -> u16 {
    match presentation {
        TuiSessionPresentation::Cards => SESSION_CARD_HEIGHT,
        TuiSessionPresentation::List => SESSION_LIST_ROW_HEIGHT,
    }
}

const fn session_columns(width: u16, presentation: TuiSessionPresentation) -> u16 {
    if matches!(presentation, TuiSessionPresentation::Cards) && width.saturating_sub(1) / 2 >= 36 {
        2
    } else {
        1
    }
}

fn draw_session_overview_item(
    frame: &mut Frame<'_>,
    app: &mut App,
    area: Rect,
    session: &SessionOverviewItem,
    selected: bool,
    theme: Theme,
) {
    match app.session_presentation {
        TuiSessionPresentation::Cards => {
            draw_session_overview_card(frame, app, area, session, selected, theme);
        }
        TuiSessionPresentation::List => {
            draw_session_overview_list(frame, app, area, session, selected, theme);
        }
    }
}

fn draw_session_overview_list(
    frame: &mut Frame<'_>,
    app: &mut App,
    area: Rect,
    session: &SessionOverviewItem,
    selected: bool,
    theme: Theme,
) {
    let animation_frame = app.status_animation_frame();
    let (symbol, label) = session.attention.map_or_else(
        || {
            (
                status_symbol(session.status, animation_frame),
                status_label(session.status),
            )
        },
        |reason| {
            (
                attention_reason_icon(reason),
                attention_reason_label(reason),
            )
        },
    );
    let line = Line::from(vec![
        Span::styled(format!("{symbol} {}", session.name), Modifier::BOLD),
        Span::raw(format!(" · {label}")),
    ]);
    let style = session.attention.map_or_else(
        || session_selection_style(selected, theme),
        |reason| theme.attention_surface(reason, selected),
    );
    frame.render_widget(Paragraph::new(line).style(style), area);
    app.hit_map
        .push(area, HitTarget::SessionOverview(session.id));
}

fn draw_session_overview_card(
    frame: &mut Frame<'_>,
    app: &mut App,
    area: Rect,
    session: &SessionOverviewItem,
    selected: bool,
    theme: Theme,
) {
    let surface = session.attention.map_or_else(
        || session_selection_style(selected, theme),
        |reason| theme.attention_surface(reason, selected),
    );
    let border = if let Some(reason) = session.attention {
        attention_reason_emphasis(reason, theme).add_modifier(if selected {
            Modifier::BOLD
        } else {
            Modifier::empty()
        })
    } else if selected {
        theme.focus
    } else {
        theme.border
    };
    let border = Style {
        bg: surface.bg,
        ..border
    };
    let animation_frame = app.status_animation_frame();
    let (symbol, label) = session.attention.map_or_else(
        || {
            (
                status_symbol(session.status, animation_frame),
                status_label(session.status),
            )
        },
        |reason| {
            (
                attention_reason_icon(reason),
                attention_reason_label(reason),
            )
        },
    );
    let actions = match session.attention {
        Some(AttentionReason::RecoverableFailure) => "[ Open terminal · Enter ]  [ Dismiss · d ]",
        Some(AttentionReason::SuccessfulCompletion | AttentionReason::TerminalFailure) => {
            "[ Open history · Enter ]"
        }
        _ => "[ Open terminal · Enter ]  [ Stop · d ]",
    };
    let lines = vec![
        Line::from(vec![
            Span::styled(format!("{symbol} {}", session.name), Modifier::BOLD),
            Span::raw(format!(" · {label}")),
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
        Line::from(actions),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).border_style(border))
            .style(surface)
            .wrap(Wrap { trim: true }),
        area,
    );
    app.hit_map
        .push(area, HitTarget::SessionOverview(session.id));
    if area.height >= 2 {
        let actions_y = area.y.saturating_add(area.height.saturating_sub(2));
        let open_width = 25.min(area.width.saturating_sub(2));
        app.hit_map.push(
            Rect::new(area.x.saturating_add(1), actions_y, open_width, 1),
            HitTarget::SessionOverviewOpen(session.id),
        );
        let stop_x = area
            .x
            .saturating_add(28)
            .min(area.x.saturating_add(area.width));
        let secondary = match session.attention {
            Some(AttentionReason::RecoverableFailure) => {
                Some(HitTarget::AttentionDismiss(session.id))
            }
            Some(AttentionReason::SuccessfulCompletion | AttentionReason::TerminalFailure) => None,
            _ => Some(HitTarget::SessionOverviewStop(session.id)),
        };
        if let Some(target) = secondary {
            app.hit_map.push(
                Rect::new(
                    stop_x,
                    actions_y,
                    12.min(area.x.saturating_add(area.width).saturating_sub(stop_x)),
                    1,
                ),
                target,
            );
        }
    }
}

fn session_selection_style(selected: bool, theme: Theme) -> Style {
    if selected { theme.selected } else { theme.text }
}

const fn attention_reason_icon(reason: AttentionReason) -> &'static str {
    match reason {
        AttentionReason::InputRequested => "?",
        AttentionReason::RecoverableFailure => "!",
        AttentionReason::TerminalFailure => "×",
        AttentionReason::SuccessfulCompletion => "✓",
    }
}

const fn attention_reason_label(reason: AttentionReason) -> &'static str {
    match reason {
        AttentionReason::InputRequested => "Input requested",
        AttentionReason::RecoverableFailure => "Recoverable failure",
        AttentionReason::TerminalFailure => "Terminal failure",
        AttentionReason::SuccessfulCompletion => "Successful Completion",
    }
}

const fn attention_reason_emphasis(reason: AttentionReason, theme: Theme) -> Style {
    match reason {
        AttentionReason::InputRequested | AttentionReason::RecoverableFailure => theme.attention,
        AttentionReason::TerminalFailure => theme.danger,
        AttentionReason::SuccessfulCompletion => theme.success,
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
    let (title, content) = if let Some(attached) = app.attached.as_ref() {
        let role = if attached.role == sylvops_core::domain::AttachmentRole::Controller {
            "controller"
        } else {
            "read-only observer"
        };
        let history = attached.parser.screen().scrollback();
        let title = if attached.is_scrolled_back() {
            format!(" Terminal · {role} · history {history} rows · Shift+End latest ")
        } else {
            format!(" Terminal · attached as {role} · wheel scrolls history ")
        };
        (title, terminal_text(attached.parser.screen(), theme.text))
    } else if let Some(session) = app.selected_session() {
        (
            " Terminal · detached ".into(),
            Text::from(format!(
                "{} is {}.\n\nSelecting a session never attaches automatically.\n{}",
                session.display_name,
                status_label(session.status),
                if lifecycle_is_active(session.status.lifecycle) {
                    "Press Enter or click Attach to connect."
                } else {
                    "Press Enter to follow this Conversation to its latest Session. If resume is unavailable, history remains open with the exact reason and New starts fresh."
                }
            )),
        )
    } else {
        (
            " Terminal ".into(),
            Text::from("No session selected.\n\nPress n or click New to create a shell session."),
        )
    };
    let mut terminal = Paragraph::new(content).block(
        Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(theme.border),
    );
    if !provider_owned {
        terminal = terminal.style(theme.text).wrap(Wrap { trim: false });
    }
    frame.render_widget(terminal, area);
    if let Some(session) = app
        .attached
        .as_ref()
        .and_then(|attached| {
            app.snapshot
                .sessions
                .iter()
                .find(|s| s.id == attached.session_id)
        })
        .or_else(|| app.selected_session())
        && let Some(event) = session.status.attention
        && event.acknowledgement == sylvops_core::session_state::AttentionAcknowledgement::Pending
        && matches!(
            event.reason,
            AttentionReason::SuccessfulCompletion | AttentionReason::TerminalFailure
        )
        && area.width >= 20
        && area.height >= 2
    {
        let target = HitTarget::MarkReviewed(session.id, event.id);
        let button = Rect::new(
            area.right().saturating_sub(19),
            area.bottom().saturating_sub(1),
            18,
            1,
        );
        frame.render_widget(
            Paragraph::new("[ Mark reviewed ]").style(theme.focus),
            button,
        );
        app.hit_map.push(button, target);
    }
    let viewport = area.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 1,
    });
    app.hit_map.push(viewport, HitTarget::TerminalViewport);

    if let Some(screen) = app
        .attached
        .as_ref()
        .map(|attached| attached.parser.screen())
        && !screen.hide_cursor()
        && screen.scrollback() == 0
    {
        let (row, column) = screen.cursor_position();
        let inner_width = area.width.saturating_sub(2);
        let inner_height = area.height.saturating_sub(2);
        if column < inner_width && row < inner_height {
            frame.set_cursor_position(Position::new(
                area.x.saturating_add(1).saturating_add(column),
                area.y.saturating_add(1).saturating_add(row),
            ));
        }
    }
}

fn terminal_text(screen: &vt100::Screen, defaults: Style) -> Text<'_> {
    let (rows, columns) = screen.size();
    let mut lines = Vec::with_capacity(usize::from(rows));
    for row in 0..rows {
        let mut spans = Vec::with_capacity(usize::from(columns));
        for column in 0..columns {
            let Some(cell) = screen.cell(row, column) else {
                continue;
            };
            if cell.is_wide_continuation() {
                continue;
            }
            // Keep cell boundaries: combining neighboring cells into one span can
            // create a new grapheme and move subsequent letters to different columns.
            spans.push(Span::styled(
                terminal_cell_text(cell),
                terminal_cell_style(cell, defaults),
            ));
        }
        lines.push(Line::from(spans));
    }
    Text::from(lines)
}

fn terminal_cell_text(cell: &vt100::Cell) -> Cow<'_, str> {
    if !cell.has_contents() {
        return Cow::Borrowed(" ");
    }
    let contents = cell.contents();
    let width = if cell.is_wide() { 2 } else { 1 };
    if !contents.contains('\u{200d}') && Span::raw(contents).width() == width {
        return Cow::Borrowed(contents);
    }

    // The VT grid is authoritative. A presentation selector must not widen a
    // one-column cell, and a trailing joiner must not join a different VT cell.
    let mut text = contents.replace('\u{200d}', "");
    if Span::raw(text.as_str()).width() != width {
        text.retain(|character| !matches!(character, '\u{fe0e}' | '\u{fe0f}' | '\u{20e3}'));
    }
    if Span::raw(text.as_str()).width() == width {
        return Cow::Owned(text);
    }
    let base = contents
        .chars()
        .next()
        .expect("occupied VT cell has content")
        .to_string();
    if Span::raw(base.as_str()).width() == width {
        return Cow::Owned(base);
    }
    Cow::Owned("�".repeat(width))
}

fn terminal_cell_style(cell: &vt100::Cell, defaults: Style) -> Style {
    // Host defaults can change independently of the selected application theme.
    // Resolve only default colors here; explicit Provider colors stay intact.
    let mut style = Style::default()
        .fg(terminal_color(
            cell.fgcolor(),
            defaults.fg.unwrap_or(Color::Reset),
        ))
        .bg(terminal_color(
            cell.bgcolor(),
            defaults.bg.unwrap_or(Color::Reset),
        ));
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

const fn terminal_color(color: vt100::Color, fallback: Color) -> Color {
    match color {
        vt100::Color::Default => fallback,
        vt100::Color::Idx(index) => Color::Indexed(index),
        vt100::Color::Rgb(red, green, blue) => Color::Rgb(red, green, blue),
    }
}

fn draw_changes(frame: &mut Frame<'_>, app: &App, area: Rect, theme: Theme) {
    let content = app
        .diff
        .as_deref()
        .unwrap_or("Press g to load the bounded daemon-owned diff for this worktree.");
    let block = Block::default()
        .title(" Changes · read-only · ↑/↓ PgUp/PgDn ")
        .borders(Borders::ALL)
        .border_style(theme.border);
    let inner_width = block.inner(area).width;
    frame.render_widget(
        Paragraph::new(styled_diff_lines(content, inner_width, theme))
            .block(block)
            .style(theme.text)
            .scroll((app.diff_scroll, 0))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn styled_diff_lines(content: &str, width: u16, theme: Theme) -> Vec<Line<'static>> {
    diff::lines(content)
        .map(|line| {
            let mut text = line.text.to_owned();
            let padding = width.saturating_sub(text_width(&text));
            text.extend(std::iter::repeat_n(' ', usize::from(padding)));
            let style = match line.kind {
                DiffLineKind::FileHeader => theme.diff_file_header,
                DiffLineKind::Metadata | DiffLineKind::Notice => theme.muted,
                DiffLineKind::HunkHeader => theme.diff_hunk_header,
                DiffLineKind::Addition => theme.diff_addition,
                DiffLineKind::Deletion => theme.diff_deletion,
                DiffLineKind::Context => theme.text,
            };
            Line::styled(text, style)
        })
        .collect()
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
    let mut lines = vec![
        Line::from(
            sylvops_core::attention::conversation_identity_guidance(session).map_or("", |_| {
                if session.external_session_id.is_some() {
                    "Conversation identity recorded"
                } else if lifecycle_is_active(session.status.lifecycle) {
                    if session.provider_kind == sylvops_core::domain::ProviderKind::Codex {
                        "Identity missing; /hooks: trust (t), check Active, prompt."
                    } else {
                        "Identity missing; check Claude SessionStart hooks."
                    }
                } else {
                    "Identity never recorded; cannot resume. Use Start Fresh."
                }
            }),
        ),
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
                status_symbol(session.status, app.status_animation_frame()),
                status_label(session.status)
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
            Span::raw(if lifecycle_is_active(session.status.lifecycle) {
                "Live Session · Enter attaches"
            } else {
                "Daemon-checked · press u · New starts fresh"
            }),
        ]),
        Line::from(""),
        Line::from(
            "Safe actions: Attach, Resume, Rename, Stop. Commit, push, merge, and PR actions are not available.",
        ),
    ];
    lines.extend(work_source_detail_lines(app, session, theme));
    if let Some((_, health)) = app.lifecycle_health.filter(|(id, _)| *id == session.id) {
        lines.push(Line::from(health.guidance()));
    }
    lines
}

fn work_source_detail_lines<'a>(app: &'a App, session: &Session, theme: Theme) -> Vec<Line<'a>> {
    let Some(source) = app
        .snapshot
        .work_sources
        .iter()
        .find(|source| source.worktree_id == session.worktree_id)
    else {
        return Vec::new();
    };
    let repository_status = match app.github_status.as_ref() {
        Some(status)
            if status.association.as_ref().is_some_and(|association| {
                association.project_id == source.project_id
                    && association.repository == source.item.repository
            }) =>
        {
            "Associated · press G to return to Issue detail"
        }
        Some(_) => "Repository changed · historical source preserved",
        None => "Not refreshed · press G to return to Issue detail",
    };
    vec![
        Line::from(""),
        Line::styled("Work Source", Modifier::BOLD),
        owned_detail_line(
            "Issue        ",
            format!("{} #{}", source.item.repository, source.item.number),
            theme,
        ),
        owned_detail_line(
            "Local status ",
            sylvops_core::github::issue_work_status(
                &source.item,
                &app.snapshot.work_sources,
                &app.snapshot.worktrees,
                &app.snapshot.sessions,
            )
            .display_name()
            .into(),
            theme,
        ),
        owned_detail_line("Repository   ", repository_status.into(), theme),
    ]
}

fn owned_detail_line(label: &'static str, value: String, theme: Theme) -> Line<'static> {
    Line::from(vec![Span::styled(label, theme.muted), Span::raw(value)])
}

fn optional_number<T: ToString>(value: Option<T>, fallback: &str) -> String {
    value.map_or_else(|| fallback.to_owned(), |value| value.to_string())
}

fn action_label(label: impl Into<String>, target: HitTarget, style: Style) -> ActionItem {
    ActionItem {
        label: format!("{} ", label.into()),
        target,
        style,
    }
}

fn draw_action_line(
    frame: &mut Frame<'_>,
    hits: &mut HitMap,
    area: Rect,
    actions: &[ActionItem],
    theme: Theme,
) {
    frame.render_widget(Block::default().style(theme.muted), area);
    let mut x = area.x;
    let right = area.x.saturating_add(area.width);
    for action in actions {
        let width = text_width(&action.label);
        if width == 0 || x.saturating_add(width) > right {
            break;
        }
        let action_area = Rect::new(x, area.y, width, 1.min(area.height));
        frame.render_widget(
            Paragraph::new(action.label.as_str()).style(action.style),
            action_area,
        );
        hits.push(action_area, action.target);
        x = x.saturating_add(width);
    }
}

fn removal_action(app: &App) -> Option<&'static str> {
    match app.selected_node() {
        Some(ExplorerNode::Workspace(_)) => Some("Delete Workspace"),
        Some(ExplorerNode::Project(_)) => Some("Remove Project"),
        Some(ExplorerNode::Worktree(_)) if worktree_removal_available(app) => {
            Some("Remove Worktree")
        }
        Some(ExplorerNode::Worktree(_)) | None => None,
        Some(ExplorerNode::Session(_)) if session_is_active(app.selected_session()) => {
            Some("Stop Session")
        }
        Some(ExplorerNode::Session(_)) => Some("Delete Session"),
    }
}

fn worktree_removal_available(app: &App) -> bool {
    app.selected_worktree().is_some_and(|worktree| {
        worktree.status == WorktreeStatus::Active
            && !worktree.is_root_checkout
            && !app.snapshot.sessions.iter().any(|session| {
                session.worktree_id == worktree.id && session_is_active(Some(session))
            })
    })
}

fn session_is_active(session: Option<&Session>) -> bool {
    session.is_some_and(|session| lifecycle_is_active(session.status.lifecycle))
}

fn contextual_actions(app: &App, width: u16, theme: Theme) -> Vec<ActionItem> {
    let terse = width < 60 || app.density == TuiDensity::Compact;
    match app.selected_node() {
        Some(ExplorerNode::Workspace(_)) => workspace_actions(terse, theme),
        Some(ExplorerNode::Project(_)) => project_actions(terse, theme),
        Some(ExplorerNode::Worktree(_)) => worktree_actions(app, terse, theme),
        Some(ExplorerNode::Session(_)) => session_actions(app, terse, theme),
        None => Vec::new(),
    }
}

fn workspace_actions(terse: bool, theme: Theme) -> Vec<ActionItem> {
    if terse {
        vec![
            action_label("[d] Delete Workspace", HitTarget::Delete, theme.danger),
            action_label("[n]Proj", HitTarget::New, theme.success),
        ]
    } else {
        vec![
            action_label("[n] Add Project", HitTarget::New, theme.success),
            action_label("[d] Delete Workspace", HitTarget::Delete, theme.danger),
        ]
    }
}

fn worktree_actions(app: &App, terse: bool, theme: Theme) -> Vec<ActionItem> {
    let active = app
        .selected_worktree()
        .is_some_and(|worktree| worktree.status == WorktreeStatus::Active);
    let mut actions = Vec::new();
    if terse && worktree_removal_available(app) {
        actions.push(action_label(
            "[d] Remove Worktree",
            HitTarget::Delete,
            theme.danger,
        ));
    }
    if active {
        actions.push(action_label(
            if terse { "[n]Sess" } else { "[n] New Session" },
            HitTarget::New,
            theme.success,
        ));
    }
    actions.push(new_worktree_action(terse, theme));
    actions.push(action_label(
        if terse { "[r]Name" } else { "[r] Rename" },
        HitTarget::Rename,
        theme.text,
    ));
    if !terse && worktree_removal_available(app) {
        actions.push(action_label(
            "[d] Remove Worktree",
            HitTarget::Delete,
            theme.danger,
        ));
    }
    actions
}

fn project_actions(terse: bool, theme: Theme) -> Vec<ActionItem> {
    if terse {
        vec![
            action_label("[d] Remove Project", HitTarget::Delete, theme.danger),
            new_worktree_action(true, theme),
            action_label("[r]Name", HitTarget::Rename, theme.text),
        ]
    } else {
        vec![
            new_worktree_action(false, theme),
            action_label("[r] Rename", HitTarget::Rename, theme.text),
            action_label("[d] Remove Project", HitTarget::Delete, theme.danger),
        ]
    }
}

fn new_worktree_action(terse: bool, theme: Theme) -> ActionItem {
    action_label(
        if terse { "[w]Tree" } else { "[w] New Worktree" },
        HitTarget::NewWorktree,
        theme.success,
    )
}

fn session_actions(app: &App, terse: bool, theme: Theme) -> Vec<ActionItem> {
    let active = session_is_active(app.selected_session());
    let removal = if active {
        "[d] Stop Session"
    } else {
        "[d] Delete Session"
    };
    let primary = primary_session_action(app, terse, theme);
    let rename = action_label(
        if terse { "[r]Name" } else { "[r] Rename" },
        HitTarget::Rename,
        theme.text,
    );
    let start_fresh = action_label(
        if active {
            if terse { "[n]New" } else { "[n] New Session" }
        } else if terse {
            "[n]Fresh"
        } else {
            "[n] Start Fresh"
        },
        HitTarget::New,
        theme.success,
    );
    let remove = action_label(removal, HitTarget::Delete, theme.danger);
    let new_worktree = new_worktree_action(terse, theme);
    if terse {
        let mut actions = vec![remove];
        if let Some(primary) = primary {
            actions.push(primary);
        }
        actions.push(start_fresh);
        actions.push(new_worktree);
        actions.push(rename);
        actions
    } else {
        let mut actions = Vec::new();
        if let Some(primary) = primary {
            actions.push(primary);
        }
        actions.push(start_fresh);
        actions.push(new_worktree);
        actions.push(rename);
        actions.push(remove);
        actions
    }
}

fn primary_session_action(app: &App, terse: bool, theme: Theme) -> Option<ActionItem> {
    if app.attached.is_some() {
        return Some(action_label(
            if terse {
                "[C-]]Detach"
            } else {
                "[Ctrl+]] Detach"
            },
            HitTarget::Detach,
            theme.focus,
        ));
    }
    let session = app.selected_session()?;
    Some(if lifecycle_is_active(session.status.lifecycle) {
        action_label(
            if terse { "[E]Attach" } else { "[Enter] Attach" },
            HitTarget::Attach,
            theme.focus,
        )
    } else {
        action_label("[u] Resume", HitTarget::Resume, theme.focus)
    })
}

fn explorer_footer_actions(app: &App, width: u16, theme: Theme) -> Vec<ActionItem> {
    let terse = width < 60 || app.density == TuiDensity::Compact;
    let navigator_label = if app.navigator_view == TuiNavigatorView::Active {
        "Sessions"
    } else {
        "Explorer"
    };
    let mut items = vec![action_label(
        if terse {
            format!("F:{navigator_label}")
        } else {
            format!("Focus: {navigator_label}")
        },
        HitTarget::FocusExplorer,
        theme.focus,
    )];
    items.push(action_label(
        if width < 60 {
            "[Tab]M"
        } else if terse {
            "[Tab] Main"
        } else {
            "[Tab] Focus Main"
        },
        HitTarget::FocusMain,
        theme.text,
    ));
    if app.navigator_view == TuiNavigatorView::Active {
        if let Some(session) = app.selected_session_id.and_then(|id| {
            app.session_overview()
                .into_iter()
                .find(|session| session.id == id)
        }) {
            items.push(action_label(
                "[Enter] Open",
                HitTarget::SessionOverviewOpen(session.id),
                theme.focus,
            ));
            match session.status.attention.map(|event| event.reason) {
                Some(AttentionReason::RecoverableFailure) => items.push(action_label(
                    "[d] Dismiss",
                    HitTarget::AttentionDismiss(session.id),
                    theme.attention,
                )),
                Some(AttentionReason::SuccessfulCompletion | AttentionReason::TerminalFailure) => {}
                _ => items.push(action_label(
                    "[d] Stop",
                    HitTarget::SessionOverviewStop(session.id),
                    theme.danger,
                )),
            }
        }
    } else {
        items.extend(contextual_actions(app, width, theme));
    }
    items
}

fn main_footer_actions(app: &App, width: u16, theme: Theme) -> Vec<ActionItem> {
    let terse = app.density == TuiDensity::Compact || width < 60;
    let mut items = vec![action_label(
        if terse {
            format!("F:{}", app.main_tab.label())
        } else {
            format!("Focus: {}", app.main_tab.label())
        },
        HitTarget::FocusMain,
        theme.focus,
    )];
    match app.main_tab {
        TuiContentView::Sessions | TuiContentView::Terminal => {
            add_terminal_footer_action(app, &mut items, theme);
        }
        TuiContentView::Changes if app.selected_worktree_id.is_some() => {
            items.push(action_label("[g] Refresh", HitTarget::Diff, theme.focus));
        }
        TuiContentView::Details if app.selected_node().is_some() => {
            if matches!(
                app.selected_node(),
                Some(
                    ExplorerNode::Project(_) | ExplorerNode::Worktree(_) | ExplorerNode::Session(_)
                )
            ) {
                items.push(action_label("[r] Rename", HitTarget::Rename, theme.text));
            }
            if let Some(removal) = removal_action(app) {
                items.push(action_label(
                    format!("[d] {removal}"),
                    HitTarget::Delete,
                    theme.danger,
                ));
            }
        }
        TuiContentView::Changes | TuiContentView::Details => {}
    }
    items.push(action_label(
        if terse {
            "[Tab] Explorer"
        } else {
            "[Tab] Focus Explorer"
        },
        HitTarget::FocusExplorer,
        theme.text,
    ));
    items
}

fn add_terminal_footer_action(app: &App, items: &mut Vec<ActionItem>, theme: Theme) {
    if let Some(action) = primary_session_action(app, false, theme) {
        items.push(action);
    } else {
        items.push(action_label("[n] New", HitTarget::New, theme.success));
    }
}

fn footer_actions(app: &App, width: u16, theme: Theme) -> Vec<ActionItem> {
    let mut actions = match app.focus {
        FocusZone::Explorer => explorer_footer_actions(app, width, theme),
        FocusZone::Main => main_footer_actions(app, width, theme),
    };
    actions.push(action_label(
        "[/] Commands",
        HitTarget::Commands,
        theme.text,
    ));
    actions.push(action_label(
        "[s] Settings",
        HitTarget::Settings,
        theme.text,
    ));
    actions.push(action_label("[?] Help", HitTarget::Help, theme.text));
    actions
}

fn draw_footer(frame: &mut Frame<'_>, app: &mut App, area: Rect, theme: Theme) {
    let version = application_version_label();
    let (content_area, version_area) = footer_areas(area, &version);
    draw_footer_content(frame, app, content_area, theme);
    if let Some(version_area) = version_area {
        frame.render_widget(
            Paragraph::new(version)
                .alignment(Alignment::Right)
                .style(theme.muted),
            version_area,
        );
    }
}

fn draw_footer_content(frame: &mut Frame<'_>, app: &mut App, area: Rect, theme: Theme) {
    if let Some(flash) = &app.flash {
        frame.render_widget(
            Paragraph::new(truncate_text(&flash.text, area.width)).style(theme.flash(flash.kind)),
            area,
        );
        return;
    }
    if app.help {
        let actions = vec![action_label(
            "[Esc] Close Help",
            HitTarget::Help,
            theme.focus,
        )];
        draw_action_line(frame, &mut app.hit_map, area, &actions, theme);
        return;
    }
    if matches!(app.mode, Mode::TerminalAttached) {
        let actions = vec![
            action_label("[Ctrl+]] Detach", HitTarget::Detach, theme.focus),
            action_label(
                "[ Older ]",
                HitTarget::TerminalHistory(crate::terminal::TerminalHistoryAction::PageUp),
                theme.text,
            ),
            action_label(
                "[ Newer ]",
                HitTarget::TerminalHistory(crate::terminal::TerminalHistoryAction::PageDown),
                theme.text,
            ),
        ];
        draw_action_line(frame, &mut app.hit_map, area, &actions, theme);
        return;
    }
    if !matches!(app.mode, Mode::Navigation) {
        if let Mode::GitHub(modal) = &app.mode {
            let actions = github_footer_actions(modal, theme);
            draw_action_line(frame, &mut app.hit_map, area, &actions, theme);
            return;
        }
        let message = match &app.mode {
            Mode::Modal(_) => "Form · ↑↓ fields · Enter continue · Esc cancel",
            Mode::DirectoryBrowser(_) => {
                "Directories · type filter · ↑↓ select · Enter open/select · Esc cancel"
            }
            Mode::Confirmation(_) => "Confirmation · y confirm · Esc cancel",
            Mode::RemovalBlocked(_) => "Cannot remove Worktree · ↑↓ or wheel scroll · Esc close",
            Mode::CommandPalette(_) => {
                "Commands · type to filter · ↑↓ select · Enter run · Esc close"
            }
            Mode::Settings(_) => "Settings · ↑↓ field · ←→ change · Enter apply · Esc cancel",
            Mode::GitHub(_) => unreachable!("GitHub footer handled above"),
            Mode::TerminalAttached => unreachable!("Terminal attachment handled above"),
            Mode::Navigation => unreachable!("Navigation handled below"),
        };
        frame.render_widget(
            Paragraph::new(truncate_text(message, area.width)).style(theme.muted),
            area,
        );
        return;
    }
    let actions = footer_actions(app, area.width, theme);
    draw_action_line(frame, &mut app.hit_map, area, &actions, theme);
}

fn application_version_label() -> String {
    format_version_label(
        option_env!("PREVIEW_TAG"),
        option_env!("RELEASE_TAG"),
        env!("CARGO_PKG_VERSION"),
    )
}

fn format_version_label(
    preview_tag: Option<&str>,
    release_tag: Option<&str>,
    package_version: &str,
) -> String {
    let version = preview_tag.or(release_tag).unwrap_or(package_version);
    if version.starts_with('v') {
        version.to_owned()
    } else {
        format!("v{version}")
    }
}

fn footer_areas(area: Rect, version: &str) -> (Rect, Option<Rect>) {
    const MIN_CONTENT_WIDTH: u16 = 40;
    const VERSION_GAP: u16 = 1;

    let version_width = text_width(version);
    let reserved = version_width.saturating_add(VERSION_GAP);
    if version_width == 0 || area.width < MIN_CONTENT_WIDTH.saturating_add(reserved) {
        return (area, None);
    }
    let content_width = area.width.saturating_sub(reserved);
    (
        Rect::new(area.x, area.y, content_width, area.height),
        Some(Rect::new(
            area.x
                .saturating_add(content_width)
                .saturating_add(VERSION_GAP),
            area.y,
            version_width,
            area.height,
        )),
    )
}

fn draw_help(frame: &mut Frame<'_>, area: Rect, theme: Theme) {
    let popup = centered(area, 78, 70);
    frame.render_widget(Clear, popup);
    frame.render_widget(Paragraph::new("Tab / Shift+Tab: Navigator ↔ Main\ne / a: Explorer / Sessions navigator view\n↑ ↓ or j k: navigate the selected navigator view\nAlt+← / Alt+→ or drag divider: resize navigator\n1 / 2 / 3: Terminal / Changes / Details\nEnter: explicit primary action\nu: resume eligible session\nn: create in context\nw: create a Worktree for the selected Project\nr: rename metadata\nd: dismiss recoverable attention, stop a Session, or remove a clean Worktree\ng: bounded read-only Git diff\nChanges: ↑/↓, j/k, Page Up/Page Down, or mouse wheel to scroll\nAttached terminal: wheel or Shift+PageUp/PageDown scrolls; Shift+Home oldest; Shift+End latest\nG: browse the selected Project's GitHub Repository\ns: TUI settings\n/: commands and entity search\nShift+A: next attention item\nProject form: Ctrl+B browse; type to filter; Enter open/select; Esc cancel\nCtrl+] while attached: detach\nq: close UI; Sessions continue\nMouse: select rows, tabs, actions, controls, drag divider, and scroll")
        .block(Block::default().title(" Shortcuts ").borders(Borders::ALL).border_style(theme.border)).style(theme.text).wrap(Wrap { trim: false }), popup);
}

#[allow(clippy::too_many_lines)]
fn github_footer_actions(modal: &GitHubModal, theme: Theme) -> Vec<ActionItem> {
    if modal.filter_editing {
        return vec![
            action_label("[Enter] Apply filter", HitTarget::GitHubFilter, theme.focus),
            action_label("[Esc] Cancel", HitTarget::GitHubBack, theme.text),
        ];
    }
    if modal.depth == GitHubDepth::StartWork {
        let mut actions = Vec::new();
        if modal.start_work.as_ref().is_some_and(|form| !form.pending) {
            actions.push(action_label(
                "[Tab] Next field",
                HitTarget::GitHubStartSubmit,
                theme.text,
            ));
            actions.push(action_label(
                "[Enter] Continue / Confirm",
                HitTarget::GitHubStartSubmit,
                theme.focus,
            ));
            actions.push(action_label(
                "[PgUp/PgDn] Context",
                HitTarget::GitHubScroll(5),
                theme.text,
            ));
            actions.push(action_label(
                "[Ctrl+B] Base",
                HitTarget::GitHubCycleBase,
                theme.text,
            ));
            actions.push(action_label(
                "[Ctrl+R] Refresh base",
                HitTarget::GitHubRefreshBase,
                theme.text,
            ));
            actions.push(action_label(
                "[Esc] Back",
                HitTarget::GitHubBack,
                theme.text,
            ));
        }
        return actions;
    }
    if modal.depth == GitHubDepth::Recovery {
        let mut actions = Vec::new();
        if modal
            .recovery
            .as_ref()
            .is_some_and(|recovery| !recovery.pending)
        {
            actions.push(action_label(
                "[s] Retry Session",
                HitTarget::GitHubRetrySession,
                theme.focus,
            ));
            actions.push(action_label(
                "[o] Open Worktree",
                HitTarget::GitHubOpenWorktree,
                theme.text,
            ));
            actions.push(action_label(
                "[d] Delete checkout",
                HitTarget::GitHubRemoveWorktree,
                theme.danger,
            ));
            actions.push(action_label(
                "[Esc] Back",
                HitTarget::GitHubBack,
                theme.text,
            ));
        }
        return actions;
    }
    let mut actions = Vec::new();
    match modal.depth {
        GitHubDepth::List => add_github_list_actions(modal, theme, &mut actions),
        GitHubDepth::Detail => add_github_detail_actions(modal, theme, &mut actions),
        GitHubDepth::StartWork | GitHubDepth::Recovery => unreachable!("handled above"),
    }
    let refresh_available = match modal.depth {
        GitHubDepth::List => !modal.list.is_loading(),
        GitHubDepth::Detail => !modal.detail.is_loading(),
        GitHubDepth::StartWork | GitHubDepth::Recovery => false,
    };
    if refresh_available {
        actions.push(action_label(
            "[r] Refresh",
            HitTarget::GitHubRefresh,
            theme.text,
        ));
    }
    if github_settings_recovery_available(modal) {
        actions.push(action_label(
            "[t] Settings",
            HitTarget::Settings,
            theme.text,
        ));
    }
    actions.push(action_label(
        "[Esc] Back",
        HitTarget::GitHubBack,
        theme.text,
    ));
    actions
}

fn add_github_list_actions(modal: &GitHubModal, theme: Theme, actions: &mut Vec<ActionItem>) {
    if modal
        .list
        .ready()
        .is_some_and(|page| !page.items.is_empty())
    {
        actions.push(action_label(
            "[k] Up",
            HitTarget::GitHubScroll(-1),
            theme.text,
        ));
        actions.push(action_label(
            "[j] Down",
            HitTarget::GitHubScroll(1),
            theme.text,
        ));
        if !modal.list.is_loading() {
            actions.push(action_label(
                "[Enter] Detail",
                HitTarget::GitHubRow(modal.selected_index),
                theme.focus,
            ));
        }
        actions.push(action_label(
            "[o] Browser",
            HitTarget::GitHubOpen,
            theme.focus,
        ));
        if modal.kind == GitHubItemKind::Issue {
            actions.push(github_start_work_action(modal, theme));
            if modal.existing_worktree().is_some() {
                actions.push(action_label(
                    "[a] Another Worktree",
                    HitTarget::GitHubStartAnother,
                    theme.text,
                ));
            }
        }
    }
    if !modal.list.is_loading() && modal.can_go_back() {
        actions.push(action_label(
            "[b] Previous",
            HitTarget::GitHubPrevious,
            theme.text,
        ));
    }
    if !modal.list.is_loading()
        && modal
            .list
            .ready()
            .is_some_and(|page| page.next_cursor.is_some())
    {
        actions.push(action_label("[n] Next", HitTarget::GitHubNext, theme.text));
    }
    actions.push(action_label(
        "[/] Filter",
        HitTarget::GitHubFilter,
        theme.text,
    ));
}

fn add_github_detail_actions(modal: &GitHubModal, theme: Theme, actions: &mut Vec<ActionItem>) {
    if modal.detail.ready().is_some() {
        actions.push(action_label(
            "[k] Up",
            HitTarget::GitHubScroll(-1),
            theme.text,
        ));
        actions.push(action_label(
            "[j] Down",
            HitTarget::GitHubScroll(1),
            theme.text,
        ));
        actions.push(action_label(
            "[o] Browser",
            HitTarget::GitHubOpen,
            theme.focus,
        ));
        if modal.kind == GitHubItemKind::Issue {
            actions.push(github_start_work_action(modal, theme));
            if modal.existing_worktree().is_some() {
                actions.push(action_label(
                    "[a] Another Worktree",
                    HitTarget::GitHubStartAnother,
                    theme.text,
                ));
            }
        }
    }
    if !modal.detail.is_loading()
        && modal
            .detail
            .ready()
            .is_some_and(|detail| detail.comments.next_cursor.is_some())
    {
        actions.push(action_label(
            "[c] More comments",
            HitTarget::GitHubMoreComments,
            theme.text,
        ));
    }
}

fn github_start_work_action(modal: &GitHubModal, theme: Theme) -> ActionItem {
    if modal.existing_worktree().is_some() {
        action_label("[s] Open Work", HitTarget::GitHubStartWork, theme.focus)
    } else {
        action_label("[s] Start Work", HitTarget::GitHubStartWork, theme.focus)
    }
}

fn github_settings_recovery_available(modal: &GitHubModal) -> bool {
    if modal.configuration_error.is_some() {
        return true;
    }
    let failure = match modal.depth {
        GitHubDepth::List => match &modal.list {
            GitHubLoad::Failed(failure) => Some(failure),
            _ => None,
        },
        GitHubDepth::Detail => match &modal.detail {
            GitHubLoad::Failed(failure) => Some(failure),
            _ => None,
        },
        GitHubDepth::StartWork | GitHubDepth::Recovery => None,
    };
    failure.is_some_and(|failure| {
        matches!(
            failure.kind,
            GitHubBrowseFailureKind::CliUnavailable
                | GitHubBrowseFailureKind::Authentication
                | GitHubBrowseFailureKind::Permission
        )
    })
}

fn draw_github(frame: &mut Frame<'_>, modal: &GitHubModal, hits: &mut HitMap, theme: Theme) {
    let area = centered(frame.area(), 94, 88);
    frame.render_widget(Clear, area);
    let title = match modal.kind {
        GitHubItemKind::Issue => " GitHub · Issues ",
        GitHubItemKind::PullRequest => " GitHub · Pull Requests ",
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(theme.focus)
        .style(theme.background);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    if modal.depth == GitHubDepth::StartWork {
        draw_start_work(frame, modal, inner, theme);
        return;
    }
    if modal.depth == GitHubDepth::Recovery {
        draw_start_work_recovery(frame, modal, inner, theme);
        return;
    }
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(2),
            Constraint::Min(1),
        ])
        .split(inner);
    draw_github_tabs(frame, modal, hits, layout[0], theme);
    draw_github_filter(frame, modal, hits, layout[1], theme);
    match modal.depth {
        GitHubDepth::List => draw_github_list(frame, modal, hits, layout[2], theme),
        GitHubDepth::Detail => draw_github_detail(frame, modal, layout[2], theme),
        GitHubDepth::StartWork | GitHubDepth::Recovery => unreachable!("handled above"),
    }
}

#[allow(clippy::too_many_lines)]
fn draw_start_work(frame: &mut Frame<'_>, modal: &GitHubModal, area: Rect, theme: Theme) {
    if modal.start_work_preparing {
        draw_github_message(
            frame,
            area,
            "Preparing Start Work…",
            "The daemon is fetching and validating the exact Issue, selected base, branch, and task-capable Providers.",
            theme.focus,
        );
        return;
    }
    if let Some(failure) = &modal.start_work_error {
        draw_github_message(
            frame,
            area,
            "Start Work preparation failed",
            &format!(
                "{} · [r] Retry latest · [l] Use current checkout · [Esc] Back",
                failure.diagnostic
            ),
            theme.danger,
        );
        return;
    }
    let Some(form) = &modal.start_work else {
        draw_github_message(
            frame,
            area,
            "Start Work unavailable",
            "Press Esc to return.",
            theme.danger,
        );
        return;
    };
    let split = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(21), Constraint::Min(1)])
        .split(area);
    let mut lines = start_work_issue_lines(form, theme);
    let provider = form
        .provider()
        .map_or("No eligible Provider", |provider| provider.display_name());
    lines.extend([
        Line::raw(""),
        Line::raw(format!(
            "Confirmed base: {} @ {}",
            form.preparation.base.head_ref, form.preparation.base.head_commit
        )),
        Line::raw(format!(
            "Base source: {} · Ctrl+B to change · Ctrl+R to refresh",
            form.base_mode.label()
        )),
        start_field_line(
            "Custom local ref",
            &form.base_ref,
            form.focus == StartWorkFocus::BaseRef,
            theme,
        ),
        Line::raw(if form.base_ready {
            "Base preview confirmed"
        } else {
            "Base changed: refresh preview before confirming"
        }),
        start_field_line(
            "Branch",
            &form.branch,
            form.focus == StartWorkFocus::Branch,
            theme,
        ),
        start_field_line(
            "Provider",
            provider,
            form.focus == StartWorkFocus::Provider,
            theme,
        ),
        start_field_line(
            "Session name (optional)",
            if form.session_name.is_empty() {
                "Issue default"
            } else {
                &form.session_name
            },
            form.focus == StartWorkFocus::SessionName,
            theme,
        ),
        start_field_line(
            "Additional instructions",
            if form.instructions.is_empty() {
                "None"
            } else {
                &form.instructions
            },
            form.focus == StartWorkFocus::Instructions,
            theme,
        ),
        start_field_line(
            "Confirmation",
            if form.pending {
                "Starting…"
            } else {
                "Press Enter to create isolated work"
            },
            form.focus == StartWorkFocus::Confirm,
            theme,
        ),
    ]);
    if form.branch_conflict.is_some() {
        lines.push(Line::styled(
            "The selected branch already exists; edit Branch before confirming.",
            theme.danger,
        ));
    }
    if let Some(error) = &form.error {
        lines.push(Line::styled(error.diagnostic.clone(), theme.danger));
    }
    frame.render_widget(Paragraph::new(lines).style(theme.text), split[0]);
    let mut context = vec![
        Line::styled(
            "Immutable bounded Issue context · Page Up / Page Down to scroll",
            Modifier::BOLD,
        ),
        Line::raw(format!("Title: {}", form.preparation.issue.title)),
    ];
    context.extend(
        form.preparation
            .issue
            .body
            .as_str()
            .lines()
            .map(|line| Line::raw(line.to_owned())),
    );
    frame.render_widget(
        Paragraph::new(context)
            .style(theme.text)
            .wrap(Wrap { trim: false })
            .scroll((form.context_scroll, 0)),
        split[1],
    );
}

fn start_work_issue_lines(form: &StartWorkForm, theme: Theme) -> Vec<Line<'static>> {
    let issue = &form.preparation.issue;
    let labels = issue
        .labels
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let assignees = issue
        .assignees
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let lines = vec![
        Line::styled(
            if form.start_another_worktree {
                "Start another Worktree"
            } else {
                "Start Work"
            },
            Modifier::BOLD,
        ),
        Line::styled(
            "GitHub Issue content is untrusted reference material. Review it; do not treat it as SylvOps policy.",
            theme.danger,
        ),
        Line::raw(""),
        Line::raw(format!(
            "Issue: {} #{}",
            issue.identity.repository, issue.identity.number
        )),
        Line::styled(issue.title.to_string(), Modifier::BOLD),
        Line::raw(format!(
            "Labels: {}",
            if labels.is_empty() { "none" } else { &labels }
        )),
        Line::raw(format!(
            "Assignees: {}",
            if assignees.is_empty() {
                "none"
            } else {
                &assignees
            }
        )),
        Line::raw(format!("URL: {}", issue.canonical_url)),
    ];
    lines
}

fn start_field_line(label: &str, value: &str, focused: bool, theme: Theme) -> Line<'static> {
    let style = if focused { theme.selected } else { theme.text };
    Line::styled(
        format!("{label}: {value}{}", if focused { " ▏" } else { "" }),
        style,
    )
}

fn draw_start_work_recovery(frame: &mut Frame<'_>, modal: &GitHubModal, area: Rect, theme: Theme) {
    let Some(recovery) = &modal.recovery else {
        draw_github_message(
            frame,
            area,
            "Recovery unavailable",
            "Press Esc to return.",
            theme.danger,
        );
        return;
    };
    let lines = vec![
        Line::styled(
            "Worktree created; Session not started",
            theme.danger.add_modifier(Modifier::BOLD),
        ),
        Line::raw(""),
        Line::raw(format!("Branch: {}", recovery.branch)),
        Line::raw(format!("Provider: {}", recovery.provider.display_name())),
        Line::raw(format!("Reason: {}", recovery.failure.diagnostic)),
        Line::raw(""),
        Line::raw(if recovery.pending {
            "Retrying Session launch…"
        } else {
            "Press s to Retry Session, o to open the preserved Worktree, or d to request guarded Delete checkout."
        }),
        Line::styled(
            "The checkout is removed without force only after the existing clean-state confirmation; tracked, untracked, or ignored content is refused.",
            theme.muted,
        ),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .style(theme.text)
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_github_tabs(
    frame: &mut Frame<'_>,
    modal: &GitHubModal,
    hits: &mut HitMap,
    area: Rect,
    theme: Theme,
) {
    let issues = if modal.kind == GitHubItemKind::Issue {
        theme.selected
    } else {
        theme.text
    };
    let pulls = if modal.kind == GitHubItemKind::PullRequest {
        theme.selected
    } else {
        theme.text
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" [i] Issues ", issues),
            Span::raw("  "),
            Span::styled(" [p] Pull Requests ", pulls),
        ])),
        area,
    );
    hits.push(
        Rect::new(area.x, area.y, 12.min(area.width), 1),
        HitTarget::GitHubIssues,
    );
    if area.width > 14 {
        hits.push(
            Rect::new(
                area.x.saturating_add(14),
                area.y,
                20.min(area.width - 14),
                1,
            ),
            HitTarget::GitHubPullRequests,
        );
    }
}

fn draw_github_filter(
    frame: &mut Frame<'_>,
    modal: &GitHubModal,
    hits: &mut HitMap,
    area: Rect,
    theme: Theme,
) {
    let cursor = if modal.filter_editing { "▏" } else { "" };
    let value = if modal.filter_input.is_empty() {
        "open items (type / to filter)".to_owned()
    } else {
        modal.filter_input.clone()
    };
    let hint = modal.filter_error.as_deref().unwrap_or(match modal.kind {
        GitHubItemKind::Issue => "text · state: · author: · assignee: · label:",
        GitHubItemKind::PullRequest => {
            "text · state: · author: · label: · draft: · review: · check:"
        }
    });
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled("Filter: ", Modifier::BOLD),
                Span::styled(
                    format!("{value}{cursor}"),
                    if modal.filter_editing {
                        theme.focus
                    } else {
                        theme.text
                    },
                ),
            ]),
            Line::styled(
                hint.to_owned(),
                if modal.filter_error.is_some() {
                    theme.danger
                } else {
                    theme.muted
                },
            ),
        ]),
        area,
    );
    hits.push(area, HitTarget::GitHubFilter);
}

fn draw_github_list(
    frame: &mut Frame<'_>,
    modal: &GitHubModal,
    hits: &mut HitMap,
    area: Rect,
    theme: Theme,
) {
    if let Some(error) = &modal.configuration_error {
        draw_github_message(
            frame,
            area,
            "GitHub Repository setup is required",
            error,
            theme.danger,
        );
        return;
    }
    let area = if let Some(error) = &modal.browser_error {
        let split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(2), Constraint::Min(1)])
            .split(area);
        frame.render_widget(
            Paragraph::new(format!("Browser action failed: {error}"))
                .style(theme.danger)
                .wrap(Wrap { trim: false }),
            split[0],
        );
        split[1]
    } else if modal.browser_pending {
        let split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(1)])
            .split(area);
        frame.render_widget(
            Paragraph::new("Opening the validated browser target…").style(theme.focus),
            split[0],
        );
        split[1]
    } else {
        area
    };
    match &modal.list {
        GitHubLoad::Idle => {
            draw_github_message(
                frame,
                area,
                "No GitHub request",
                "Press r to load items.",
                theme.muted,
            );
        }
        GitHubLoad::Loading(None) => {
            draw_github_message(
                frame,
                area,
                "Loading GitHub items…",
                "The request is bounded and cancellable by closing the modal.",
                theme.focus,
            );
        }
        GitHubLoad::Failed(failure) => draw_github_failure(frame, area, failure, theme),
        GitHubLoad::Ready(page) | GitHubLoad::Loading(Some(page)) => {
            draw_github_loaded_list(frame, modal, page, hits, area, theme);
        }
    }
}

fn draw_github_loaded_list(
    frame: &mut Frame<'_>,
    modal: &GitHubModal,
    page: &sylvops_core::github::GitHubListPage,
    hits: &mut HitMap,
    area: Rect,
    theme: Theme,
) {
    if page.items.is_empty() {
        let detail = if modal.list.is_loading() {
            "Refreshing the current bounded page…"
        } else if modal.filter_input.is_empty() {
            "No open items were returned. Press r to refresh."
        } else {
            "No items match the current filters. Press / to edit them."
        };
        let title = if modal.list.is_loading() {
            "Refreshing GitHub items…"
        } else {
            "No GitHub items"
        };
        draw_github_message(frame, area, title, detail, theme.muted);
        return;
    }
    let rows_visible = usize::from(area.height / 2).max(1);
    let start = modal
        .selected_index
        .saturating_sub(rows_visible.saturating_sub(1))
        .min(page.items.len().saturating_sub(rows_visible));
    let items = page
        .items
        .iter()
        .enumerate()
        .skip(start)
        .take(rows_visible)
        .map(|(index, item)| {
            github_list_item(
                item,
                index,
                modal.selected_index,
                modal.status_for(&item.identity),
                modal.remote_status_for(&item.identity),
                theme,
            )
        })
        .collect::<Vec<_>>();
    frame.render_widget(List::new(items), area);
    for (visible, index) in (start..page.items.len()).take(rows_visible).enumerate() {
        hits.push(
            Rect::new(
                area.x,
                area.y
                    .saturating_add(u16::try_from(visible * 2).unwrap_or(u16::MAX)),
                area.width,
                2.min(area.height),
            ),
            HitTarget::GitHubRow(index),
        );
    }
    if modal.list.is_loading() && area.height > 0 {
        frame.render_widget(
            Paragraph::new("Refreshing…")
                .style(theme.focus)
                .alignment(Alignment::Right),
            Rect::new(area.x, area.y, area.width, 1),
        );
    }
}

fn github_list_item<'a>(
    item: &sylvops_core::github::GitHubItemSummary,
    index: usize,
    selected_index: usize,
    work_status: sylvops_core::github::IssueWorkStatus,
    remote_status: Option<crate::github_modal::IssueRemoteStatus>,
    theme: Theme,
) -> ListItem<'a> {
    let mut metadata = github_list_metadata(item);
    if item.identity.kind == GitHubItemKind::Issue {
        let _ = write!(metadata, " · {}", work_status.display_name());
        if let Some(
            remote_status @ (crate::github_modal::IssueRemoteStatus::Closed
            | crate::github_modal::IssueRemoteStatus::Unavailable
            | crate::github_modal::IssueRemoteStatus::RepositoryChanged),
        ) = remote_status
        {
            let _ = write!(metadata, " · {}", remote_status.display_name());
        }
    }
    if let Some(pull) = &item.pull_request {
        let _ = write!(metadata, " · {}", github_pull_request_metadata(pull));
    }
    let selected = index == selected_index;
    let style = if selected { theme.selected } else { theme.text };
    ListItem::new(vec![
        Line::styled(format!("#{} {}", item.identity.number, item.title), style),
        Line::styled(
            metadata,
            if selected {
                theme.selected
            } else {
                theme.muted
            },
        ),
    ])
}

fn github_list_metadata(item: &sylvops_core::github::GitHubItemSummary) -> String {
    let state = github_item_state(item.state);
    let author = item
        .author
        .as_ref()
        .map_or("unknown", |value| value.as_str());
    let mut metadata = format!("{state} · by {author} · updated {}", item.updated_at);
    if !item.labels.is_empty() {
        metadata.push_str(" · labels: ");
        metadata.push_str(
            &item
                .labels
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", "),
        );
    }
    if !item.assignees.is_empty() {
        metadata.push_str(" · assigned: ");
        metadata.push_str(
            &item
                .assignees
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", "),
        );
    }
    metadata
}

fn draw_github_detail(frame: &mut Frame<'_>, modal: &GitHubModal, area: Rect, theme: Theme) {
    if modal.selected_identity().is_some_and(|identity| {
        modal.remote_status_for(&identity)
            == Some(crate::github_modal::IssueRemoteStatus::RepositoryChanged)
    }) {
        draw_github_message(
            frame,
            area,
            "Repository changed",
            "The historical Work Source is preserved, but this Project now uses a different GitHub Repository.",
            theme.danger,
        );
        return;
    }
    if let Some(error) = &modal.configuration_error {
        draw_github_message(
            frame,
            area,
            "GitHub Repository setup is required",
            error,
            theme.danger,
        );
        return;
    }
    match &modal.detail {
        GitHubLoad::Idle => draw_github_message(
            frame,
            area,
            "No item selected",
            "Press Esc to return to the list.",
            theme.muted,
        ),
        GitHubLoad::Loading(None) => draw_github_message(
            frame,
            area,
            "Loading GitHub item…",
            "Fetching bounded details and comments.",
            theme.focus,
        ),
        GitHubLoad::Failed(failure) => draw_github_failure(frame, area, failure, theme),
        GitHubLoad::Ready(detail) | GitHubLoad::Loading(Some(detail)) => {
            frame.render_widget(
                Paragraph::new(Text::from(github_detail_lines(detail, modal, theme)))
                    .style(theme.text)
                    .wrap(Wrap { trim: false })
                    .scroll((modal.detail_scroll, 0)),
                area,
            );
        }
    }
}

fn github_detail_lines(
    detail: &sylvops_core::github::GitHubItemDetail,
    modal: &GitHubModal,
    theme: Theme,
) -> Vec<Line<'static>> {
    let mut lines = github_detail_header_lines(&detail.summary, theme);
    if detail.summary.identity.kind == GitHubItemKind::Issue {
        lines.push(Line::styled(
            format!(
                "Local work: {}",
                modal.status_for(&detail.summary.identity).display_name()
            ),
            theme.focus,
        ));
        if let Some(remote_status) = modal.remote_status_for(&detail.summary.identity) {
            lines.push(Line::styled(
                format!("Remote: {}", remote_status.display_name()),
                theme.muted,
            ));
        }
    }
    if modal.detail.is_loading() {
        lines.push(Line::styled("Refreshing GitHub detail…", theme.focus));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled("Description", Modifier::BOLD));
    lines.extend(
        detail
            .body
            .as_str()
            .lines()
            .map(|line| Line::raw(line.to_owned())),
    );
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        format!("Comments ({})", detail.comments.comments.len()),
        Modifier::BOLD,
    ));
    lines.extend(github_comment_lines(&detail.comments, theme));
    if detail.comments.next_cursor.is_some() {
        lines.push(Line::styled(
            "More comments are available · press c",
            theme.focus,
        ));
    }
    if let Some(error) = &modal.browser_error {
        lines.push(Line::styled(error.clone(), theme.danger));
    } else if modal.browser_pending {
        lines.push(Line::styled(
            "Opening the validated browser target…",
            theme.focus,
        ));
    }
    lines
}

fn github_detail_header_lines(
    item: &sylvops_core::github::GitHubItemSummary,
    theme: Theme,
) -> Vec<Line<'static>> {
    let author = item
        .author
        .as_ref()
        .map_or("unknown", |value| value.as_str());
    let mut lines = vec![
        Line::styled(
            format!(
                "#{} · {}",
                item.identity.number,
                github_item_state(item.state)
            ),
            theme.focus,
        ),
        Line::styled(item.title.to_string(), Modifier::BOLD),
        Line::styled(
            format!(
                "Author: {author} · Created {} · Updated {}",
                item.created_at, item.updated_at
            ),
            theme.muted,
        ),
    ];
    if let Some(pull) = &item.pull_request {
        lines.push(Line::styled(
            github_pull_request_metadata(pull),
            theme.muted,
        ));
    }
    if !item.labels.is_empty() || !item.assignees.is_empty() {
        let labels = item
            .labels
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        let assignees = item
            .assignees
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(Line::styled(
            format!(
                "Labels: {} · Assignees: {}",
                if labels.is_empty() { "none" } else { &labels },
                if assignees.is_empty() {
                    "none"
                } else {
                    &assignees
                }
            ),
            theme.muted,
        ));
    }
    lines
}

fn github_pull_request_metadata(pull: &sylvops_core::github::GitHubPullRequestSummary) -> String {
    format!(
        "{} → {} · {} · review {} · checks {} ({}) · {} files (+{} −{})",
        pull.head_branch,
        pull.base_branch,
        if pull.draft { "draft" } else { "ready" },
        github_review_state(pull.review),
        github_check_state(pull.checks.state),
        pull.checks.total,
        pull.changed_files.total,
        pull.changed_files.additions,
        pull.changed_files.deletions,
    )
}

fn github_comment_lines(
    comments: &sylvops_core::github::GitHubCommentPage,
    theme: Theme,
) -> Vec<Line<'static>> {
    if comments.comments.is_empty() {
        return vec![Line::styled("No comments on this page.", theme.muted)];
    }
    let mut lines = Vec::new();
    for comment in &comments.comments {
        let author = comment
            .author
            .as_ref()
            .map_or("unknown", |value| value.as_str());
        lines.push(Line::styled(
            format!("{author} · {}", comment.created_at),
            theme.focus,
        ));
        lines.extend(
            comment
                .body
                .as_str()
                .lines()
                .map(|line| Line::raw(line.to_owned())),
        );
        lines.push(Line::raw(""));
    }
    lines
}

fn draw_github_failure(
    frame: &mut Frame<'_>,
    area: Rect,
    failure: &GitHubBrowseFailure,
    theme: Theme,
) {
    let title = match failure.kind {
        GitHubBrowseFailureKind::CliUnavailable => "GitHub CLI unavailable",
        GitHubBrowseFailureKind::Authentication => "GitHub authentication required",
        GitHubBrowseFailureKind::Permission => "GitHub Repository access denied",
        GitHubBrowseFailureKind::RateLimited => "GitHub rate limit reached",
        GitHubBrowseFailureKind::Network => "GitHub is offline",
        GitHubBrowseFailureKind::TimedOut => "GitHub request timed out",
        GitHubBrowseFailureKind::MalformedResponse => "GitHub returned malformed data",
        GitHubBrowseFailureKind::OversizedResponse => "GitHub response exceeded safety limits",
        GitHubBrowseFailureKind::UnavailableItem => "This GitHub item is unavailable",
    };
    draw_github_message(frame, area, title, &failure.diagnostic, theme.danger);
}

fn draw_github_message(frame: &mut Frame<'_>, area: Rect, title: &str, detail: &str, style: Style) {
    frame.render_widget(
        Paragraph::new(vec![
            Line::styled(title.to_owned(), style.add_modifier(Modifier::BOLD)),
            Line::raw(""),
            Line::styled(detail.to_owned(), style),
        ])
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: false }),
        area,
    );
}

const fn github_item_state(state: GitHubItemState) -> &'static str {
    match state {
        GitHubItemState::Open => "open",
        GitHubItemState::Closed => "closed",
        GitHubItemState::Merged => "merged",
    }
}

const fn github_review_state(state: GitHubReviewState) -> &'static str {
    match state {
        GitHubReviewState::Approved => "approved",
        GitHubReviewState::ChangesRequested => "changes requested",
        GitHubReviewState::ReviewRequired => "review required",
        GitHubReviewState::None => "none",
    }
}

const fn github_check_state(state: GitHubCheckState) -> &'static str {
    match state {
        GitHubCheckState::Success => "passing",
        GitHubCheckState::Failure => "failing",
        GitHubCheckState::Pending => "pending",
        GitHubCheckState::Neutral => "neutral",
        GitHubCheckState::Skipped => "skipped",
        GitHubCheckState::None => "none",
    }
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
    if form.is_worktree() {
        lines.push(Line::from("Base · ←/→ to choose"));
        row = row.saturating_add(1);
        for mode in sylvops_core::ui_forms::WorktreeBaseMode::ALL {
            lines.push(Line::styled(
                format!(
                    "{} {}",
                    if form.worktree_base_mode == mode {
                        "●"
                    } else {
                        "○"
                    },
                    mode.label()
                ),
                theme.focus,
            ));
            hits.push(
                Rect::new(area.x + 1, row, area.width.saturating_sub(2), 1),
                HitTarget::ModalWorktreeBase(mode),
            );
            row = row.saturating_add(1);
        }
        if form.pending {
            lines.push(Line::from("Fetching base / creating Worktree…"));
            row = row.saturating_add(1);
        }
    }
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
    if matches!(form.kind, FormKind::RegisterProject(_)) {
        lines.push(Line::from(Span::styled(
            "[ Browse directories · Ctrl+B ]",
            theme.focus,
        )));
        hits.push(
            Rect::new(
                area.x.saturating_add(1),
                row,
                area.width.saturating_sub(2),
                1,
            ),
            HitTarget::ModalBrowse,
        );
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

fn draw_directory_browser(
    frame: &mut Frame<'_>,
    browser: &DirectoryBrowser,
    hits: &mut HitMap,
    theme: Theme,
) {
    let area = centered(frame.area(), 86, 82);
    frame.render_widget(Clear, area);
    let inner = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(2),
            Constraint::Min(4),
            Constraint::Length(2),
        ])
        .margin(1)
        .split(area);
    frame.render_widget(
        Paragraph::new(format!("Current\n{}", display_path(&browser.current))).style(theme.text),
        inner[0],
    );
    frame.render_widget(
        Paragraph::new(directory_browser_filter(browser)).style(theme.muted),
        inner[1],
    );

    let entries = browser.visible_entries();
    let visible_rows = usize::from(inner[2].height);
    let feedback = directory_browser_feedback(browser, theme);
    let entry_rows = visible_rows.saturating_sub(feedback.len()).max(1);
    let offset = browser
        .selected
        .saturating_add(1)
        .saturating_sub(entry_rows);
    let lines = entries
        .iter()
        .enumerate()
        .skip(offset)
        .take(entry_rows)
        .map(|(index, entry)| {
            hits.push(
                Rect::new(
                    inner[2].x,
                    inner[2]
                        .y
                        .saturating_add(u16::try_from(index - offset).unwrap_or(u16::MAX)),
                    inner[2].width,
                    1,
                ),
                HitTarget::DirectoryBrowserRow(index),
            );
            Line::from(Span::styled(
                format!(
                    "{} {}",
                    if index == browser.selected {
                        "›"
                    } else {
                        " "
                    },
                    entry.label()
                ),
                if index == browser.selected {
                    theme.focus
                } else {
                    theme.text
                },
            ))
        })
        .collect::<Vec<_>>();
    let mut list = lines;
    list.extend(feedback);
    frame.render_widget(Paragraph::new(list).wrap(Wrap { trim: false }), inner[2]);

    let select_style = if browser.can_select_current() {
        theme.success
    } else {
        theme.muted
    };
    frame.render_widget(
        Paragraph::new(vec![Line::from(vec![
            Span::styled("[ Select current ]", select_style),
            Span::raw("   "),
            Span::styled("[ Cancel ]", theme.muted),
        ])]),
        inner[3],
    );
    if browser.can_select_current() {
        hits.push(
            Rect::new(inner[3].x, inner[3].y, 18, 1),
            HitTarget::DirectoryBrowserSelect,
        );
    }
    hits.push(
        Rect::new(inner[3].x.saturating_add(21), inner[3].y, 12, 1),
        HitTarget::DirectoryBrowserCancel,
    );
    frame.render_widget(
        Block::default()
            .title(" Directory browser · h hidden · Ctrl+R refresh · ↑↓ move · Enter open/select · Esc cancel ")
            .borders(Borders::ALL)
            .border_style(theme.border),
        area,
    );
}

fn directory_browser_feedback(browser: &DirectoryBrowser, theme: Theme) -> Vec<Line<'static>> {
    let mut feedback = Vec::new();
    if let Some(notice) = browser.scan_notice() {
        feedback.push(Line::from(Span::styled(notice, theme.muted)));
    }
    if let Some(notice) = browser.truncation_notice() {
        feedback.push(Line::from(Span::styled(notice, theme.muted)));
    }
    if let Some(error) = &browser.error {
        feedback.push(Line::from(Span::styled(error.clone(), theme.danger)));
    }
    feedback
}

fn directory_browser_filter(browser: &DirectoryBrowser) -> String {
    let hidden = if browser.show_hidden {
        "shown"
    } else {
        "hidden"
    };
    if browser.filter.is_empty() {
        format!("Filter: (type a directory name) · hidden: {hidden}")
    } else {
        format!("Filter: {} · hidden: {hidden}", browser.filter)
    }
}

fn draw_removal_notice(
    frame: &mut Frame<'_>,
    notice: &mut crate::removal_notice::RemovalNotice,
    hits: &mut HitMap,
    theme: Theme,
) {
    let area = centered(frame.area(), 92, 85);
    frame.render_widget(Clear, area);
    let block = Block::default()
        .title(" Cannot remove this Worktree ")
        .borders(Borders::ALL)
        .border_style(theme.attention)
        .style(theme.text);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let body = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner.height.saturating_sub(1),
    );
    let mut wrapped = Vec::new();
    for line in &notice.lines {
        let mut row = String::new();
        let mut width = 0_u16;
        for character in line.chars() {
            let character_width = text_width(&character.to_string());
            if width.saturating_add(character_width) > body.width && !row.is_empty() {
                wrapped.push(Line::from(std::mem::take(&mut row)));
                width = 0;
            }
            row.push(character);
            width = width.saturating_add(character_width);
        }
        wrapped.push(Line::from(row));
    }
    notice.max_scroll = u16::try_from(wrapped.len())
        .unwrap_or(u16::MAX)
        .saturating_sub(body.height);
    notice.scroll = notice.scroll.min(notice.max_scroll);
    frame.render_widget(
        Paragraph::new(wrapped)
            .scroll((notice.scroll, 0))
            .style(theme.text),
        body,
    );
    let close = Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1);
    frame.render_widget(
        Paragraph::new("[ Close · Esc / Enter ]").style(theme.focus),
        close,
    );
    hits.push(close, HitTarget::DismissNotice);
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
    let area = centered(frame.area(), 92, 94);
    frame.render_widget(Clear, area);
    let inner = area.inner(ratatui::layout::Margin {
        horizontal: 2,
        vertical: 1,
    });
    let mut row = inner.y;
    let mut lines = Vec::new();
    lines.push(Line::from(Span::styled(
        if settings.focus == SettingsFocus::GitHub {
            "› GitHub integration"
        } else {
            "  GitHub integration"
        },
        Modifier::BOLD,
    )));
    row = row.saturating_add(1);
    if let Some(status) = &app.github_status {
        for connection in &status.connections {
            lines.push(Line::from(format!(
                "  {} · {}",
                connection.host,
                connection.state.display_name()
            )));
            row = row.saturating_add(1);
        }
        if app.selected_project_id.is_some() {
            if status.remotes.is_empty() {
                lines.push(Line::from(Span::styled(
                    "  No validated GitHub remotes in this Project.",
                    theme.muted,
                )));
                row = row.saturating_add(1);
            }
            for (index, remote) in status.remotes.iter().enumerate() {
                let associated = status.association.as_ref().is_some_and(|association| {
                    association.remote_name == remote.name
                        && association.repository == remote.repository
                });
                let selected = settings.github_remote_index == index;
                let label = format!(
                    "  {} {} {}",
                    if selected { "›" } else { " " },
                    if associated { "●" } else { "○" },
                    remote
                );
                lines.push(Line::from(Span::styled(
                    label.chars().take(76).collect::<String>(),
                    if associated {
                        theme.selected
                    } else {
                        theme.text
                    },
                )));
                app.hit_map.push(
                    Rect::new(inner.x, row, inner.width, 1),
                    HitTarget::SettingsGithubRemote(index),
                );
                row = row.saturating_add(1);
            }
        } else {
            lines.push(Line::from(Span::styled(
                "  Select a Project to choose its GitHub remote.",
                theme.muted,
            )));
            row = row.saturating_add(1);
        }
    } else {
        lines.push(Line::from(Span::styled(
            "  GitHub status has not been checked.",
            theme.muted,
        )));
        row = row.saturating_add(1);
    }
    lines.push(Line::from("  [r Refresh]  [c Clear association]"));
    app.hit_map.push(
        Rect::new(inner.x.saturating_add(2), row, 11, 1),
        HitTarget::SettingsGithubRefresh,
    );
    app.hit_map.push(
        Rect::new(inner.x.saturating_add(15), row, 21, 1),
        HitTarget::SettingsGithubClear,
    );
    row = row.saturating_add(1);
    lines.push(Line::from(""));
    row = row.saturating_add(1);
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
    if app.density == TuiDensity::Comfortable {
        lines.push(Line::from(""));
        row = row.saturating_add(1);
    }
    lines.push(Line::from(Span::styled(
        if settings.focus == SettingsFocus::NativeNotifications {
            "› Native and in-client notifications"
        } else {
            "  Native and in-client notifications"
        },
        Modifier::BOLD,
    )));
    row = row.saturating_add(1);
    for (enabled, label) in [(true, "Enabled"), (false, "Disabled")] {
        let selected = app.notification_preferences.native_notifications_enabled == enabled;
        lines.push(Line::from(Span::styled(
            format!("  {} {label}", if selected { "●" } else { "○" }),
            if selected { theme.selected } else { theme.text },
        )));
        app.hit_map.push(
            Rect::new(inner.x, row, inner.width, 1),
            HitTarget::SettingsNativeNotifications(enabled),
        );
        row = row.saturating_add(1);
    }
    lines.push(Line::from(Span::styled(
        if settings.focus == SettingsFocus::Sound {
            "› Attention sound"
        } else {
            "  Attention sound"
        },
        Modifier::BOLD,
    )));
    row = row.saturating_add(1);
    for (enabled, label) in [(true, "Enabled"), (false, "Disabled")] {
        let selected = app.notification_preferences.sound_enabled == enabled;
        lines.push(Line::from(Span::styled(
            format!("  {} {label}", if selected { "●" } else { "○" }),
            if selected { theme.selected } else { theme.text },
        )));
        app.hit_map.push(
            Rect::new(inner.x, row, inner.width, 1),
            HitTarget::SettingsSound(enabled),
        );
        row = row.saturating_add(1);
    }
    lines.push(Line::from(Span::styled(
        if settings.focus == SettingsFocus::TestNotification {
            "› [ Test notification ]"
        } else {
            "  [ Test notification ]"
        },
        theme.focus,
    )));
    app.hit_map.push(
        Rect::new(inner.x, row, inner.width, 1),
        HitTarget::SettingsTestNotification,
    );
    row = row.saturating_add(1);
    lines.push(Line::from(Span::styled(
        "Preview is local until Apply. Test uses saved shared notification settings.",
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
                    .title(" Settings · ↑/↓ section · ←/→ choice · Enter connect/apply ")
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

pub(crate) fn status_symbol(status: SessionStatus, animation_frame: usize) -> &'static str {
    const SPINNER: [&str; 4] = ["◐", "◓", "◑", "◒"];
    match status.lifecycle {
        SessionLifecycle::Fresh => "○",
        SessionLifecycle::Starting => SPINNER[animation_frame % SPINNER.len()],
        SessionLifecycle::Running => match status.activity {
            ProviderActivity::AwaitingInput | ProviderActivity::RecoverableFailure => "!",
            ProviderActivity::Completed => "◆",
            ProviderActivity::None | ProviderActivity::Working | ProviderActivity::Interrupted => {
                SPINNER[animation_frame % SPINNER.len()]
            }
        },
        SessionLifecycle::Exited => "✓",
        SessionLifecycle::Failed | SessionLifecycle::Terminated => "×",
        SessionLifecycle::Disconnected => "—",
    }
}

pub(crate) fn status_label(status: SessionStatus) -> &'static str {
    match status.lifecycle {
        SessionLifecycle::Fresh => "Fresh",
        SessionLifecycle::Starting => "Starting",
        SessionLifecycle::Running => match status.activity {
            ProviderActivity::AwaitingInput => "Needs input",
            ProviderActivity::Completed => "Completed",
            ProviderActivity::Interrupted => "Interrupted",
            ProviderActivity::RecoverableFailure => "Recoverable failure",
            ProviderActivity::None | ProviderActivity::Working => "Running",
        },
        SessionLifecycle::Exited => "Finished",
        SessionLifecycle::Failed => "Failed",
        SessionLifecycle::Terminated => "Stopped",
        SessionLifecycle::Disconnected => "Disconnected",
    }
}

fn text_width(text: &str) -> u16 {
    u16::try_from(Span::raw(text).width()).unwrap_or(u16::MAX)
}

fn truncate_text(text: &str, width: u16) -> String {
    if text_width(text) <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let budget = width.saturating_sub(text_width("…"));
    let mut truncated = String::new();
    let mut used = 0_u16;
    for character in text.chars() {
        let character_width = text_width(&character.to_string());
        if used.saturating_add(character_width) > budget {
            break;
        }
        truncated.push(character);
        used = used.saturating_add(character_width);
    }
    truncated.push('…');
    truncated
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_redraw_preserves_columns_after_unicode_symbols() {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 2)).unwrap();
        let mut parser = vt100::Parser::new(2, 40, 0);
        for prefix in [
            "plain ",
            "› ",
            "界 ",
            "🙂 ",
            "⚠️ ",
            "❤️ ",
            "👩‍💻 ",
            "🇲🇰 ",
            "1️⃣ ",
            "plain ",
        ] {
            parser.process(format!("\r\x1b[2K{prefix}TAIL").as_bytes());
            terminal
                .draw(|frame| {
                    frame.render_widget(
                        Paragraph::new(terminal_text(
                            parser.screen(),
                            Theme::resolve(TuiTheme::Nord).text,
                        )),
                        frame.area(),
                    );
                })
                .unwrap();
            for symbol in ["T", "A", "I", "L"] {
                let expected = (0..40)
                    .find(|column| {
                        parser
                            .screen()
                            .cell(0, *column)
                            .is_some_and(|cell| cell.contents() == symbol)
                    })
                    .unwrap();
                let actual = terminal.backend().buffer()[(expected, 0)].symbol();
                assert_eq!(
                    actual, symbol,
                    "{prefix:?}: {symbol} moved during rendering"
                );
            }
        }
    }

    #[test]
    fn startup_wordmark_fits_large_and_small_terminals() {
        for (width, height) in [(80, 24), (50, 9), (30, 8), (7, 3), (1, 1)] {
            let backend = ratatui::backend::TestBackend::new(width, height);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| draw_startup(frame, TuiTheme::default(), false))
                .unwrap();
            let rendered = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>();
            if width >= 50 {
                assert!(rendered.contains("/ ___|"));
                assert!(rendered.contains("Loading workspace…"));
            } else if width >= 7 {
                assert!(rendered.contains("SYLVOPS"));
            }
        }
    }

    #[test]
    fn tui_diff_lines_use_semantic_theme_styles_and_fill_short_rows() {
        let theme = Theme::resolve(TuiTheme::Nord);
        let lines = styled_diff_lines(
            "diff --git a/a.rs b/a.rs\n@@ -1 +1 @@\n-old\n+new\n context",
            32,
            theme,
        );

        assert_eq!(lines[0].style, theme.diff_file_header);
        assert_eq!(lines[1].style, theme.diff_hunk_header);
        assert_eq!(lines[2].style, theme.diff_deletion);
        assert_eq!(lines[3].style, theme.diff_addition);
        assert_eq!(lines[4].style, theme.text);
        assert!(lines.iter().all(|line| line.width() >= 32));
    }
}
