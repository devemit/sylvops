//! Pure navigation reducer. Async daemon work is represented as effects.

use sylvops_core::{
    ids::{ProjectId, SessionId},
    provider::session_can_resume,
    session_state::AttentionReason,
    ui::{TuiContentView, TuiNavigatorView},
};

use crate::app::{
    App, ExplorerNode, Flash, FlashKind, FocusZone, Mode, NavigatorResizeState, Palette,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Action {
    ToggleFocus,
    Focus(FocusZone),
    Move(isize),
    MoveExplorer(isize),
    Expand,
    Collapse,
    SelectExplorer(usize),
    SelectOverviewSession(SessionId),
    OpenOverviewSession(SessionId),
    StopOverviewSession(SessionId),
    DismissAttention(SessionId),
    SelectNavigatorView(TuiNavigatorView),
    BeginNavigatorResize,
    ResizeNavigator(u16),
    EndNavigatorResize,
    AdjustNavigatorWidth(i16),
    SelectTab(TuiContentView),
    Primary,
    Resume,
    New,
    NewWorktree,
    Rename,
    Delete,
    Diff,
    ScrollDiff(isize),
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
    OpenAttention {
        session_id: SessionId,
        attention_id: sylvops_core::session_state::AttentionEventId,
        reason: AttentionReason,
    },
    DismissAttention {
        session_id: SessionId,
        attention_id: sylvops_core::session_state::AttentionEventId,
    },
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

#[allow(clippy::too_many_lines)]
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
        Action::Move(delta)
            if app.focus == FocusZone::Explorer
                && app.navigator_view == TuiNavigatorView::Active =>
        {
            app.move_overview_session(delta);
        }
        Action::Move(delta) if app.focus == FocusZone::Explorer => app.move_explorer(delta),
        Action::MoveExplorer(delta) => {
            app.focus = FocusZone::Explorer;
            if app.navigator_view == TuiNavigatorView::Active {
                app.move_overview_session(delta);
            } else {
                app.move_explorer(delta);
            }
        }
        Action::Expand => app.expand_selected(),
        Action::Collapse => app.collapse_selected(),
        Action::SelectExplorer(index) => {
            app.focus = FocusZone::Explorer;
            app.select_explorer_index(index);
        }
        Action::SelectOverviewSession(session_id) => {
            select_overview_session(app, session_id, None, false);
        }
        Action::OpenOverviewSession(session_id) => {
            return open_overview_session(app, session_id);
        }
        Action::StopOverviewSession(session_id) => {
            return select_overview_session(app, session_id, Some(Effect::OpenDelete), false);
        }
        Action::DismissAttention(session_id) => {
            return dismiss_overview_attention(app, session_id);
        }
        Action::SelectNavigatorView(view) => app.select_navigator_view(view),
        Action::BeginNavigatorResize => {
            app.navigator_resizing = NavigatorResizeState::Dragging;
        }
        Action::ResizeNavigator(column)
            if app.navigator_resizing == NavigatorResizeState::Dragging =>
        {
            app.set_navigator_width(column);
        }
        Action::Move(_) | Action::ResizeNavigator(_) => {}
        Action::EndNavigatorResize => app.navigator_resizing = NavigatorResizeState::Idle,
        Action::AdjustNavigatorWidth(delta) => app.adjust_navigator_width(delta),
        Action::SelectTab(tab) => app.set_tab(tab),
        Action::Primary => {
            if app.focus == FocusZone::Explorer
                && app.navigator_view == TuiNavigatorView::Active
                && let Some(session_id) = app.selected_session_id
                && app
                    .session_overview()
                    .iter()
                    .any(|session| session.id == session_id)
            {
                return open_overview_session(app, session_id);
            }
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
            } else if app.main_tab == TuiContentView::Terminal && app.selected_session().is_some() {
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
            app.diff_scroll = 0;
            app.set_tab(TuiContentView::Changes);
            return vec![Effect::LoadDiff];
        }
        Action::ScrollDiff(delta) => app.scroll_diff(delta),
        Action::GitHub => return vec![Effect::OpenGitHub],
        Action::Attention => {
            app.flash = Some(match app.select_attention() {
                Some(reason) => Flash::transient(FlashKind::Info, format!("Attention: {reason}")),
                None => Flash::transient(FlashKind::Info, "No Sessions currently need attention."),
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

fn open_overview_session(app: &mut App, session_id: SessionId) -> Vec<Effect> {
    if !app.select_overview_session(session_id) {
        return Vec::new();
    }
    app.navigator_view = TuiNavigatorView::Active;
    app.set_tab(TuiContentView::Terminal);
    if let Some(event) = app
        .selected_session()
        .and_then(|session| session.status.attention)
        .filter(|event| {
            event.acknowledgement == sylvops_core::session_state::AttentionAcknowledgement::Pending
        })
    {
        vec![Effect::OpenAttention {
            session_id,
            attention_id: event.id,
            reason: event.reason,
        }]
    } else {
        vec![Effect::Attach]
    }
}

fn dismiss_overview_attention(app: &mut App, session_id: SessionId) -> Vec<Effect> {
    if !app.select_overview_session(session_id) {
        return Vec::new();
    }
    let recoverable = app.selected_session().and_then(|session| {
        session.status.attention.filter(|event| {
            event.reason == AttentionReason::RecoverableFailure
                && event.acknowledgement
                    == sylvops_core::session_state::AttentionAcknowledgement::Pending
        })
    });
    if let Some(event) = recoverable {
        vec![Effect::DismissAttention {
            session_id,
            attention_id: event.id,
        }]
    } else {
        Vec::new()
    }
}

fn new_worktree_effect(app: &App) -> Vec<Effect> {
    app.selected_project()
        .map(|project| vec![Effect::OpenCreateWorktree(project.id)])
        .unwrap_or_default()
}

fn select_overview_session(
    app: &mut App,
    session_id: SessionId,
    effect: Option<Effect>,
    focus_main: bool,
) -> Vec<Effect> {
    if app.select_overview_session(session_id) {
        app.navigator_view = TuiNavigatorView::Active;
        if focus_main {
            app.set_tab(TuiContentView::Terminal);
        } else {
            app.focus = FocusZone::Explorer;
            app.mark_navigation_dirty();
        }
        effect.into_iter().collect()
    } else {
        app.flash = Some(Flash::transient(
            FlashKind::Info,
            "That Session is no longer available.",
        ));
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sylvops_core::{
        domain::{DaemonSnapshot, ProviderKind, Session},
        ids::{SessionId, WorktreeId},
        provider::{AuthenticationRequirement, ProviderCapabilities, ProviderHealth},
    };

    use sylvops_core::session_state::{SessionLifecycle, SessionStatus};

    fn session(lifecycle: SessionLifecycle, external_session_id: Option<&str>) -> Session {
        Session {
            id: SessionId::new(),
            worktree_id: WorktreeId::new(),
            provider_profile_id: None,
            provider_kind: ProviderKind::Claude,
            display_name: "Claude".into(),
            status: SessionStatus {
                lifecycle,
                ..SessionStatus::default()
            },
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
        let resumable = session(SessionLifecycle::Exited, Some("verified-id"));
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
        app.snapshot.sessions[0].status.lifecycle = SessionLifecycle::Terminated;
        assert_eq!(
            reduce(&mut app, Action::Resume),
            vec![Effect::Resume(resumable_id)]
        );
        app.resume_pending = None;

        app.snapshot.sessions[0].status.lifecycle = SessionLifecycle::Exited;
        app.snapshot.sessions[0].provider_kind = ProviderKind::Shell;
        assert_eq!(reduce(&mut app, Action::Resume), Vec::<Effect>::new());

        app.snapshot.sessions[0].provider_kind = ProviderKind::Claude;
        let mut successor = app.snapshot.sessions[0].clone();
        successor.id = SessionId::new();
        successor.created_at = 2;
        successor.status.lifecycle = SessionLifecycle::Running;
        app.snapshot.sessions.push(successor);
        assert_eq!(reduce(&mut app, Action::Resume), Vec::<Effect>::new());
    }

    #[test]
    fn diff_scrolling_is_bounded_and_refresh_resets_to_the_top() {
        let mut app = App::new(DaemonSnapshot::default(), Vec::new(), None);
        app.diff = Some("one\ntwo\nthree".into());

        assert_eq!(
            reduce(&mut app, Action::ScrollDiff(1)),
            Vec::<Effect>::new()
        );
        assert_eq!(app.diff_scroll, 1);
        reduce(&mut app, Action::ScrollDiff(100));
        assert_eq!(app.diff_scroll, 2);
        reduce(&mut app, Action::ScrollDiff(-1));
        assert_eq!(app.diff_scroll, 1);
        reduce(&mut app, Action::ScrollDiff(-100));
        assert_eq!(app.diff_scroll, 0);

        app.diff_scroll = 2;
        assert_eq!(reduce(&mut app, Action::Diff), vec![Effect::LoadDiff]);
        assert_eq!(app.diff_scroll, 0);
    }
}
