use std::time::Duration;

use sylvops_core::{
    attention::AttentionNotificationPreferences,
    domain::ProviderKind,
    protocol::{ClientRequest, DaemonEvent, DaemonResponse},
    session_state::{AttentionAcknowledgement, AttentionReason},
};
use sylvops_daemon::{client::DaemonClient, daemon, runtime::RuntimePaths};
use sylvops_test_support::{AbortOnDropTask, TemporaryRepository};

const TEST_TIMEOUT: Duration = Duration::from_secs(30);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_attention_claims_are_single_connection_scoped_and_preferences_persist() {
    tokio::time::timeout(TEST_TIMEOUT, exercise_attention_notifications())
        .await
        .expect("attention notification integration timeout");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn foreground_focus_suppresses_delivery_but_requires_explicit_review() {
    tokio::time::timeout(TEST_TIMEOUT, exercise_foreground_focus())
        .await
        .expect("attention focus integration timeout");
}

async fn exercise_foreground_focus() {
    let repository = TemporaryRepository::initialize()
        .await
        .expect("temporary repository");
    let paths = RuntimePaths::discover(Some(&repository.root().join("state"))).expect("paths");
    let daemon_paths = paths.clone();
    let mut daemon_task = AbortOnDropTask::spawn(async move { daemon::run(daemon_paths).await });
    let desktop = connect_eventually(&paths, "attention-desktop").await;
    let tui = connect_eventually(&paths, "attention-tui").await;

    let workspace_id = match desktop
        .request(&ClientRequest::AddWorkspace {
            name: "focus".into(),
        })
        .await
        .expect("workspace response")
    {
        DaemonResponse::WorkspaceAdded { workspace, .. } => workspace.id,
        response => panic!("unexpected workspace response: {response:?}"),
    };
    let worktree_id = match desktop
        .request(&ClientRequest::AddProject {
            workspace_id,
            repository_path: repository.path().to_string_lossy().into_owned(),
        })
        .await
        .expect("project response")
    {
        DaemonResponse::ProjectAdded { root_worktree, .. } => root_worktree.id,
        response => panic!("unexpected project response: {response:?}"),
    };

    let visible = create_shell_session(&desktop, worktree_id, "Visible session").await;
    let other = create_shell_session(&desktop, worktree_id, "Other\nprivate").await;
    report_focus(&desktop, Some(visible)).await;
    let visible_event = finish_shell_session(&desktop, visible).await;

    assert!(claim(&desktop).await.is_none());
    assert!(claim(&tui).await.is_none());
    assert_eq!(
        attention(&desktop, visible).await.acknowledgement,
        AttentionAcknowledgement::Pending
    );

    report_focus(&desktop, None).await;
    report_focus(&desktop, Some(visible)).await;
    assert_eq!(
        attention(&tui, visible).await.acknowledgement,
        AttentionAcknowledgement::Pending
    );
    assert!(matches!(
        desktop
            .request(&ClientRequest::AcknowledgeAttention {
                session_id: visible,
                attention_id: visible_event.id,
            })
            .await
            .expect("explicit review"),
        DaemonResponse::SessionAttentionAcknowledged { .. }
    ));
    assert_eq!(
        attention(&tui, visible).await.acknowledgement,
        AttentionAcknowledgement::Acknowledged
    );

    let other_event = finish_shell_session(&desktop, other).await;
    let (desktop_claim, tui_claim) = tokio::join!(claim(&desktop), claim(&tui));
    let granted = desktop_claim
        .as_ref()
        .or(tui_claim.as_ref())
        .expect("one delivery grant for the non-visible Session");
    assert!(desktop_claim.is_none() ^ tui_claim.is_none());
    assert_eq!(granted.session_id, other);
    assert_eq!(granted.attention_id, other_event.id);
    assert_eq!(granted.session_display_name, "Otherprivate");
    assert_eq!(visible_event.reason, AttentionReason::SuccessfulCompletion);

    if desktop_claim.is_some() {
        drop(desktop);
        assert!(claim(&tui).await.is_none());
        assert_eq!(
            attention(&tui, other).await.acknowledgement,
            AttentionAcknowledgement::Pending
        );
        tui.request(&ClientRequest::ShutdownDaemon)
            .await
            .expect("shutdown response");
    } else {
        drop(tui);
        assert!(claim(&desktop).await.is_none());
        assert_eq!(
            attention(&desktop, other).await.acknowledgement,
            AttentionAcknowledgement::Pending
        );
        desktop
            .request(&ClientRequest::ShutdownDaemon)
            .await
            .expect("shutdown response");
    }
    daemon_task
        .join()
        .await
        .expect("daemon join")
        .expect("daemon shutdown");
}

#[allow(clippy::too_many_lines)]
async fn exercise_attention_notifications() {
    let repository = TemporaryRepository::initialize()
        .await
        .expect("temporary repository");
    let paths = RuntimePaths::discover(Some(&repository.root().join("state"))).expect("paths");
    let daemon_paths = paths.clone();
    let mut daemon_task = AbortOnDropTask::spawn(async move { daemon::run(daemon_paths).await });
    let first = connect_eventually(&paths, "attention-first").await;

    let defaults = preferences(&first).await;
    assert_eq!(defaults, AttentionNotificationPreferences::default());
    assert!(defaults.native_notifications_enabled);
    assert!(defaults.sound_enabled);

    let disabled_native = AttentionNotificationPreferences {
        native_notifications_enabled: false,
        sound_enabled: true,
    };
    assert!(matches!(
        first
            .request(&ClientRequest::SaveAttentionNotificationPreferences {
                preferences: disabled_native,
            })
            .await
            .expect("save preferences"),
        DaemonResponse::AttentionNotificationPreferencesSaved
    ));
    assert_eq!(preferences(&first).await, disabled_native);

    let workspace_id = match first
        .request(&ClientRequest::AddWorkspace {
            name: "attention".into(),
        })
        .await
        .expect("workspace response")
    {
        DaemonResponse::WorkspaceAdded { workspace, .. } => workspace.id,
        response => panic!("unexpected workspace response: {response:?}"),
    };
    let worktree_id = match first
        .request(&ClientRequest::AddProject {
            workspace_id,
            repository_path: repository.path().to_string_lossy().into_owned(),
        })
        .await
        .expect("project response")
    {
        DaemonResponse::ProjectAdded { root_worktree, .. } => root_worktree.id,
        response => panic!("unexpected project response: {response:?}"),
    };

    let second = connect_eventually(&paths, "attention-second").await;
    let first_session = create_shell_session(&first, worktree_id, "First completion").await;
    let first_event = finish_shell_session(&first, first_session).await;
    let first_claim = claim(&first).await;
    let second_claim = claim(&second).await;
    let claimed = first_claim
        .as_ref()
        .or(second_claim.as_ref())
        .expect("one live claim");
    assert_eq!(claimed.session_id, first_session);
    assert_eq!(claimed.attention_id, first_event.id);
    assert_eq!(claimed.reason, AttentionReason::SuccessfulCompletion);
    assert_eq!(claimed.session_display_name, "First completion");
    assert_eq!(claimed.preferences, disabled_native);
    assert!(first_claim.is_none() ^ second_claim.is_none());
    assert!(claim(&first).await.is_none());
    assert!(claim(&second).await.is_none());

    let second_session = create_shell_session(&first, worktree_id, "Reconnect history").await;
    let second_event = finish_shell_session(&first, second_session).await;
    let late = connect_eventually(&paths, "attention-late").await;
    assert!(claim(&late).await.is_none());
    let live_claim = claim(&first).await.expect("pre-existing client claim");
    assert_eq!(live_claim.session_id, second_session);
    assert_eq!(live_claim.attention_id, second_event.id);

    assert!(matches!(
        first
            .request(&ClientRequest::ShutdownDaemon)
            .await
            .expect("shutdown response"),
        DaemonResponse::Acknowledged
    ));
    daemon_task
        .join()
        .await
        .expect("daemon join")
        .expect("daemon shutdown");

    let restart_paths = paths.clone();
    let mut restarted = AbortOnDropTask::spawn(async move { daemon::run(restart_paths).await });
    let after_restart = connect_eventually(&paths, "attention-restarted").await;
    assert_eq!(preferences(&after_restart).await, disabled_native);
    let snapshot = match after_restart
        .request(&ClientRequest::GetSnapshot)
        .await
        .expect("snapshot response")
    {
        DaemonResponse::Snapshot(snapshot) => snapshot,
        response => panic!("unexpected snapshot response: {response:?}"),
    };
    assert!(snapshot.sessions.iter().any(|session| {
        session.id == second_session
            && session.status.attention.is_some_and(|event| {
                event.id == second_event.id
                    && event.acknowledgement == AttentionAcknowledgement::Pending
            })
    }));
    assert!(claim(&after_restart).await.is_none());
    after_restart
        .request(&ClientRequest::ShutdownDaemon)
        .await
        .expect("restart shutdown response");
    restarted
        .join()
        .await
        .expect("restarted daemon join")
        .expect("restarted daemon shutdown");
}

async fn preferences(client: &DaemonClient) -> AttentionNotificationPreferences {
    match client
        .request(&ClientRequest::GetAttentionNotificationPreferences)
        .await
        .expect("preferences response")
    {
        DaemonResponse::AttentionNotificationPreferences(preferences) => preferences,
        response => panic!("unexpected preferences response: {response:?}"),
    }
}

async fn claim(
    client: &DaemonClient,
) -> Option<sylvops_core::attention::AttentionNotificationClaim> {
    match client
        .request(&ClientRequest::ClaimAttentionNotification)
        .await
        .expect("claim response")
    {
        DaemonResponse::AttentionNotificationClaim(claim) => claim,
        response => panic!("unexpected claim response: {response:?}"),
    }
}

async fn report_focus(client: &DaemonClient, session_id: Option<sylvops_core::ids::SessionId>) {
    assert!(matches!(
        client
            .request(&ClientRequest::ReportAttentionFocus { session_id })
            .await
            .expect("attention focus response"),
        DaemonResponse::AttentionFocusUpdated
    ));
}

async fn attention(
    client: &DaemonClient,
    session_id: sylvops_core::ids::SessionId,
) -> sylvops_core::session_state::AttentionEvent {
    match client
        .request(&ClientRequest::GetSnapshot)
        .await
        .expect("snapshot response")
    {
        DaemonResponse::Snapshot(snapshot) => snapshot
            .sessions
            .into_iter()
            .find(|session| session.id == session_id)
            .and_then(|session| session.status.attention)
            .expect("Session attention"),
        response => panic!("unexpected snapshot response: {response:?}"),
    }
}

async fn create_shell_session(
    client: &DaemonClient,
    worktree_id: sylvops_core::ids::WorktreeId,
    display_name: &str,
) -> sylvops_core::ids::SessionId {
    match client
        .request(&ClientRequest::CreateSession {
            worktree_id,
            provider: ProviderKind::Shell,
            display_name: Some(display_name.into()),
            model: None,
            effort: None,
            initial_prompt: None,
            columns: 80,
            rows: 24,
        })
        .await
        .expect("session response")
    {
        DaemonResponse::SessionCreated { session, .. } => session.id,
        response => panic!("unexpected session response: {response:?}"),
    }
}

async fn finish_shell_session(
    client: &DaemonClient,
    session_id: sylvops_core::ids::SessionId,
) -> sylvops_core::session_state::AttentionEvent {
    let mut events = client.subscribe();
    assert!(matches!(
        client
            .request(&ClientRequest::AttachSession {
                session_id,
                from_sequence: 0,
                columns: 80,
                rows: 24,
            })
            .await
            .expect("attach response"),
        DaemonResponse::Attached { .. }
    ));
    client
        .request(&ClientRequest::SessionInput {
            session_id,
            bytes: exit_shell_command(),
        })
        .await
        .expect("shell exit input");
    loop {
        if let DaemonEvent::SessionStatusChanged { session, .. } =
            events.recv().await.expect("session status event")
            && session.id == session_id
            && let Some(attention) = session.status.attention
        {
            return attention;
        }
    }
}

async fn connect_eventually(paths: &RuntimePaths, name: &str) -> DaemonClient {
    loop {
        match DaemonClient::connect(paths, name).await {
            Ok(client) => return client,
            Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }
}

#[cfg(windows)]
fn exit_shell_command() -> Vec<u8> {
    b"exit\r".to_vec()
}

#[cfg(unix)]
fn exit_shell_command() -> Vec<u8> {
    b"exit\n".to_vec()
}
