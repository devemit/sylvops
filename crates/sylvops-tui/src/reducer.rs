//! Pure navigation reducer. Async daemon work is represented as effects.

use sylvops_core::{domain::session_can_resume, ids::SessionId, ui::MainTab};

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
    Resume,
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
    Resume(SessionId),
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
        Action::Resume => {
            if let Some(session) = app.selected_session()
                && app.resume_pending.is_none()
                && session_can_resume(session, &app.snapshot.sessions)
            {
                let session_id = session.id;
                app.resume_pending = Some(session_id);
                return vec![Effect::Resume(session_id)];
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
    use sylvops_core::{
        domain::{DaemonSnapshot, ProviderKind, Session, SessionState},
        ids::{SessionId, WorktreeId},
    };

    fn session(state: SessionState, external_session_id: Option<&str>) -> Session {
        Session {
            id: SessionId::new(),
            worktree_id: WorktreeId::new(),
            provider_profile_id: None,
            provider_kind: ProviderKind::Codex,
            display_name: "Codex".into(),
            state,
            process_id: None,
            external_session_id: external_session_id.map(str::to_owned),
            command: "codex".into(),
            arguments_json: "[]".into(),
            cwd: "/repo".into(),
            created_at: 1,
            started_at: Some(1),
            ended_at: Some(2),
            last_activity_at: 2,
            last_seen_output_sequence: 0,
            exit_code: Some(0),
            failure_reason: None,
        }
    }

    #[test]
    fn tabs_and_focus_are_synchronous_state_changes() {
        let mut app = App::new(DaemonSnapshot::default(), Vec::new(), None);
        reduce(&mut app, Action::SelectTab(MainTab::Details));
        assert_eq!(app.main_tab, MainTab::Details);
        assert_eq!(app.focus, FocusZone::Main);
        assert!(reduce(&mut app, Action::Quit).contains(&Effect::Quit));
    }

    #[test]
    fn resume_effect_is_only_offered_for_verified_eligible_sessions() {
        let resumable = session(SessionState::FinishedSeen, Some("verified-id"));
        let resumable_id = resumable.id;
        let mut app = App::new(DaemonSnapshot::default(), Vec::new(), None);
        app.snapshot.sessions.push(resumable);
        app.selected_session_id = Some(resumable_id);

        assert_eq!(
            reduce(&mut app, Action::Resume),
            vec![Effect::Resume(resumable_id)]
        );
        assert!(reduce(&mut app, Action::Resume).is_empty());

        app.resume_pending = None;
        app.snapshot.sessions[0].external_session_id = None;
        assert!(reduce(&mut app, Action::Resume).is_empty());
        app.snapshot.sessions[0].external_session_id = Some("verified-id".into());
        app.snapshot.sessions[0].state = SessionState::Terminated;
        assert!(reduce(&mut app, Action::Resume).is_empty());

        app.snapshot.sessions[0].state = SessionState::FinishedSeen;
        app.snapshot.sessions[0].provider_kind = ProviderKind::Shell;
        assert!(reduce(&mut app, Action::Resume).is_empty());

        app.snapshot.sessions[0].provider_kind = ProviderKind::Codex;
        let mut successor = app.snapshot.sessions[0].clone();
        successor.id = SessionId::new();
        successor.created_at = 2;
        successor.state = SessionState::Running;
        app.snapshot.sessions.push(successor);
        assert!(reduce(&mut app, Action::Resume).is_empty());
    }
}
