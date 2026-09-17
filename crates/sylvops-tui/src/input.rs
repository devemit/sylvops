use crossterm::event::{Event, KeyCode, KeyModifiers, MouseButton, MouseEventKind};
use sylvops_core::ui::MainTab;

use crate::{
    app::{App, HitTarget},
    reducer::Action,
};

pub(crate) fn navigation_action(app: &App, event: &Event) -> Option<Action> {
    match event {
        Event::Key(key) => match key.code {
            KeyCode::Char('?') => Some(Action::ToggleHelp),
            KeyCode::Char('q') => Some(Action::Quit),
            KeyCode::Tab | KeyCode::BackTab => Some(Action::ToggleFocus),
            KeyCode::Down | KeyCode::Char('j') => Some(Action::Move(1)),
            KeyCode::Up | KeyCode::Char('k') => Some(Action::Move(-1)),
            KeyCode::Right | KeyCode::Char('l') => Some(Action::Expand),
            KeyCode::Left | KeyCode::Char('h') => Some(Action::Collapse),
            KeyCode::Enter => Some(Action::Primary),
            KeyCode::Char('1') => Some(Action::SelectTab(MainTab::Terminal)),
            KeyCode::Char('2') => Some(Action::SelectTab(MainTab::Changes)),
            KeyCode::Char('3') => Some(Action::SelectTab(MainTab::Details)),
            KeyCode::Char('n') => Some(Action::New),
            KeyCode::Char('r') => Some(Action::Rename),
            KeyCode::Char('d') => Some(Action::Delete),
            KeyCode::Char('g') => Some(Action::Diff),
            KeyCode::Char('/') => Some(Action::Palette),
            KeyCode::Char('A') if key.modifiers.contains(KeyModifiers::SHIFT) => {
                Some(Action::Attention)
            }
            KeyCode::Esc if app.help => Some(Action::ToggleHelp),
            _ => None,
        },
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
            MouseEventKind::Down(MouseButton::Left) => app
                .hit_map
                .target_at(mouse.column, mouse.row)
                .and_then(target_action),
            _ => None,
        },
        _ => None,
    }
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
        HitTarget::FocusExplorer => Some(Action::Focus(crate::app::FocusZone::Explorer)),
        HitTarget::FocusMain => Some(Action::Focus(crate::app::FocusZone::Main)),
        HitTarget::Attach => Some(Action::Primary),
        HitTarget::New => Some(Action::New),
        HitTarget::Rename => Some(Action::Rename),
        HitTarget::Delete => Some(Action::Delete),
        HitTarget::Attention => Some(Action::Attention),
        _ => None,
    }
}
