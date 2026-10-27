//! Nonblocking local-change review and terminal-specific input/rendering.
use crate::{
    app::{App, FocusZone, HitTarget, Mode},
    theme::Theme,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::Rect,
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};
use std::time::{Duration, Instant};
use sylvops_core::{
    changes::{ChangeGroup, ReviewState},
    diff::DiffLineKind,
    ids::WorktreeId,
    protocol::{ClientRequest, DaemonResponse},
    ui::TuiContentView,
};
use sylvops_daemon::client::DaemonClient;

type Reply = Result<
    (
        DaemonResponse,
        Option<Vec<Vec<sylvops_core::changes::SyntaxToken>>>,
    ),
    sylvops_daemon::DaemonError,
>;

#[derive(Debug)]
struct Pending {
    checkout: WorktreeId,
    generation: u64,
    file: bool,
    reply: tokio::sync::oneshot::Receiver<Reply>,
}

#[derive(Debug, Default)]
pub(crate) struct Review {
    pub state: ReviewState,
    pending: Option<Pending>,
    last_refresh: Option<Instant>,
    pub refresh: bool,
    load_file: bool,
    pub editing: Option<bool>,
    updated_at: Option<Instant>,
    discard_pending: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Action {
    Move(isize),
    Select(usize),
    Open,
    Back,
    Pane,
    Pause,
    Refresh,
    Filter,
    Search,
    Character(char),
    Erase,
    Finish,
    Jump(bool, bool),
    Horizontal(isize),
    Group(ChangeGroup),
    Wheel(isize, bool),
}

pub(crate) fn key_action(review: &Review, key: &KeyEvent) -> Option<Action> {
    if review.editing.is_some() {
        return match key.code {
            KeyCode::Backspace => Some(Action::Erase),
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(Action::Character(c))
            }
            _ => Some(Action::Finish),
        };
    }
    match key.code {
        KeyCode::Tab | KeyCode::BackTab => Some(Action::Pane),
        KeyCode::Enter => Some(Action::Open),
        KeyCode::Esc => Some(Action::Back),
        KeyCode::Char('p') => Some(Action::Pause),
        KeyCode::Char('R') => Some(Action::Refresh),
        KeyCode::Char('f') => Some(Action::Filter),
        KeyCode::Char('F') => Some(Action::Search),
        KeyCode::Char(']') => Some(Action::Jump(true, false)),
        KeyCode::Char('[') => Some(Action::Jump(false, false)),
        KeyCode::Char('n') => Some(Action::Jump(true, true)),
        KeyCode::Char('N') => Some(Action::Jump(false, true)),
        KeyCode::Left if !review.state.file_focus => Some(Action::Horizontal(-4)),
        KeyCode::Right if !review.state.file_focus => Some(Action::Horizontal(4)),
        _ => None,
    }
}

