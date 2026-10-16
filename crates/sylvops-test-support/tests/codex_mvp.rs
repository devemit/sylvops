#![allow(unsafe_code)]

use std::{ffi::OsString, path::Path, process::Command, time::Duration};

use rusqlite::OptionalExtension;
use sylvops_core::{
    domain::{ProviderKind, Session, SessionState},
    ids::{ProjectId, SessionId, WorkspaceId, WorktreeId},
    protocol::{ClientRequest, DaemonResponse},
};
use sylvops_daemon::{client::DaemonClient, daemon, runtime::RuntimePaths};

const PROMPT_SENTINEL: &str = "SYLVOPS_PRIVATE_PROMPT_25_7f8cbfea";
const AUTO_FINISH_PROMPT: &str = "SYLVOPS_TEST_AUTO_FINISH";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::too_many_lines)]
async fn fake_codex_hooks_attention_and_resume() {
    tokio::time::timeout(
        Duration::from_secs(120),
        fake_codex_hooks_attention_and_resume_inner(),
    )
    .await
    .expect("fake Codex scenario timeout");
}

#[allow(clippy::too_many_lines)]
async fn fake_codex_hooks_attention_and_resume_inner() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let repository = temporary.path().join("repository");
    initialize_repository(&repository);
    let bin_directory = temporary.path().join("bin");
    std::fs::create_dir(&bin_directory).expect("fake provider bin directory");
    let fake_codex = install_fake_codex(&bin_directory);
    let _codex_cli_path = EnvironmentGuard::set("CODEX_CLI_PATH", fake_codex);
    let _path =
        EnvironmentGuard::set(
            "PATH",
            std::env::join_paths(std::iter::once(bin_directory.clone()).chain(
                std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
            ))
            .expect("test PATH"),
        );
    let codex_home = temporary.path().join("codex-home");
    let _codex_home = EnvironmentGuard::set("CODEX_HOME", codex_home.clone());

    let paths = RuntimePaths::discover(Some(&temporary.path().join("state"))).expect("paths");
    let daemon_paths = paths.clone();
    let mut daemon_task = tokio::spawn(async move { daemon::run(daemon_paths).await });
    let client = tokio::select! {
        result = &mut daemon_task => panic!("daemon exited before connection: {result:?}"),
        client = connect_eventually(&paths) => client,
    };

    let workspace_id = match client
        .request(&ClientRequest::AddWorkspace {
            name: "codex-mvp".into(),
        })
        .await
        .expect("workspace response")
    {
        DaemonResponse::WorkspaceAdded { workspace, .. } => workspace.id,
        response => panic!("unexpected workspace response: {response:?}"),
    };
    let (project_id, worktree_id) = match client
        .request(&ClientRequest::AddProject {
            workspace_id,
            repository_path: repository.to_string_lossy().into_owned(),
        })
        .await
        .expect("project response")
    {
        DaemonResponse::ProjectAdded {
            project,
            root_worktree,
            ..
        } => (project.id, root_worktree.id),
        response => panic!("unexpected project response: {response:?}"),
    };
    let shell_session_id = create_shell_session(&client, worktree_id).await;
    match client
        .request(&ClientRequest::ResumeSession {
            session_id: shell_session_id,
            columns: 80,
            rows: 24,
        })
        .await
        .expect("missing resume ID response")
    {
        DaemonResponse::Error(error) => assert!(
            error
                .message
                .contains("no verified provider resume identifier"),
            "missing resume ID error should be actionable: {}",
            error.message
        ),
        response => panic!("session without resume ID unexpectedly resumed: {response:?}"),
    }
    client
        .request(&ClientRequest::StopSession {
            session_id: shell_session_id,
        })
        .await
        .expect("stop shell response");

    let terminated_session_id = create_codex_session(&client, worktree_id).await;
    assert_eq!(owned_hook_profile_count(&codex_home), 1);
    let terminated_profile =
        codex_home.join(format!("sylvops-{terminated_session_id}.config.toml"));
    let terminated_profile_source =
        std::fs::read_to_string(&terminated_profile).expect("read owned Codex profile");
    attach(&client, terminated_session_id).await;
    client
        .request(&ClientRequest::SessionInput {
            session_id: terminated_session_id,
            bytes: input_line("permission"),
        })
        .await
        .expect("terminated-session permission input");
    wait_for_session(&client, terminated_session_id, |session| {
        session.state == SessionState::NeedsFeedback && session.external_session_id.is_some()
    })
    .await;
    std::fs::write(
        &terminated_profile,
        with_codex_trust(&terminated_profile_source, &repository),
    )
    .expect("Codex adds trust before process-exit cleanup");
    client
        .request(&ClientRequest::StopSession {
            session_id: terminated_session_id,
        })
        .await
        .expect("terminate response");
    wait_for_session(&client, terminated_session_id, |session| {
        session.state == SessionState::Terminated
    })
    .await;
    assert_eq!(owned_hook_profile_count(&codex_home), 0);
    let stopped_successor = match client
        .request(&ClientRequest::ResumeSession {
            session_id: terminated_session_id,
            columns: 80,
            rows: 24,
        })
        .await
        .expect("terminated resume response")
    {
        DaemonResponse::SessionResumed { session, .. } => session,
        response => panic!("stopped conversation did not resume: {response:?}"),
    };
    assert_eq!(stopped_successor.worktree_id, worktree_id);
    client
        .request(&ClientRequest::StopSession {
            session_id: stopped_successor.id,
        })
        .await
        .expect("stop resumed fixture");
    wait_for_session(&client, stopped_successor.id, |session| {
        session.state == SessionState::Terminated
    })
    .await;
    assert_eq!(owned_hook_profile_count(&codex_home), 0);
    std::fs::write(
        &terminated_profile,
        with_codex_trust(&terminated_profile_source, &repository),
    )
    .expect("restore owned Codex profile with current trust metadata");
    assert_eq!(owned_hook_profile_count(&codex_home), 1);
    let authorization_token = match client
        .request(&ClientRequest::InspectSessionDeletion {
            session_id: terminated_session_id,
        })
        .await
        .expect("Codex deletion inspection")
    {
        DaemonResponse::SessionDeletionInspected(inspection) => inspection
            .authorization_token
            .expect("terminated Codex deletion token"),
        response => panic!("unexpected Codex deletion inspection: {response:?}"),
    };
    assert!(matches!(
        client
            .request(&ClientRequest::DeleteSession {
                session_id: terminated_session_id,
                authorization_token,
            })
            .await
            .expect("Codex deletion response"),
        DaemonResponse::SessionDeleted { session_id, .. }
            if session_id == terminated_session_id
    ));
    assert_eq!(owned_hook_profile_count(&codex_home), 0);

    let source_session_id = create_codex_session(&client, worktree_id).await;
    attach(&client, source_session_id).await;
    client
        .request(&ClientRequest::SessionInput {
            session_id: source_session_id,
            bytes: input_line("permission"),
        })
        .await
        .expect("permission input");
    let waiting = wait_for_session(&client, source_session_id, |session| {
        session.state == SessionState::NeedsFeedback && session.external_session_id.is_some()
    })
    .await;
    let external_id = waiting.external_session_id.expect("external Codex ID");
    let source_profile = codex_home.join(format!("sylvops-{source_session_id}.config.toml"));
    let source_profile_text = std::fs::read_to_string(&source_profile).expect("source profile");
    std::fs::write(
        &source_profile,
        with_codex_trust(&source_profile_text, &repository),
    )
    .expect("Codex adds trust before successful exit");

    client
        .request(&ClientRequest::SessionInput {
            session_id: source_session_id,
            bytes: input_line("finish"),
        })
        .await
        .expect("finish input");
    wait_for_session(&client, source_session_id, |session| {
        session.process_id.is_none()
            && matches!(
                session.state,
                SessionState::FinishedUnseen | SessionState::FinishedSeen
            )
    })
    .await;
    assert_eq!(owned_hook_profile_count(&codex_home), 0);

    let first_resume = ClientRequest::ResumeSession {
        session_id: source_session_id,
        columns: 80,
        rows: 24,
    };
    let concurrent_resume = first_resume.clone();
    let (first_response, concurrent_response) = tokio::join!(
        client.request(&first_resume),
        client.request(&concurrent_resume)
    );
    let mut resumed_id = None;
    let mut refused = 0;
    for response in [first_response, concurrent_response] {
        match response.expect("concurrent resume response") {
            DaemonResponse::SessionResumed { session, .. } => {
                assert_eq!(
                    session.external_session_id.as_deref(),
                    Some(external_id.as_str())
                );
                assert!(resumed_id.replace(session.id).is_none());
            }
            DaemonResponse::Error(error) => {
                assert!(
                    error.message.contains("already been resumed"),
                    "concurrent resume error should be actionable: {}",
                    error.message
                );
                refused += 1;
            }
            response => panic!("unexpected concurrent resume response: {response:?}"),
        }
    }
    assert_eq!(refused, 1);
    let resumed_id = resumed_id.expect("one concurrent resume succeeds");
    match client
        .request(&ClientRequest::ResumeSession {
            session_id: source_session_id,
            columns: 80,
            rows: 24,
        })
        .await
        .expect("stale resume response")
    {
        DaemonResponse::Error(error) => assert!(
            error.message.contains("already been resumed"),
            "stale resume error should be actionable: {}",
            error.message
        ),
        response => panic!("stale resume unexpectedly succeeded: {response:?}"),
    }
    let history = match client
        .request(&ClientRequest::GetSnapshot)
        .await
        .expect("history snapshot response")
    {
        DaemonResponse::Snapshot(snapshot) => snapshot,
        response => panic!("unexpected history snapshot response: {response:?}"),
    };
    let source_history = history
        .sessions
        .iter()
        .find(|session| session.id == source_session_id)
        .expect("historical source session");
    assert!(matches!(
        source_history.state,
        SessionState::FinishedUnseen | SessionState::FinishedSeen
    ));
    assert_eq!(
        source_history.external_session_id.as_deref(),
        Some(external_id.as_str())
    );
    assert!(
        history
            .sessions
            .iter()
            .any(|session| session.id == resumed_id)
    );
    attach(&client, resumed_id).await;
    client
        .request(&ClientRequest::SessionInput {
            session_id: resumed_id,
            bytes: input_line("finish"),
        })
        .await
        .expect("resumed finish input");
    wait_for_session(&client, resumed_id, |session| session.process_id.is_none()).await;
    assert_eq!(owned_hook_profile_count(&codex_home), 0);

    exercise_codex_cleanup_refusal(&client, worktree_id, &codex_home, &repository).await;

    let cleanup_session_id = create_codex_session(&client, worktree_id).await;
    wait_for_session(&client, cleanup_session_id, |session| {
        session.state == SessionState::Running && session.process_id.is_some()
    })
    .await;
    assert_eq!(owned_hook_profile_count(&codex_home), 1);

    client
        .request(&ClientRequest::ShutdownDaemon)
        .await
        .expect("shutdown response");
    tokio::time::timeout(Duration::from_secs(10), daemon_task)
        .await
        .expect("daemon shutdown timeout")
        .expect("daemon task")
        .expect("daemon result");
    assert_eq!(owned_hook_profile_count(&codex_home), 0);

    let stale_profile = codex_home.join("sylvops-stale.config.toml");
    std::fs::write(
        &stale_profile,
        with_codex_trust(&terminated_profile_source, &repository),
    )
    .expect("legacy v1 profile left by an affected version");
    let unrelated = codex_home.join("config.toml");
    let unknown_profile = codex_home.join("sylvops-user.config.toml");
    let unrelated_source = "# user configuration\n";
    std::fs::write(&unrelated, unrelated_source).expect("user configuration");
    std::fs::write(&unknown_profile, unrelated_source).expect("unowned profile");

    let restart_paths = paths.clone();
    let mut restart_task = tokio::spawn(async move { daemon::run(restart_paths).await });
    let restarted = tokio::select! {
        result = &mut restart_task => panic!("restarted daemon exited before connection: {result:?}"),
        client = connect_eventually(&paths) => client,
    };
    assert!(
        !stale_profile.exists(),
        "startup removes provably owned stale profiles"
    );
    for path in [&unrelated, &unknown_profile] {
        assert_eq!(std::fs::read_to_string(path).unwrap(), unrelated_source);
    }
    let snapshot = match restarted
        .request(&ClientRequest::GetSnapshot)
        .await
        .expect("restart snapshot response")
    {
        DaemonResponse::Snapshot(snapshot) => snapshot,
        response => panic!("unexpected restart response: {response:?}"),
    };
    assert!(snapshot.sessions.iter().any(|session| {
        session.id == source_session_id
            && session.external_session_id.as_deref() == Some(external_id.as_str())
    }));
    match restarted
        .request(&ClientRequest::ResumeSession {
            session_id: source_session_id,
            columns: 80,
            rows: 24,
        })
        .await
        .expect("restart stale resume response")
    {
        DaemonResponse::Error(error) => assert!(error.message.contains("already been resumed")),
        response => panic!("restart stale resume unexpectedly succeeded: {response:?}"),
    }
    exercise_session_lifecycle_stale_authorization(&restarted, worktree_id).await;
    delete_all_sessions(&restarted).await;
    let managed_worktree_id = match restarted
        .request(&ClientRequest::CreateWorktree {
            project_id,
            name: Some("fake-provider-removal".into()),
            branch: "feature/fake-provider-removal".into(),
            base_ref: Some("HEAD".into()),
        })
        .await
        .expect("managed Worktree response")
    {
        DaemonResponse::WorktreeCreated { worktree, .. } => worktree.id,
        response => panic!("unexpected managed Worktree response: {response:?}"),
    };
    remove_worktree(&restarted, managed_worktree_id).await;
    remove_project(&restarted, project_id).await;
    delete_workspace(&restarted, workspace_id).await;
    let removed = match restarted
        .request(&ClientRequest::GetSnapshot)
        .await
        .expect("removed hierarchy snapshot")
    {
        DaemonResponse::Snapshot(snapshot) => snapshot,
        response => panic!("unexpected removed hierarchy response: {response:?}"),
    };
    assert_eq!(removed.sessions.as_slice(), &[]);
    assert_eq!(removed.projects.as_slice(), &[]);
    assert_eq!(removed.workspaces.as_slice(), &[]);
    restarted
        .request(&ClientRequest::ShutdownDaemon)
        .await
        .expect("restart shutdown response");
    tokio::time::timeout(Duration::from_secs(10), restart_task)
        .await
        .expect("restart daemon shutdown timeout")
        .expect("restart daemon task")
        .expect("restart daemon result");
    assert_no_persisted_text_contains(&paths.database, PROMPT_SENTINEL);
}

