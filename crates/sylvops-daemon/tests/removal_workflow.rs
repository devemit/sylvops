use std::{path::Path, time::Duration};

use rusqlite::Connection;
use sylvops_core::{
    domain::{ProviderKind, WorktreeStatus},
    ids::{ProjectId, SessionId, WorkspaceId, WorktreeId},
    protocol::{ClientRequest, DaemonEvent, DaemonResponse},
    session_state::SessionLifecycle,
};
use sylvops_daemon::{client::DaemonClient, daemon, runtime::RuntimePaths};
use sylvops_test_support::{AbortOnDropTask, TemporaryRepository, git_branch_exists};

const PROMPT_SENTINEL: &str = "AUDIT_PRIVATE_PROMPT_97";
const TERMINAL_SENTINEL: &str = "AUDIT_PRIVATE_TERMINAL_97";
const ENVIRONMENT_SENTINEL: &str = "AUDIT_PRIVATE_ENVIRONMENT_97";
const REMOTE_SENTINEL: &str = "AUDIT_PRIVATE_REMOTE_97";
const CREDENTIAL_SENTINEL: &str = "AUDIT_PRIVATE_CREDENTIAL_97";
const MAX_SENSITIVE_OUTPUT_BYTES: usize = 16 * 1024;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn authenticated_clients_converge_through_the_complete_leaf_first_removal_workflow() {
    tokio::time::timeout(
        Duration::from_secs(60),
        exercise_complete_removal_workflow(),
    )
    .await
    .expect("complete removal workflow timeout");
}