#[allow(clippy::too_many_lines)] // Pure review action reducer.
pub(crate) fn update(app: &mut App, action: Action) {
    app.focus = FocusZone::Main;
    let review = &mut app.review;
    match action {
        Action::Wheel(delta, files) => {
            review.state.file_focus = files;
            if files {
                review.state.move_file(delta);
                review.load_file = true;
            } else {
                review.state.scroll = review.state.scroll.saturating_add_signed(delta).min(
                    review
                        .state
                        .file
                        .as_ref()
                        .map_or(0, |f| f.lines.len().saturating_sub(1)),
                );
            }
        }
        Action::Move(delta) => {
            if review.state.file_focus {
                review.state.move_file(delta);
                review.load_file = true;
            } else {
                review.state.scroll = review.state.scroll.saturating_add_signed(delta).min(
                    review
                        .state
                        .file
                        .as_ref()
                        .map_or(0, |f| f.lines.len().saturating_sub(1)),
                );
            }
        }
        Action::Select(index) => {
            let key = review.state.visible().get(index).map(|f| f.key.clone());
            review.state.select(key);
            review.state.detail = true;
            review.state.file_focus = false;
            review.load_file = true;
            app.focus = FocusZone::Main;
        }
        Action::Open => {
            review.state.detail = true;
            review.state.file_focus = false;
            review.load_file = true;
        }
        Action::Back => {
            if review.state.detail {
                review.state.detail = false;
                review.state.file_focus = true;
            } else {
                app.focus = FocusZone::Explorer;
            }
        }
        Action::Pane => {
            if review.state.file_focus {
                review.state.file_focus = false;
                review.state.detail = true;
            } else {
                review.state.file_focus = true;
                review.state.detail = false;
            }
        }
        Action::Pause => {
            review.state.paused = !review.state.paused;
            if review.state.paused {
                review.discard_pending = review.pending.is_some();
                review.load_file = false;
            }
            if !review.state.paused {
                review.discard_pending = false;
                review.refresh = true;
            }
        }
        Action::Refresh => {
            review.discard_pending = false;
            review.refresh = true;
        }
        Action::Filter => review.editing = Some(false),
        Action::Search => review.editing = Some(true),
        Action::Character(c) => {
            if let Some(search) = review.editing {
                let value = if search {
                    &mut review.state.search
                } else {
                    &mut review.state.filter
                };
                if value.len() < 256 && !c.is_control() {
                    value.push(c);
                }
            }
        }
        Action::Erase => {
            if let Some(search) = review.editing {
                if search {
                    review.state.search.pop();
                } else {
                    review.state.filter.pop();
                }
            }
        }
        Action::Finish => {
            if review.editing == Some(false) {
                let key = review.state.visible().first().map(|f| f.key.clone());
                review.state.select(key);
                review.load_file = true;
            }
            review.editing = None;
        }
        Action::Jump(forward, search) => {
            review.state.jump(forward, search);
            review.state.file_focus = false;
            review.state.detail = true;
        }
        Action::Horizontal(delta) => {
            review.state.horizontal = review
                .state
                .horizontal
                .saturating_add_signed(delta)
                .min(8192);
        }
        Action::Group(group) => {
            review.state.collapsed[group as usize] = !review.state.collapsed[group as usize];
        }
    }
}

#[allow(clippy::too_many_lines)] // A single bounded request cycle, without blocking the event loop.
pub(crate) fn poll(client: &DaemonClient, app: &mut App) -> bool {
    let mut dirty = false;
    let visible = app.main_tab == TuiContentView::Changes && matches!(app.mode, Mode::Navigation);
    if visible
        && let Some(id) = app.selected_worktree_id
        && app.review.state.checkout != Some(id)
    {
        app.review.state.enter(id);
        app.review.refresh = true;
        dirty = true;
    }
    let review = &mut app.review;
    let reply = review
        .pending
        .as_mut()
        .and_then(|pending| match pending.reply.try_recv() {
            Ok(result) => Some(result),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => None,
            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => Some(Err(
                sylvops_daemon::DaemonError::Lifecycle("Changes request stopped".into()),
            )),
        });
    if let Some(result) = reply {
        let pending = review.pending.take().expect("pending review");
        if review.discard_pending {
            review.discard_pending = false;
            return true;
        }
        dirty = true;
        if Some(pending.checkout) == review.state.checkout
            && review.state.generation == pending.generation
            && app.selected_worktree_id == Some(pending.checkout)
        {
            match result {
                Ok((DaemonResponse::CheckoutChanges(list), _)) if !pending.file => {
                    review.state.apply_list(list);
                    review.load_file = true;
                }
                Ok((DaemonResponse::FileChange(file), Some(syntax))) if pending.file => {
                    review
                        .state
                        .apply_prepared(sylvops_core::changes::PreparedFile { file, syntax });
                    if review.state.updated {
                        review.updated_at = Some(Instant::now());
                    }
                }
                Ok((DaemonResponse::Error(error), _)) => {
                    review.state.error = Some(
                        error
                            .message
                            .chars()
                            .filter(|c| !c.is_control())
                            .take(512)
                            .collect(),
                    );
                }
                _ => {
                    review.state.error = Some(
                        "Cannot refresh changes. Check daemon health and press R to retry.".into(),
                    );
                }
            }
        } else {
            review.load_file = true;
        }
    }
    if !visible || review.pending.is_some() {
        return dirty;
    }
    let Some(id) = app.selected_worktree_id else {
        return dirty;
    };
    let request = if review.load_file {
        review.load_file = false;
        review.state.selected.clone().map(|key| {
            (
                true,
                ClientRequest::LoadFileChange {
                    worktree_id: id,
                    key,
                },
            )
        })
    } else if review.refresh
        || (!review.state.paused
            && review
                .last_refresh
                .is_none_or(|t| t.elapsed() >= Duration::from_secs(2)))
    {
        review.refresh = false;
        review.last_refresh = Some(Instant::now());
        Some((
            false,
            ClientRequest::ListCheckoutChanges { worktree_id: id },
        ))
    } else {
        None
    };
    if let Some((file, request)) = request {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let client = client.clone();
        tokio::spawn(async move {
            let response = match client.request(&request).await {
                Ok(DaemonResponse::FileChange(file)) => {
                    match tokio::task::spawn_blocking(move || {
                        sylvops_core::changes::PreparedFile::new(file)
                    })
                    .await
                    {
                        Ok(prepared) => Ok((
                            DaemonResponse::FileChange(prepared.file),
                            Some(prepared.syntax),
                        )),
                        Err(_) => Err(sylvops_daemon::DaemonError::Lifecycle(
                            "Cannot prepare file review".into(),
                        )),
                    }
                }
                Ok(response) => Ok((response, None)),
                Err(error) => Err(error),
            };
            let _ = tx.send(response);
        });
        review.pending = Some(Pending {
            checkout: id,
            generation: review.state.generation,
            file,
            reply: rx,
        });
        dirty = true;
    }
    dirty
}

