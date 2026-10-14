//! Pure navigation reducer. Async daemon work is represented as effects.

use sylvops_core::{
    ids::{ProjectId, SessionId},
    provider::session_can_resume,
    ui::TuiContentView,
};

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
    SelectActiveSession(SessionId),
    OpenActiveSession(SessionId),
    StopActiveSession(SessionId),
    SelectTab(TuiContentView),
    Primary,
    Resume,
    New,
    NewWorktree,
    Rename,
    Delete,
    Diff,
    GitHub,
    Attention,
    Settings,
    Palette,
    ToggleHelp,
    Quit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Effect {
    Attach,
    Resume(SessionId),
    OpenCreate,
    OpenCreateWorktree(ProjectId),
    OpenRename,
    OpenDelete,
    LoadDiff,
    LoadGitHubStatus,
    OpenGitHub,
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
        Action::Move(delta)
            if app.focus == FocusZone::Main && app.main_tab == TuiContentView::Sessions =>
        {
            app.move_active_session(delta);
        }
        Action::Move(_) => {}
        Action::MoveExplorer(delta) => {
            app.focus = FocusZone::Explorer;
            app.move_explorer(delta);
        }
        Action::Expand
            if app.focus == FocusZone::Main && app.main_tab == TuiContentView::Sessions =>
        {
            app.move_active_session(1);
        }
        Action::Collapse
            if app.focus == FocusZone::Main && app.main_tab == TuiContentView::Sessions =>
        {
            app.move_active_session(-1);
        }
        Action::Expand => app.expand_selected(),
        Action::Collapse => app.collapse_selected(),
        Action::SelectExplorer(index) => {
            app.focus = FocusZone::Explorer;
            app.select_explorer_index(index);
        }
        Action::SelectActiveSession(session_id) => {
            select_active_session(app, session_id, None);
        }
        Action::OpenActiveSession(session_id) => {
            return select_active_session(app, session_id, Some(Effect::Attach));
        }
        Action::StopActiveSession(session_id) => {
            return select_active_session(app, session_id, Some(Effect::OpenDelete));
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
            } else if matches!(
                app.main_tab,
                TuiContentView::Terminal | TuiContentView::Sessions
            ) && app.selected_session().is_some()
            {
                return vec![Effect::Attach];
            }
        }
        Action::Resume => {
            if let Some(session) = app.selected_session()
                && app.resume_pending.is_none()
                && session_can_resume(session, &app.snapshot.sessions, &app.providers)
            {
                let session_id = session.id;
                app.resume_pending = Some(session_id);
                return vec![Effect::Resume(session_id)];
            }
        }
        Action::New => return vec![Effect::OpenCreate],
        Action::NewWorktree => return new_worktree_effect(app),
        Action::Rename => return vec![Effect::OpenRename],
        Action::Delete => return vec![Effect::OpenDelete],
        Action::Diff => {
            app.set_tab(TuiContentView::Changes);
            return vec![Effect::LoadDiff];
        }
        Action::GitHub => return vec![Effect::OpenGitHub],
        Action::Attention => {
            app.flash = Some(match app.select_attention() {
                Some(reason) => Flash::transient(FlashKind::Info, format!("Attention: {reason}")),
                None => Flash::transient(FlashKind::Info, "No sessions currently need attention."),
            });
        }
        Action::Settings => {
            app.open_settings();
            return vec![Effect::LoadGitHubStatus];
        }
        Action::Palette => app.mode = Mode::CommandPalette(Palette::default()),
        Action::ToggleHelp => app.help = !app.help,
        Action::Quit => return vec![Effect::Quit],
    }
    Vec::new()
}

fn new_worktree_effect(app: &App) -> Vec<Effect> {
    app.selected_project()
        .map(|project| vec![Effect::OpenCreateWorktree(project.id)])
        .unwrap_or_default()
}

fn select_active_session(
    app: &mut App,
    session_id: SessionId,
    effect: Option<Effect>,
) -> Vec<Effect> {
    app.focus = FocusZone::Main;
    if app.select_active_session(session_id) {
        effect.into_iter().collect()
    } else {
        app.flash = Some(Flash::transient(
            FlashKind::Info,
            "That Active Session is no longer available.",
        ));
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sylvops_core::{
        domain::{DaemonSnapshot, ProviderKind, Session, SessionState},
        ids::{SessionId, WorktreeId},
        provider::{AuthenticationRequirement, ProviderCapabilities, ProviderHealth},
    };

    fn session(state: SessionState, external_session_id: Option<&str>) -> Session {
        Session {
            id: SessionId::new(),
            worktree_id: WorktreeId::new(),
            provider_profile_id: None,
            provider_kind: ProviderKind::Claude,
            display_name: "Claude".into(),
            state,
            process_id: None,
            external_session_id: external_session_id.map(str::to_owned),
            command: "claude".into(),
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

    fn ready_claude() -> ProviderHealth {
        ProviderHealth {
            kind: ProviderKind::Claude,
            available: true,
            authenticated: true,
            executable_path: None,
            version: Some("2.1.145".into()),
            diagnostic: None,
            capabilities: ProviderCapabilities {
                interactive: true,
                resume: true,
                authentication: AuthenticationRequirement::ExistingLogin,
                ..ProviderCapabilities::default()
            },
            checked_at: 0,
        }
    }

    #[test]
    fn tabs_and_focus_are_synchronous_state_changes() {
        let mut app = App::new(DaemonSnapshot::default(), vec![ready_claude()], None);
        reduce(&mut app, Action::SelectTab(TuiContentView::Details));
        assert_eq!(app.main_tab, TuiContentView::Details);
        assert_eq!(app.focus, FocusZone::Main);
        assert!(reduce(&mut app, Action::Quit).contains(&Effect::Quit));
        assert_eq!(reduce(&mut app, Action::GitHub), vec![Effect::OpenGitHub]);
    }

    #[test]
    fn resume_effect_is_only_offered_for_verified_eligible_sessions() {
        let resumable = session(SessionState::FinishedSeen, Some("verified-id"));
        let resumable_id = resumable.id;
        let mut app = App::new(DaemonSnapshot::default(), vec![ready_claude()], None);
        app.snapshot.sessions.push(resumable);
        app.selected_session_id = Some(resumable_id);

        assert_eq!(
            reduce(&mut app, Action::Resume),
            vec![Effect::Resume(resumable_id)]
        );
        assert_eq!(reduce(&mut app, Action::Resume), Vec::<Effect>::new());

        app.resume_pending = None;
        app.snapshot.sessions[0].external_session_id = None;
        assert_eq!(reduce(&mut app, Action::Resume), Vec::<Effect>::new());
        app.snapshot.sessions[0].external_session_id = Some("verified-id".into());
        app.snapshot.sessions[0].state = SessionState::Terminated;
        assert_eq!(reduce(&mut app, Action::Resume), Vec::<Effect>::new());

        app.snapshot.sessions[0].state = SessionState::FinishedSeen;
        app.snapshot.sessions[0].provider_kind = ProviderKind::Shell;
        assert_eq!(reduce(&mut app, Action::Resume), Vec::<Effect>::new());

        app.snapshot.sessions[0].provider_kind = ProviderKind::Claude;
        let mut successor = app.snapshot.sessions[0].clone();
        successor.id = SessionId::new();
        successor.created_at = 2;
        successor.state = SessionState::Running;
        app.snapshot.sessions.push(successor);
        assert_eq!(reduce(&mut app, Action::Resume), Vec::<Effect>::new());
    }
}