#[allow(clippy::too_many_lines)]
async fn exercise_complete_removal_workflow() {
    let repository = TemporaryRepository::initialize().await.unwrap();
    let competing_repository = TemporaryRepository::initialize().await.unwrap();
    let repository_sentinel = repository.path().join("repository-must-survive.txt");
    std::fs::write(
        &repository_sentinel,
        "repository-secret-must-not-enter-audit\n",
    )
    .unwrap();
    add_sensitive_remote(repository.path()).await;
    let state_directory = repository.root().join("state");
    let paths = RuntimePaths::discover(Some(&state_directory)).unwrap();
    let daemon_paths = paths.clone();
    let mut daemon_task = AbortOnDropTask::spawn(async move { daemon::run(daemon_paths).await });
    let actor = connect_eventually(&paths, "removal-actor").await;
    let observer = connect_eventually(&paths, "removal-observer").await;
    let mut events = observer.subscribe();

    let fallback = add_workspace(&actor, "Fallback private name").await;
    let target = add_workspace(&actor, "Target private name").await;
    let (project, root_worktree) = add_project(&actor, target, repository.path()).await;
    let managed_worktree = create_worktree(&actor, project.id).await;
    let managed_path = Path::new(&managed_worktree.canonical_path).to_path_buf();
    let session = create_session(&actor, managed_worktree.id, "Private prompt-like name").await;
    let active_inspection = inspect_session(&actor, session.id).await;
    assert!(active_inspection.authorization_token.is_none());
    let mut output_events = actor.subscribe();
    actor
        .request(&ClientRequest::AttachSession {
            session_id: session.id,
            from_sequence: 0,
            columns: 80,
            rows: 24,
        })
        .await
        .unwrap();
    actor
        .request(&ClientRequest::SessionInput {
            session_id: session.id,
            bytes: sensitive_shell_input(),
        })
        .await
        .unwrap();
    wait_for_sensitive_output(&mut output_events, session.id).await;
    actor
        .request(&ClientRequest::DetachSession {
            session_id: session.id,
        })
        .await
        .unwrap();
    actor
        .request(&ClientRequest::StopSession {
            session_id: session.id,
        })
        .await
        .unwrap();
    wait_for_session_state(&observer, session.id, SessionLifecycle::Terminated).await;
    let stale_session_token = inspect_session(&actor, session.id)
        .await
        .authorization_token
        .unwrap();
    actor
        .request(&ClientRequest::RenameSession {
            session_id: session.id,
            name: "Changed private Session name".into(),
        })
        .await
        .unwrap();
    assert_refused(
        actor
            .request(&ClientRequest::DeleteSession {
                session_id: session.id,
                authorization_token: stale_session_token,
            })
            .await
            .unwrap(),
        "session_request_refused",
    );
    assert!(
        snapshot(&observer)
            .await
            .sessions
            .iter()
            .any(|item| item.id == session.id)
    );
    delete_session(&actor, session.id).await;
    let session_revision = wait_for_session_deleted(&mut events, session.id).await;
    assert!(
        !snapshot(&observer)
            .await
            .sessions
            .iter()
            .any(|item| item.id == session.id)
    );

    let stale_worktree_token = worktree_token(&actor, managed_worktree.id).await;
    let dirty_file = managed_path.join("changed-after-inspection.txt");
    std::fs::write(&dirty_file, "dirty content must survive refusal\n").unwrap();
    assert_refused(
        actor
            .request(&ClientRequest::RemoveWorktree {
                worktree_id: managed_worktree.id,
                confirmation_token: stale_worktree_token,
            })
            .await
            .unwrap(),
        "git_validation_failed",
    );
    assert!(managed_path.exists());
    assert!(
        snapshot(&observer)
            .await
            .worktrees
            .iter()
            .any(|item| item.id == managed_worktree.id && item.status == WorktreeStatus::Active)
    );
    std::fs::remove_file(&dirty_file).unwrap();
    let worktree_token = worktree_token(&actor, managed_worktree.id).await;
    actor
        .request(&ClientRequest::RemoveWorktree {
            worktree_id: managed_worktree.id,
            confirmation_token: worktree_token,
        })
        .await
        .unwrap();
    let worktree_revision = wait_for_worktree_removed(&mut events, managed_worktree.id).await;
    assert!(worktree_revision > session_revision);
    assert!(!managed_path.exists());
    assert!(
        git_branch_exists(repository.path(), "feature/complete-removal")
            .await
            .unwrap()
    );

    let stale_project_token = inspect_project(&actor, project.id)
        .await
        .authorization_token
        .unwrap();
    let blocking_session =
        create_session(&actor, root_worktree.id, "Concurrent private name").await;
    assert_refused(
        actor
            .request(&ClientRequest::RemoveProject {
                project_id: project.id,
                authorization_token: stale_project_token,
            })
            .await
            .unwrap(),
        "project_request_refused",
    );
    actor
        .request(&ClientRequest::StopSession {
            session_id: blocking_session.id,
        })
        .await
        .unwrap();
    wait_for_session_state(&observer, blocking_session.id, SessionLifecycle::Terminated).await;
    delete_session(&actor, blocking_session.id).await;
    let project_token = inspect_project(&actor, project.id)
        .await
        .authorization_token
        .unwrap();
    actor
        .request(&ClientRequest::RemoveProject {
            project_id: project.id,
            authorization_token: project_token,
        })
        .await
        .unwrap();
    let project_revision = wait_for_project_removed(&mut events, project.id).await;
    assert!(project_revision > worktree_revision);
    let after_project = snapshot(&observer).await;
    assert!(
        !after_project
            .projects
            .iter()
            .any(|item| item.id == project.id)
    );
    assert!(
        !after_project
            .worktrees
            .iter()
            .any(|item| item.project_id == project.id)
    );
    assert!(repository.path().join(".git").exists());
    assert_eq!(
        std::fs::read_to_string(&repository_sentinel).unwrap(),
        "repository-secret-must-not-enter-audit\n"
    );

    let stale_workspace_token = inspect_workspace(&actor, target)
        .await
        .authorization_token
        .unwrap();
    let (competing_project, _) = add_project(&actor, target, competing_repository.path()).await;
    assert_refused(
        actor
            .request(&ClientRequest::DeleteWorkspace {
                workspace_id: target,
                authorization_token: stale_workspace_token,
            })
            .await
            .unwrap(),
        "workspace_request_refused",
    );
    let competing_token = inspect_project(&actor, competing_project.id)
        .await
        .authorization_token
        .unwrap();
    actor
        .request(&ClientRequest::RemoveProject {
            project_id: competing_project.id,
            authorization_token: competing_token,
        })
        .await
        .unwrap();
    let workspace_token = inspect_workspace(&actor, target)
        .await
        .authorization_token
        .unwrap();
    actor
        .request(&ClientRequest::DeleteWorkspace {
            workspace_id: target,
            authorization_token: workspace_token,
        })
        .await
        .unwrap();
    let workspace_revision = wait_for_workspace_deleted(&mut events, target, fallback).await;
    assert!(workspace_revision > project_revision);
    let final_snapshot = snapshot(&observer).await;
    assert!(
        !final_snapshot
            .workspaces
            .iter()
            .any(|item| item.id == target)
    );
    assert!(
        final_snapshot
            .workspaces
            .iter()
            .any(|item| item.id == fallback && item.is_open)
    );
    assert!(repository.path().join(".git").exists());
    assert!(competing_repository.path().join(".git").exists());

    actor.request(&ClientRequest::ShutdownDaemon).await.unwrap();
    daemon_task.join().await.unwrap().unwrap();
    let repository_path = repository.path().to_string_lossy().into_owned();
    assert_content_free_removal_audits(
        &paths.database,
        &[
            "Fallback private name",
            "Target private name",
            "Private managed Worktree name",
            "Private prompt-like name",
            "Changed private Session name",
            PROMPT_SENTINEL,
            TERMINAL_SENTINEL,
            ENVIRONMENT_SENTINEL,
            REMOTE_SENTINEL,
            CREDENTIAL_SENTINEL,
            &repository_path,
        ],
    );
}