#[allow(clippy::too_many_lines)] // One viewport layout, with file and diff rendering delegated below.
pub(crate) fn draw(frame: &mut Frame<'_>, app: &mut App, area: Rect, theme: Theme) {
    if area.height < 5 || area.width < 10 {
        return;
    }
    let state = &app.review.state;
    if state.checkout != app.selected_worktree_id || app.selected_worktree_id.is_none() {
        frame.render_widget(
            Paragraph::new("Select a checkout to review local changes.")
                .block(Block::bordered().title("Changes")),
            area,
        );
        return;
    }
    let name = app
        .snapshot
        .worktrees
        .iter()
        .find(|w| Some(w.id) == state.checkout)
        .map_or_else(String::new, |w| {
            format!("{} · {}", w.name, w.branch.as_deref().unwrap_or("detached"))
        });
    let warning = state.error.as_ref().map_or_else(
        || {
            if state.truncated {
                "File list incomplete".into()
            } else if app
                .review
                .updated_at
                .is_some_and(|at| at.elapsed() < Duration::from_secs(2))
            {
                "Updated".into()
            } else {
                String::new()
            }
        },
        |error| format!("Stale · {error}"),
    );
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(format!(
                "{name} · {} · {warning}",
                if state.paused {
                    "Paused"
                } else {
                    "Auto refresh"
                }
            )),
            Line::from(format!(
                "[f] Files: {}    [F] Find: {} ({} matches{})",
                state.filter,
                state.search,
                state.matches().len(),
                if state.file.as_ref().is_some_and(|f| f.truncated) {
                    ", partial"
                } else {
                    ""
                }
            )),
        ])
        .style(theme.text),
        Rect { height: 2, ..area },
    );
    let bottom = Rect {
        y: area.y + area.height - 1,
        height: 1,
        ..area
    };
    let controls = [
        ("[p] Pause/Resume ", Action::Pause),
        ("[R] Refresh ", Action::Refresh),
        ("[[] Prev hunk ", Action::Jump(false, false)),
        ("[]] Next hunk ", Action::Jump(true, false)),
        ("[Esc] Files ", Action::Back),
    ];
    let mut x = bottom.x;
    for (label, action) in controls {
        let width = u16::try_from(label.len())
            .unwrap_or(0)
            .min(bottom.right().saturating_sub(x));
        let rect = Rect::new(x, bottom.y, width, 1);
        frame.render_widget(Paragraph::new(label).style(theme.focus), rect);
        app.hit_map.push(rect, HitTarget::ReviewAction(action));
        x += width;
    }
    let content = Rect::new(area.x, area.y + 2, area.width, area.height - 3);
    let wide = content.width >= 100;
    let list_width = if wide {
        (content.width / 3).min(40)
    } else {
        content.width
    };
    let list_area = Rect {
        width: list_width,
        ..content
    };
    if wide || !state.detail {
        draw_files(frame, app, list_area, theme);
    }
    if wide || app.review.state.detail {
        let detail = if wide {
            Rect::new(
                content.x + list_width,
                content.y,
                content.width - list_width,
                content.height,
            )
        } else {
            content
        };
        draw_file(frame, app, detail, theme);
    }
}

