//! Pure navigation reducer. Async daemon work is represented as effects.

use sylvops_core::ui::MainTab;

use crate::app::{App, ExplorerNode, Flash, FlashKind, FocusZone, Mode, Palette};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Action {
    ToggleFocus,
    Focus(FocusZone),
    Move(isize),
    MoveExplorer(isize),
    Expand,
    Collapse,
    SelectExplorer(usize),
    SelectTab(MainTab),
    Primary,
    New,
    Rename,
    Delete,
    Diff,
    Attention,
    Palette,
    ToggleHelp,
    Quit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Effect {
    Attach,
    OpenCreate,
    OpenRename,
    OpenDelete,
    LoadDiff,
    Quit,
}

pub(crate) fn reduce(app: &mut App, action: Action) -> Vec<Effect> {
    match action {
        Action::ToggleFocus => {
            app.focus = if app.focus == FocusZone::Explorer {
                FocusZone::Main
            } else {
                FocusZone::Explorer
            };
            app.mark_navigation_dirty();
        }
        Action::Focus(focus) => app.focus = focus,
        Action::Move(delta) if app.focus == FocusZone::Explorer => app.move_explorer(delta),
        Action::Move(_) => {}
        Action::MoveExplorer(delta) => {
            app.focus = FocusZone::Explorer;
            app.move_explorer(delta);
        }
        Action::Expand => app.expand_selected(),
        Action::Collapse => app.collapse_selected(),
        Action::SelectExplorer(index) => {
            app.focus = FocusZone::Explorer;
            app.select_explorer_index(index);
        }
        Action::SelectTab(tab) => app.set_tab(tab),
        Action::Primary => {
            if app.focus == FocusZone::Explorer {
                match app
                    .explorer_rows()
                    .get(app.explorer_index)
                    .map(|row| row.node)
                {
                    Some(ExplorerNode::Session(_)) => return vec![Effect::Attach],
                    Some(_) => app.expand_selected(),
                    None => return vec![Effect::OpenCreate],
                }
            } else if app.main_tab == MainTab::Terminal && app.selected_session().is_some() {
                return vec![Effect::Attach];
            }
        }
        Action::New => return vec![Effect::OpenCreate],
        Action::Rename => return vec![Effect::OpenRename],
        Action::Delete => return vec![Effect::OpenDelete],
        Action::Diff => {
            app.set_tab(MainTab::Changes);
            return vec![Effect::LoadDiff];
        }
        Action::Attention => {
            app.flash = Some(match app.select_attention() {
                Some(reason) => Flash::transient(FlashKind::Info, format!("Attention: {reason}")),
                None => Flash::transient(FlashKind::Info, "No sessions currently need attention."),
            });
        }
        Action::Palette => app.mode = Mode::CommandPalette(Palette::default()),
        Action::ToggleHelp => app.help = !app.help,
        Action::Quit => return vec![Effect::Quit],
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sylvops_core::domain::DaemonSnapshot;

    #[test]
    fn tabs_and_focus_are_synchronous_state_changes() {
        let mut app = App::new(DaemonSnapshot::default(), Vec::new(), None);
        reduce(&mut app, Action::SelectTab(MainTab::Details));
        assert_eq!(app.main_tab, MainTab::Details);
        assert_eq!(app.focus, FocusZone::Main);
        assert!(reduce(&mut app, Action::Quit).contains(&Effect::Quit));
    }
}
