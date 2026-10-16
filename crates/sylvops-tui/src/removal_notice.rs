use crossterm::event::{Event, KeyCode, MouseButton, MouseEventKind};
use sylvops_core::domain::{GitWorktreeState, WorktreeBlockingKind};

use crate::app::{App, HitTarget, Mode};

#[derive(Clone, Debug)]
pub(crate) struct RemovalNotice {
    pub lines: Vec<String>,
    pub scroll: u16,
    pub max_scroll: u16,
}

impl RemovalNotice {
    pub fn new(status: &GitWorktreeState) -> Self {
        let mut lines = vec![
            "Nothing was deleted.".into(),
            format!(
                "Tracked changes: {} | Untracked entries: {} | Ignored entries: {}",
                status.tracked_changes, status.untracked_files, status.ignored_files
            ),
            String::new(),
        ];
        for entry in &status.blocking_paths {
            let label = match entry.kind {
                WorktreeBlockingKind::Tracked => "Tracked change",
                WorktreeBlockingKind::Untracked => "Untracked content",
                WorktreeBlockingKind::Ignored => "Ignored content",
            };
            lines.push(format!("{label}: {}", entry.path));
        }
        if status.blocking_paths_truncated {
            lines.push("...and more entries. Showing a sample.".into());
        }
        lines.extend([
            String::new(),
            "One entry can be an entire folder (for example target/).".into(),
            "Review and remove or move the listed content, then try again.".into(),
        ]);
        Self {
            lines,
            scroll: 0,
            max_scroll: 0,
        }
    }
}

pub(crate) fn handle_input(app: &mut App, event: &Event) {
    let close = match event {
        Event::Key(key) => matches!(key.code, KeyCode::Esc | KeyCode::Enter),
        Event::Mouse(mouse) => {
            mouse.kind == MouseEventKind::Down(MouseButton::Left)
                && app.hit_map.target_at(mouse.column, mouse.row) == Some(HitTarget::DismissNotice)
        }
        _ => false,
    };
    if close {
        app.mode = Mode::Navigation;
        return;
    }
    let Mode::RemovalBlocked(notice) = &mut app.mode else {
        return;
    };
    let delta = match event {
        Event::Key(key) => match key.code {
            KeyCode::Up => -1,
            KeyCode::Down => 1,
            KeyCode::PageUp => -8,
            KeyCode::PageDown => 8,
            KeyCode::Home => {
                notice.scroll = 0;
                return;
            }
            KeyCode::End => {
                notice.scroll = notice.max_scroll;
                return;
            }
            _ => 0,
        },
        Event::Mouse(mouse) => match mouse.kind {
            MouseEventKind::ScrollUp => -3,
            MouseEventKind::ScrollDown => 3,
            _ => 0,
        },
        _ => 0,
    };
    notice.scroll = notice
        .scroll
        .saturating_add_signed(delta)
        .min(notice.max_scroll);
}