fn with_codex_trust(source: &str, project: &Path) -> String {
    let project = serde_json::to_string(&project.to_string_lossy()).expect("quoted Project path");
    format!("{source}\n[projects.{project}]\ntrust_level = \"trusted\"\n")
}

async fn exercise_codex_cleanup_refusal(
    client: &DaemonClient,
    worktree_id: WorktreeId,
    codex_home: &Path,
    project: &Path,
) {
    let session_id = create_codex_session(client, worktree_id).await;
    attach(client, session_id).await;
    wait_for_session(client, session_id, |session| {
        session.external_session_id.is_some()
    })
    .await;
    let profile = codex_home.join(format!("sylvops-{session_id}.config.toml"));
    let original = std::fs::read_to_string(&profile).expect("owned profile");
    let changed = format!("{original}\n[notice]\nprivate_unknown_edit = true\n");
    std::fs::write(&profile, &changed).expect("unknown Codex profile edit");
    client
        .request(&ClientRequest::SessionInput {
            session_id,
            bytes: input_line("finish"),
        })
        .await
        .expect("finish despite cleanup refusal");
    let finished =
        wait_for_session(client, session_id, |session| session.process_id.is_none()).await;
    assert!(matches!(
        finished.state,
        SessionState::FinishedSeen | SessionState::FinishedUnseen
    ));
    assert_eq!(std::fs::read_to_string(&profile).unwrap(), changed);
    let authorization_token = session_deletion_token(client, session_id).await;
    match client
        .request(&ClientRequest::DeleteSession {
            session_id,
            authorization_token,
        })
        .await
        .expect("unknown mutation deletion refusal")
    {
        DaemonResponse::Error(error) => {
            assert!(error.message.len() < 512);
            assert!(!error.message.contains("private_unknown_edit"));
        }
        other => panic!("unknown mutation must refuse deletion: {other:?}"),
    }
    assert_eq!(std::fs::read_to_string(&profile).unwrap(), changed);
    std::fs::write(&profile, with_codex_trust(&original, project)).expect("repair unknown edit");
    let authorization_token = session_deletion_token(client, session_id).await;
    assert!(matches!(
        client
            .request(&ClientRequest::DeleteSession {
                session_id,
                authorization_token
            })
            .await
            .unwrap(),
        DaemonResponse::SessionDeleted { .. }
    ));
    assert!(!profile.exists());
}