fn draw_files(frame: &mut Frame<'_>, app: &mut App, area: Rect, theme: Theme) {
    let state = &app.review.state;
    let block = Block::default()
        .borders(Borders::ALL)
        .title(if state.file_focus {
            " Files · focused "
        } else {
            " Files "
        });
    let inner = block.inner(area);
    frame.render_widget(block.style(theme.border), area);
    let visible = state.visible();
    let selected = visible
        .iter()
        .position(|f| Some(&f.key) == state.selected.as_ref())
        .unwrap_or(0);
    let mut rows = Vec::new();
    let mut index = 0;
    for group in ChangeGroup::ALL {
        rows.push((
            format!(
                "{} {} ({})",
                if state.collapsed[group as usize] {
                    ">"
                } else {
                    "v"
                },
                group.label(),
                state.files.iter().filter(|f| f.key.group == group).count()
            ),
            HitTarget::ReviewAction(Action::Group(group)),
            false,
        ));
        for file in visible.iter().filter(|f| f.key.group == group) {
            rows.push((
                format!(
                    "{} {} · {}",
                    if Some(&file.key) == state.selected.as_ref() {
                        ">"
                    } else {
                        " "
                    },
                    file.key.display_path(),
                    file.status
                ),
                HitTarget::ReviewFile(index),
                index == selected,
            ));
            index += 1;
        }
    }
    let selected_row = rows
        .iter()
        .position(|(_, _, selected)| *selected)
        .unwrap_or(0);
    let start = selected_row.saturating_sub(usize::from(inner.height).saturating_sub(2));
    for (offset, (label, target, selected)) in rows
        .into_iter()
        .skip(start)
        .take(usize::from(inner.height))
        .enumerate()
    {
        let rect = Rect::new(
            inner.x,
            inner.y + u16::try_from(offset).unwrap_or(0),
            inner.width,
            1,
        );
        frame.render_widget(
            Paragraph::new(label).style(if selected { theme.focus } else { theme.text }),
            rect,
        );
        app.hit_map.push(rect, target);
    }
    if state.files.is_empty() {
        frame.render_widget(
            Paragraph::new(if app.review.pending.is_some() {
                "Loading…"
            } else if state.error.is_some() {
                "Changes unavailable"
            } else if state.truncated {
                "File list incomplete"
            } else {
                "Checkout is clean."
            }),
            inner,
        );
    } else if visible.is_empty() {
        frame.render_widget(
            Paragraph::new("No matches / groups collapsed"),
            Rect {
                y: inner.y + 3,
                height: inner.height.saturating_sub(3),
                ..inner
            },
        );
    }
}