async fn add_workspace(client: &DaemonClient, name: &str) -> WorkspaceId {
    match client
        .request(&ClientRequest::AddWorkspace { name: name.into() })
        .await
        .unwrap()
    {
        DaemonResponse::WorkspaceAdded { workspace, .. } => workspace.id,
        other => panic!("unexpected Workspace response: {other:?}"),
    }
}

async fn add_project(
    client: &DaemonClient,
    workspace_id: WorkspaceId,
    repository: &Path,
) -> (
    sylvops_core::domain::Project,
    sylvops_core::domain::Worktree,
) {
    match client
        .request(&ClientRequest::AddProject {
            workspace_id,
            repository_path: repository.to_string_lossy().into_owned(),
        })
        .await
        .unwrap()
    {
        DaemonResponse::ProjectAdded {
            project,
            root_worktree,
            ..
        } => (project, root_worktree),
        other => panic!("unexpected Project response: {other:?}"),
    }
}

async fn create_worktree(
    client: &DaemonClient,
    project_id: ProjectId,
) -> sylvops_core::domain::Worktree {
    match client
        .request(&ClientRequest::CreateWorktree {
            project_id,
            name: Some("Private managed Worktree name".into()),
            branch: "feature/complete-removal".into(),
            base_ref: Some("HEAD".into()),
        })
        .await
        .unwrap()
    {
        DaemonResponse::WorktreeCreated { worktree, .. } => worktree,
        other => panic!("unexpected Worktree response: {other:?}"),
    }
}

async fn create_session(
    client: &DaemonClient,
    worktree_id: WorktreeId,
    display_name: &str,
) -> sylvops_core::domain::Session {
    match client
        .request(&ClientRequest::CreateSession {
            worktree_id,
            provider: ProviderKind::Shell,
            display_name: Some(display_name.into()),
            model: None,
            effort: None,
            initial_prompt: Some(PROMPT_SENTINEL.into()),
            columns: 80,
            rows: 24,
        })
        .await
        .unwrap()
    {
        DaemonResponse::SessionCreated { session, .. } => session,
        other => panic!("unexpected Session response: {other:?}"),
    }
}