async fn exercise_session_lifecycle_stale_authorization(
    client: &DaemonClient,
    worktree_id: WorktreeId,
) {
    let session_id = match client
        .request(&ClientRequest::CreateSession {
            worktree_id,
            provider: ProviderKind::Codex,
            display_name: Some("auto-finishing fake Codex".into()),
            model: Some("fake-model".into()),
            effort: Some("high".into()),
            initial_prompt: Some(AUTO_FINISH_PROMPT.into()),
            columns: 80,
            rows: 24,
        })
        .await
        .expect("auto-finishing Session response")
    {
        DaemonResponse::SessionCreated { session, .. } => session.id,
        response => panic!("unexpected auto-finishing Session response: {response:?}"),
    };
    wait_for_session(client, session_id, |session| {
        session.state == SessionState::FinishedUnseen && session.process_id.is_none()
    })
    .await;
    let stale_token = session_deletion_token(client, session_id).await;
    attach(client, session_id).await;
    wait_for_session(client, session_id, |session| {
        session.state == SessionState::FinishedSeen
    })
    .await;
    assert!(matches!(
        client
            .request(&ClientRequest::DeleteSession {
                session_id,
                authorization_token: stale_token,
            })
            .await
            .expect("stale lifecycle deletion response"),
        DaemonResponse::Error(error) if error.code == "session_request_refused"
    ));
    let preserved = wait_for_session(client, session_id, |session| {
        session.state == SessionState::FinishedSeen
    })
    .await;
    assert_eq!(preserved.id, session_id);
    let authorization_token = session_deletion_token(client, session_id).await;
    assert!(matches!(
        client
            .request(&ClientRequest::DeleteSession {
                session_id,
                authorization_token,
            })
            .await
            .expect("fresh lifecycle deletion response"),
        DaemonResponse::SessionDeleted { session_id: deleted, .. } if deleted == session_id
    ));
}

