use crate::{
    app::{App, HitTarget, main_view_from_shortcut},
    reducer::Action,
};
use crossterm::event::{Event, KeyCode, KeyModifiers, MouseButton, MouseEventKind};

pub(crate) fn navigation_action(app: &App, event: &Event) -> Option<Action> {
    match event {
        Event::Key(key) => {
            if let KeyCode::Char(shortcut) = key.code
                && let Some(view) = main_view_from_shortcut(shortcut)
            {
                return Some(Action::SelectTab(view));
            }
            match key.code {
                KeyCode::Char('?') => Some(Action::ToggleHelp),
                KeyCode::Char('q') => Some(Action::Quit),
                KeyCode::Tab | KeyCode::BackTab => Some(Action::ToggleFocus),
                KeyCode::Down | KeyCode::Char('j') => Some(Action::Move(1)),
                KeyCode::Up | KeyCode::Char('k') => Some(Action::Move(-1)),
                KeyCode::Right | KeyCode::Char('l') => Some(Action::Expand),
                KeyCode::Left | KeyCode::Char('h') => Some(Action::Collapse),
                KeyCode::Enter => Some(Action::Primary),
                KeyCode::Char('u') => Some(Action::Resume),
                KeyCode::Char('n') => Some(Action::New),
                KeyCode::Char('r') => Some(Action::Rename),
                KeyCode::Char('d') => Some(Action::Delete),
                KeyCode::Char('G') => Some(Action::GitHub),
                KeyCode::Char('g') => Some(Action::Diff),
                KeyCode::Char('s') => Some(Action::Settings),
                KeyCode::Char('/') => Some(Action::Palette),
                KeyCode::Char('A') if key.modifiers.contains(KeyModifiers::SHIFT) => {
                    Some(Action::Attention)
                }
                KeyCode::Esc if app.help => Some(Action::ToggleHelp),
                _ => None,
            }
        }
        Event::Mouse(mouse) => match mouse.kind {
            MouseEventKind::ScrollDown
                if explorer_target(app.hit_map.target_at(mouse.column, mouse.row)) =>
            {
                Some(Action::MoveExplorer(1))
            }
            MouseEventKind::ScrollUp
                if explorer_target(app.hit_map.target_at(mouse.column, mouse.row)) =>
            {
                Some(Action::MoveExplorer(-1))
            }
            MouseEventKind::ScrollDown
                if overview_target(app.hit_map.target_at(mouse.column, mouse.row)) =>
            {
                Some(Action::Move(1))
            }
            MouseEventKind::ScrollUp
                if overview_target(app.hit_map.target_at(mouse.column, mouse.row)) =>
            {
                Some(Action::Move(-1))
            }
            MouseEventKind::Down(MouseButton::Left) => app
                .hit_map
                .target_at(mouse.column, mouse.row)
                .and_then(target_action),
            _ => None,
        },
        _ => None,
    }
}

fn overview_target(target: Option<HitTarget>) -> bool {
    matches!(
        target,
        Some(
            HitTarget::FocusMain
                | HitTarget::ActiveSession(_)
                | HitTarget::ActiveSessionOpen(_)
                | HitTarget::ActiveSessionStop(_)
        )
    )
}

fn explorer_target(target: Option<HitTarget>) -> bool {
    matches!(
        target,
        Some(HitTarget::ExplorerRow(_) | HitTarget::FocusExplorer)
    )
}

fn target_action(target: HitTarget) -> Option<Action> {
    match target {
        HitTarget::ExplorerRow(index) => Some(Action::SelectExplorer(index)),
        HitTarget::Tab(tab) => Some(Action::SelectTab(tab)),
        HitTarget::ActiveSession(session_id) => Some(Action::SelectActiveSession(session_id)),
        HitTarget::ActiveSessionOpen(session_id) => Some(Action::OpenActiveSession(session_id)),
        HitTarget::ActiveSessionStop(session_id) => Some(Action::StopActiveSession(session_id)),
        HitTarget::FocusExplorer => Some(Action::Focus(crate::app::FocusZone::Explorer)),
        HitTarget::FocusMain => Some(Action::Focus(crate::app::FocusZone::Main)),
        HitTarget::Attach => Some(Action::Primary),
        HitTarget::Resume => Some(Action::Resume),
        HitTarget::New => Some(Action::New),
        HitTarget::Rename => Some(Action::Rename),
        HitTarget::Delete => Some(Action::Delete),
        HitTarget::Diff => Some(Action::Diff),
        HitTarget::Attention => Some(Action::Attention),
        HitTarget::Settings => Some(Action::Settings),
        HitTarget::Commands => Some(Action::Palette),
        HitTarget::Help => Some(Action::ToggleHelp),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;
    use sylvops_core::domain::DaemonSnapshot;

    #[test]
    fn uppercase_g_opens_github_while_lowercase_g_keeps_diff() {
        let app = App::new(DaemonSnapshot::default(), Vec::new(), None);

        assert_eq!(
            navigation_action(
                &app,
                &Event::Key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT)),
            ),
            Some(Action::GitHub)
        );
        assert_eq!(
            navigation_action(
                &app,
                &Event::Key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE)),
            ),
            Some(Action::Diff)
        );
    }
}