fn draw_file(frame: &mut Frame<'_>, app: &mut App, area: Rect, theme: Theme) {
    let state = &app.review.state;
    let title = state.selected.as_ref().map_or_else(
        || "Choose a file".into(),
        |key| format!("{} · {}", key.group.label(), key.display_path()),
    );
    let block = Block::bordered().title(title);
    let inner = block.inner(area);
    frame.render_widget(block.style(theme.border), area);
    app.hit_map.push(inner, HitTarget::FocusMain);
    let Some(file) = &state.file else {
        frame.render_widget(Paragraph::new("Choose a file / loading…"), inner);
        return;
    };
    if let Some(notice) = &file.notice {
        frame.render_widget(Paragraph::new(notice.as_str()), inner);
        return;
    }
    let mut lines = Vec::new();
    for line in file
        .lines
        .iter()
        .skip(state.scroll)
        .take(usize::from(inner.height))
    {
        let old = line.old_line.map_or_else(String::new, |n| n.to_string());
        let new = line.new_line.map_or_else(String::new, |n| n.to_string());
        let style = match line.kind {
            DiffLineKind::Addition => theme.diff_addition,
            DiffLineKind::Deletion => theme.diff_deletion,
            DiffLineKind::HunkHeader => theme.diff_hunk_header,
            DiffLineKind::FileHeader => theme.diff_file_header,
            _ => theme.text,
        };
        let matched = !state.search.is_empty()
            && line
                .text
                .to_lowercase()
                .contains(&state.search.to_lowercase());
        let start = line
            .text
            .char_indices()
            .nth(state.horizontal)
            .map_or(line.text.len(), |(i, _)| i);
        let mut spans = vec![Span::styled(
            format!("{}{old:>5} {new:>5} ", if matched { ">" } else { " " }),
            if matched { theme.focus } else { theme.muted },
        )];
        let light = matches!(theme.background.bg, Some(ratatui::style::Color::Rgb(r,g,b)) if u16::from(r)+u16::from(g)+u16::from(b)>384);
        let index = state.scroll + lines.len();
        let mut offset = start;
        if let Some(tokens) = state.syntax.get(index) {
            for token in tokens.iter().filter(|t| t.end > start) {
                let begin = token.start.max(start);
                if begin > offset {
                    spans.push(Span::styled(&line.text[offset..begin], style));
                }
                let [r, g, b] = if light { token.light } else { token.dark };
                spans.push(Span::styled(
                    &line.text[begin..token.end],
                    style.fg(ratatui::style::Color::Rgb(r, g, b)),
                ));
                offset = token.end;
            }
        }
        if offset < line.text.len() {
            spans.push(Span::styled(&line.text[offset..], style));
        }
        lines.push(Line::from(spans).style(style));
    }
    frame.render_widget(Paragraph::new(lines), inner);
    if file.truncated && inner.height > 0 {
        frame.render_widget(
            Paragraph::new("Diff truncated · review safety limit"),
            Rect {
                y: inner.bottom() - 1,
                height: 1,
                ..inner
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sylvops_core::{
        changes::{ChangeKey, ChangeStatus, ChangedFile, CheckoutChanges, FileChange},
        domain::DaemonSnapshot,
    };

    fn fixture() -> App {
        let mut app = App::new(DaemonSnapshot::default(), Vec::new(), None);
        let id = WorktreeId::new();
        app.selected_worktree_id = Some(id);
        app.main_tab = TuiContentView::Changes;
        app.mode = Mode::Navigation;
        app.focus = FocusZone::Main;
        app.review.state.enter(id);
        let key = ChangeKey {
            group: ChangeGroup::Unstaged,
            path: b"src/main.rs".to_vec(),
        };
        app.review.state.apply_list(CheckoutChanges {
            worktree_id: id,
            files: vec![
                ChangedFile {
                    key: key.clone(),
                    status: ChangeStatus::Modified,
                    old_path: None,
                },
                ChangedFile {
                    key: ChangeKey {
                        group: ChangeGroup::Untracked,
                        path: b"new.md".to_vec(),
                    },
                    status: ChangeStatus::Untracked,
                    old_path: None,
                },
            ],
            truncated: false,
        });
        app.review.state.apply_file(FileChange::from_patch(
            id,
            key,
            "@@ -1 +1 @@\n-old\n+new\n",
            false,
        ));
        app
    }

    fn rendered(app: &mut App, width: u16) -> String {
        let backend = ratatui::backend::TestBackend::new(width, 20);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw(frame, app, frame.area(), Theme::resolve(app.theme)))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect()
    }

    #[test]
    fn file_navigation_filter_search_and_pause_are_independent() {
        let mut app = fixture();
        update(&mut app, Action::Move(1));
        assert_eq!(
            app.review.state.selected.as_ref().unwrap().group,
            ChangeGroup::Untracked
        );
        update(&mut app, Action::Filter);
        for c in "main".chars() {
            update(&mut app, Action::Character(c));
        }
        update(&mut app, Action::Finish);
        assert_eq!(app.review.state.visible().len(), 1);
        update(&mut app, Action::Pause);
        assert!(app.review.state.paused);
        update(&mut app, Action::Refresh);
        assert!(app.review.state.paused);
        assert!(app.review.refresh);
        assert_eq!(
            key_action(
                &app.review,
                &KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE)
            ),
            None
        );
    }

    #[test]
    fn wide_and_narrow_views_show_groups_diff_lines_and_stale_state() {
        let mut app = fixture();
        let wide = rendered(&mut app, 120);
        assert!(wide.contains("Unstaged (1)"));
        assert!(wide.contains("Untracked (1)"));
        assert!(wide.contains("+new"));
        let list = rendered(&mut app, 70);
        assert!(list.contains("src/main.rs"));
        assert!(!list.contains("+new"));
        update(&mut app, Action::Open);
        let detail = rendered(&mut app, 70);
        assert!(detail.contains("+new"));
        update(&mut app, Action::Back);
        assert_eq!(app.focus, FocusZone::Main);
        assert!(!app.review.state.detail);
        update(&mut app, Action::Open);
        app.review.state.error = Some("Git timed out".into());
        assert!(rendered(&mut app, 70).contains("Stale"));
        assert!(app.review.state.file.is_some());
    }
}