async fn session_deletion_token(
    client: &DaemonClient,
    session_id: SessionId,
) -> sylvops_core::protocol::SessionDeletionAuthorization {
    match client
        .request(&ClientRequest::InspectSessionDeletion { session_id })
        .await
        .expect("Session deletion inspection")
    {
        DaemonResponse::SessionDeletionInspected(inspection) => inspection
            .authorization_token
            .expect("inactive fake-provider Session deletion token"),
        response => panic!("unexpected Session deletion inspection: {response:?}"),
    }
}

async fn delete_all_sessions(client: &DaemonClient) {
    let sessions = match client
        .request(&ClientRequest::GetSnapshot)
        .await
        .expect("Session cleanup snapshot")
    {
        DaemonResponse::Snapshot(snapshot) => snapshot.sessions,
        response => panic!("unexpected Session cleanup response: {response:?}"),
    };
    for session in sessions {
        let authorization_token = session_deletion_token(client, session.id).await;
        assert!(matches!(
            client
                .request(&ClientRequest::DeleteSession {
                    session_id: session.id,
                    authorization_token,
                })
                .await
                .expect("Session deletion response"),
            DaemonResponse::SessionDeleted { .. }
        ));
    }
}

async fn remove_worktree(client: &DaemonClient, worktree_id: WorktreeId) {
    let confirmation_token = match client
        .request(&ClientRequest::GetWorktreeStatus { worktree_id })
        .await
        .expect("Worktree removal inspection")
    {
        DaemonResponse::WorktreeStatus(status) => status
            .removal_confirmation_token
            .expect("clean fake-provider Worktree token"),
        response => panic!("unexpected Worktree removal inspection: {response:?}"),
    };
    assert!(matches!(
        client
            .request(&ClientRequest::RemoveWorktree {
                worktree_id,
                confirmation_token,
            })
            .await
            .expect("Worktree removal response"),
        DaemonResponse::WorktreeRemoved { .. }
    ));
}