async fn inspect_session(
    client: &DaemonClient,
    session_id: SessionId,
) -> sylvops_core::protocol::SessionDeletionInspection {
    match client
        .request(&ClientRequest::InspectSessionDeletion { session_id })
        .await
        .unwrap()
    {
        DaemonResponse::SessionDeletionInspected(inspection) => inspection,
        other => panic!("unexpected Session inspection: {other:?}"),
    }
}

async fn delete_session(client: &DaemonClient, session_id: SessionId) {
    let authorization_token = inspect_session(client, session_id)
        .await
        .authorization_token
        .unwrap();
    assert!(matches!(
        client
            .request(&ClientRequest::DeleteSession {
                session_id,
                authorization_token,
            })
            .await
            .unwrap(),
        DaemonResponse::SessionDeleted { .. }
    ));
}

async fn inspect_project(
    client: &DaemonClient,
    project_id: ProjectId,
) -> sylvops_core::protocol::ProjectRemovalInspection {
    match client
        .request(&ClientRequest::InspectProjectRemoval { project_id })
        .await
        .unwrap()
    {
        DaemonResponse::ProjectRemovalInspected(inspection) => inspection,
        other => panic!("unexpected Project inspection: {other:?}"),
    }
}

async fn inspect_workspace(
    client: &DaemonClient,
    workspace_id: WorkspaceId,
) -> sylvops_core::protocol::WorkspaceDeletionInspection {
    match client
        .request(&ClientRequest::InspectWorkspaceDeletion { workspace_id })
        .await
        .unwrap()
    {
        DaemonResponse::WorkspaceDeletionInspected(inspection) => inspection,
        other => panic!("unexpected Workspace inspection: {other:?}"),
    }
}

async fn worktree_token(client: &DaemonClient, worktree_id: WorktreeId) -> String {
    match client
        .request(&ClientRequest::GetWorktreeStatus { worktree_id })
        .await
        .unwrap()
    {
        DaemonResponse::WorktreeStatus(status) => status.removal_confirmation_token.unwrap(),
        other => panic!("unexpected Worktree inspection: {other:?}"),
    }
}

async fn snapshot(client: &DaemonClient) -> sylvops_core::domain::DaemonSnapshot {
    match client.request(&ClientRequest::GetSnapshot).await.unwrap() {
        DaemonResponse::Snapshot(snapshot) => snapshot,
        other => panic!("unexpected snapshot response: {other:?}"),
    }
}