async fn remove_project(client: &DaemonClient, project_id: ProjectId) {
    let authorization_token = match client
        .request(&ClientRequest::InspectProjectRemoval { project_id })
        .await
        .expect("Project removal inspection")
    {
        DaemonResponse::ProjectRemovalInspected(inspection) => inspection
            .authorization_token
            .expect("cleared fake-provider Project token"),
        response => panic!("unexpected Project removal inspection: {response:?}"),
    };
    assert!(matches!(
        client
            .request(&ClientRequest::RemoveProject {
                project_id,
                authorization_token,
            })
            .await
            .expect("Project removal response"),
        DaemonResponse::ProjectRemoved { .. }
    ));
}

async fn delete_workspace(client: &DaemonClient, workspace_id: WorkspaceId) {
    let authorization_token = match client
        .request(&ClientRequest::InspectWorkspaceDeletion { workspace_id })
        .await
        .expect("Workspace deletion inspection")
    {
        DaemonResponse::WorkspaceDeletionInspected(inspection) => inspection
            .authorization_token
            .expect("empty fake-provider Workspace token"),
        response => panic!("unexpected Workspace deletion inspection: {response:?}"),
    };
    assert!(matches!(
        client
            .request(&ClientRequest::DeleteWorkspace {
                workspace_id,
                authorization_token,
            })
            .await
            .expect("Workspace deletion response"),
        DaemonResponse::WorkspaceDeleted { .. }
    ));
}

fn owned_hook_profile_count(codex_home: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(codex_home) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with("sylvops-") && name.ends_with(".config.toml")
        })
        .count()
}

async fn create_codex_session(client: &DaemonClient, worktree_id: WorktreeId) -> SessionId {
    match client
        .request(&ClientRequest::CreateSession {
            worktree_id,
            provider: ProviderKind::Codex,
            display_name: Some("fake Codex".into()),
            model: Some("fake-model".into()),
            effort: Some("high".into()),
            initial_prompt: Some(PROMPT_SENTINEL.into()),
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

async fn create_shell_session(client: &DaemonClient, worktree_id: WorktreeId) -> SessionId {
    match client
        .request(&ClientRequest::CreateSession {
            worktree_id,
            provider: ProviderKind::Shell,
            display_name: Some("plain shell".into()),
            model: None,
            effort: None,
            initial_prompt: None,
            columns: 80,
            rows: 24,
        })
        .await
        .expect("shell session response")
    {
        DaemonResponse::SessionCreated { session, .. } => session.id,
        response => panic!("unexpected shell session response: {response:?}"),
    }
}

fn assert_no_persisted_text_contains(path: &Path, sentinel: &str) {
    let connection = rusqlite::Connection::open(path).expect("open persisted database");
    let tables = connection
        .prepare(
            "SELECT name FROM sqlite_schema \
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .expect("prepare table query")
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query tables")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("collect tables");

    for table in tables {
        let quoted_table = quote_identifier(&table);
        let columns = connection
            .prepare(&format!("PRAGMA table_info({quoted_table})"))
            .expect("prepare column query")
            .query_map([], |row| row.get::<_, String>(1))
            .expect("query columns")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("collect columns");
        for column in columns {
            let quoted_column = quote_identifier(&column);
            let query = format!(
                "SELECT {quoted_column} FROM {quoted_table} \
                 WHERE typeof({quoted_column}) = 'text' AND instr({quoted_column}, ?1) > 0"
            );
            let found = connection
                .query_row(&query, [sentinel], |row| row.get::<_, String>(0))
                .optional()
                .expect("scan persisted text");
            assert!(
                found.is_none(),
                "prompt sentinel persisted in {table}.{column}"
            );
        }
    }
}

fn quote_identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

async fn attach(client: &DaemonClient, session_id: SessionId) {
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
}

async fn wait_for_session(
    client: &DaemonClient,
    session_id: SessionId,
    predicate: impl Fn(&Session) -> bool,
) -> Session {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut last_observed = None;
    loop {
        if let DaemonResponse::Snapshot(snapshot) = client
            .request(&ClientRequest::GetSnapshot)
            .await
            .expect("snapshot response")
            && let Some(session) = snapshot
                .sessions
                .into_iter()
                .find(|session| session.id == session_id)
        {
            if predicate(&session) {
                return session;
            }
            last_observed = Some(session);
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "session state timeout; last observed: {last_observed:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn connect_eventually(paths: &RuntimePaths) -> DaemonClient {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(client) = DaemonClient::connect(paths, "fake-codex-test").await {
                return client;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("daemon connection timeout")
}

fn install_fake_codex(directory: &Path) -> std::path::PathBuf {
    let target = directory.join(if cfg!(windows) { "codex.exe" } else { "codex" });
    let source = std::env::var_os("SYLVOPS_TEST_FAKE_CODEX").map_or_else(
        || std::path::PathBuf::from(env!("CARGO_BIN_EXE_fake-codex")),
        std::path::PathBuf::from,
    );
    std::fs::copy(source, &target).expect("copy fake Codex");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&target)
            .expect("fake metadata")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&target, permissions).expect("fake executable permissions");
    }
    target
}

#[cfg(windows)]
fn input_line(value: &str) -> Vec<u8> {
    format!("{value}\r").into_bytes()
}

#[cfg(unix)]
fn input_line(value: &str) -> Vec<u8> {
    format!("{value}\n").into_bytes()
}

fn initialize_repository(path: &Path) {
    std::fs::create_dir(path).expect("repository directory");
    run_git(path, &["init", "--initial-branch=main"]);
    run_git(path, &["config", "user.name", "SylvOps Test"]);
    run_git(path, &["config", "user.email", "sylvops@example.invalid"]);
    std::fs::write(path.join("README.md"), "fixture\n").expect("fixture file");
    run_git(path, &["add", "README.md"]);
    run_git(path, &["commit", "-m", "fixture"]);
}

fn run_git(path: &Path, arguments: &[&str]) {
    let status = Command::new("git")
        .args(arguments)
        .current_dir(path)
        .status()
        .expect("run git");
    assert!(status.success(), "git {arguments:?} failed");
}

struct EnvironmentGuard {
    name: &'static str,
    previous: Option<OsString>,
}

impl EnvironmentGuard {
    fn set(name: &'static str, value: impl Into<OsString>) -> Self {
        let previous = std::env::var_os(name);
        // SAFETY: this test serializes all environment changes in its process and restores them.
        unsafe { std::env::set_var(name, value.into()) };
        Self { name, previous }
    }
}

impl Drop for EnvironmentGuard {
    fn drop(&mut self) {
        // SAFETY: this test serializes all environment changes in its process and restores them.
        unsafe {
            if let Some(previous) = self.previous.take() {
                std::env::set_var(self.name, previous);
            } else {
                std::env::remove_var(self.name);
            }
        }
    }
}