async fn wait_for_session_state(
    client: &DaemonClient,
    session_id: SessionId,
    state: SessionLifecycle,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if snapshot(client)
            .await
            .sessions
            .iter()
            .any(|item| item.id == session_id && item.status.lifecycle == state)
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "Session state wait timed out"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn assert_refused(response: DaemonResponse, code: &str) {
    assert!(matches!(response, DaemonResponse::Error(failure) if failure.code == code));
}

async fn wait_for_session_deleted(
    events: &mut tokio::sync::broadcast::Receiver<DaemonEvent>,
    expected: SessionId,
) -> u64 {
    loop {
        if let DaemonEvent::SessionDeleted {
            revision,
            session_id,
            ..
        } = events.recv().await.unwrap()
            && session_id == expected
        {
            return revision;
        }
    }
}

async fn wait_for_worktree_removed(
    events: &mut tokio::sync::broadcast::Receiver<DaemonEvent>,
    expected: WorktreeId,
) -> u64 {
    loop {
        if let DaemonEvent::WorktreeRemoved { revision, worktree } = events.recv().await.unwrap()
            && worktree.id == expected
        {
            return revision;
        }
    }
}

async fn wait_for_project_removed(
    events: &mut tokio::sync::broadcast::Receiver<DaemonEvent>,
    expected: ProjectId,
) -> u64 {
    loop {
        if let DaemonEvent::ProjectRemoved {
            revision,
            project_id,
            ..
        } = events.recv().await.unwrap()
            && project_id == expected
        {
            return revision;
        }
    }
}

async fn wait_for_workspace_deleted(
    events: &mut tokio::sync::broadcast::Receiver<DaemonEvent>,
    expected: WorkspaceId,
    fallback: WorkspaceId,
) -> u64 {
    loop {
        if let DaemonEvent::WorkspaceDeleted {
            revision,
            workspace_id,
            opened_workspace_id,
        } = events.recv().await.unwrap()
            && workspace_id == expected
            && opened_workspace_id == Some(fallback)
        {
            return revision;
        }
    }
}

async fn wait_for_sensitive_output(
    events: &mut tokio::sync::broadcast::Receiver<DaemonEvent>,
    expected: SessionId,
) {
    let mut output = Vec::new();
    loop {
        if let DaemonEvent::SessionOutput {
            session_id, bytes, ..
        } = events.recv().await.unwrap()
            && session_id == expected
        {
            assert!(
                output.len().saturating_add(bytes.len()) <= MAX_SENSITIVE_OUTPUT_BYTES,
                "sensitive output marker was not observed within the byte bound"
            );
            output.extend(bytes);
            let text = String::from_utf8_lossy(&output);
            if text.contains(TERMINAL_SENTINEL) && text.contains(ENVIRONMENT_SENTINEL) {
                return;
            }
        }
    }
}

#[cfg(windows)]
fn sensitive_shell_input() -> Vec<u8> {
    format!("echo {TERMINAL_SENTINEL} {ENVIRONMENT_SENTINEL}\r").into_bytes()
}

#[cfg(unix)]
fn sensitive_shell_input() -> Vec<u8> {
    format!("printf '{TERMINAL_SENTINEL} {ENVIRONMENT_SENTINEL}\\n'\n").into_bytes()
}

async fn add_sensitive_remote(repository: &Path) {
    let mut command = tokio::process::Command::new("git");
    command
        .arg("-C")
        .arg(repository)
        .args([
            "remote",
            "add",
            "origin",
            &format!("https://audit:{CREDENTIAL_SENTINEL}@example.invalid/{REMOTE_SENTINEL}.git"),
        ])
        .kill_on_drop(true);
    let status = tokio::time::timeout(Duration::from_secs(10), command.status())
        .await
        .expect("Git remote command timed out")
        .expect("Git remote command failed to start");
    assert!(status.success());
}

fn assert_content_free_removal_audits(database_path: &Path, forbidden: &[&str]) {
    let connection = Connection::open(database_path).unwrap();
    let mut statement = connection
        .prepare(
            "SELECT action, entity_kind, entity_id, outcome, details_json FROM audit_events \
             WHERE action IN ('session_deleted', 'worktree_removed', 'project_removed', \
             'workspace_deleted') ORDER BY occurred_at, id",
        )
        .unwrap();
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(rows.len(), 6);
    for (action, entity_kind, entity_id, outcome, details) in rows {
        assert!(matches!(
            (action.as_str(), entity_kind.as_str()),
            ("session_deleted", "session")
                | ("worktree_removed", "worktree")
                | ("project_removed", "project")
                | ("workspace_deleted", "workspace")
        ));
        assert_eq!(outcome, "succeeded");
        assert_eq!(details, "{}");
        assert!(uuid::Uuid::parse_str(&entity_id).is_ok());
    }
    let complete_audit: String = connection
        .prepare(
            "SELECT COALESCE(actor_id, '') || ' ' || action || ' ' || \
             COALESCE(entity_kind, '') || ' ' || COALESCE(entity_id, '') || ' ' || \
             outcome || ' ' || details_json FROM audit_events ORDER BY occurred_at, id",
        )
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap()
        .join("\n");
    for value in forbidden {
        assert!(
            !complete_audit.contains(value),
            "audit leaked forbidden content: {value}"
        );
    }
}

async fn connect_eventually(paths: &RuntimePaths, client_name: &str) -> DaemonClient {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match DaemonClient::connect(paths, client_name).await {
            Ok(client) => return client,
            Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(error) => panic!("daemon did not become ready: {error}"),
        }
    }
}
