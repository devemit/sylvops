use std::{path::Path, time::Duration};

use rusqlite::Connection;
use sylvops_core::{
    domain::{ProviderKind, WorktreeStatus},
    ids::{ProjectId, SessionId, WorkspaceId, WorktreeId},
    protocol::{ClientRequest, DaemonResponse},
    session_state::SessionLifecycle,
};
use sylvops_daemon::{client::DaemonClient, daemon, runtime::RuntimePaths};
use sylvops_test_support::{AbortOnDropTask, TemporaryRepository, git_branch_exists};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn injected_metadata_failures_roll_back_every_leaf_first_removal() {
    tokio::time::timeout(Duration::from_secs(40), exercise_metadata_rollbacks())
        .await
        .expect("metadata removal rollback timeout");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worktree_persistence_failure_is_redacted_and_reconciles_missing_checkout() {
    tokio::time::timeout(Duration::from_secs(40), exercise_worktree_reconciliation())
        .await
        .expect("Worktree reconciliation timeout");
}

async fn exercise_metadata_rollbacks() {
    let repository = TemporaryRepository::initialize().await.unwrap();
    let paths = RuntimePaths::discover(Some(&repository.root().join("state"))).unwrap();
    let daemon_paths = paths.clone();
    let mut daemon_task = AbortOnDropTask::spawn(async move { daemon::run(daemon_paths).await });
    let client = connect_eventually(&paths).await;
    let workspace_id = add_workspace(&client).await;
    let (project_id, root_worktree_id) =
        add_project(&client, workspace_id, repository.path()).await;
    let session_id = create_stopped_session(&client, root_worktree_id).await;

    let session_token = session_token(&client, session_id).await;
    install_delete_failure(&paths.database, "sessions", &session_id.to_string());
    assert_failure(
        client
            .request(&ClientRequest::DeleteSession {
                session_id,
                authorization_token: session_token,
            })
            .await
            .unwrap(),
    );
    let after_session_failure = snapshot(&client).await;
    assert!(
        after_session_failure
            .sessions
            .iter()
            .any(|item| item.id == session_id)
    );
    drop_failure(&paths.database);
    delete_session(&client, session_id).await;

    let project_token = project_token(&client, project_id).await;
    install_delete_failure(&paths.database, "projects", &project_id.to_string());
    assert_failure(
        client
            .request(&ClientRequest::RemoveProject {
                project_id,
                authorization_token: project_token,
            })
            .await
            .unwrap(),
    );
    let after_project_failure = snapshot(&client).await;
    assert!(
        after_project_failure
            .projects
            .iter()
            .any(|item| item.id == project_id)
    );
    assert!(
        after_project_failure
            .worktrees
            .iter()
            .any(|item| item.id == root_worktree_id)
    );
    drop_failure(&paths.database);
    remove_project(&client, project_id).await;

    let workspace_token = workspace_token(&client, workspace_id).await;
    install_delete_failure(&paths.database, "workspaces", &workspace_id.to_string());
    assert_failure(
        client
            .request(&ClientRequest::DeleteWorkspace {
                workspace_id,
                authorization_token: workspace_token,
            })
            .await
            .unwrap(),
    );
    assert!(
        snapshot(&client)
            .await
            .workspaces
            .iter()
            .any(|item| item.id == workspace_id)
    );
    drop_failure(&paths.database);
    delete_workspace(&client, workspace_id).await;

    client
        .request(&ClientRequest::ShutdownDaemon)
        .await
        .unwrap();
    daemon_task.join().await.unwrap().unwrap();
    let connection = Connection::open(&paths.database).unwrap();
    for action in ["session_deleted", "project_removed", "workspace_deleted"] {
        let count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM audit_events WHERE action = ?1",
                [action],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "failed {action} transaction left an audit record");
    }
    assert!(repository.path().join(".git").exists());
}

async fn exercise_worktree_reconciliation() {
    let repository = TemporaryRepository::initialize().await.unwrap();
    let paths = RuntimePaths::discover(Some(&repository.root().join("state"))).unwrap();
    let daemon_paths = paths.clone();
    let mut daemon_task = AbortOnDropTask::spawn(async move { daemon::run(daemon_paths).await });
    let client = connect_eventually(&paths).await;
    let workspace_id = add_workspace(&client).await;
    let (project_id, _) = add_project(&client, workspace_id, repository.path()).await;
    let worktree = match client
        .request(&ClientRequest::CreateWorktree {
            project_id,
            name: Some("redacted failure target".into()),
            branch: "feature/reconciled-removal".into(),
            base_ref: Some("HEAD".into()),
        })
        .await
        .unwrap()
    {
        DaemonResponse::WorktreeCreated { worktree, .. } => worktree,
        other => panic!("unexpected Worktree response: {other:?}"),
    };
    let worktree_path = Path::new(&worktree.canonical_path).to_path_buf();
    let token = worktree_token(&client, worktree.id).await;
    install_worktree_update_failure(&paths.database, worktree.id);
    assert_failure(
        client
            .request(&ClientRequest::RemoveWorktree {
                worktree_id: worktree.id,
                confirmation_token: token,
            })
            .await
            .unwrap(),
    );
    assert!(!worktree_path.exists());
    assert!(
        git_branch_exists(repository.path(), "feature/reconciled-removal")
            .await
            .unwrap()
    );
    assert!(
        snapshot(&client)
            .await
            .worktrees
            .iter()
            .any(|item| item.id == worktree.id && item.status == WorktreeStatus::Active)
    );
    drop_failure(&paths.database);
    client
        .request(&ClientRequest::ShutdownDaemon)
        .await
        .unwrap();
    daemon_task.join().await.unwrap().unwrap();

    let restart_paths = paths.clone();
    let mut restart_task = AbortOnDropTask::spawn(async move { daemon::run(restart_paths).await });
    let restarted = connect_eventually(&paths).await;
    assert!(
        snapshot(&restarted)
            .await
            .worktrees
            .iter()
            .any(|item| item.id == worktree.id && item.status == WorktreeStatus::Missing)
    );
    restarted
        .request(&ClientRequest::ShutdownDaemon)
        .await
        .unwrap();
    restart_task.join().await.unwrap().unwrap();

    let connection = Connection::open(&paths.database).unwrap();
    let failure_details: String = connection
        .query_row(
            "SELECT details_json FROM audit_events \
             WHERE action = 'worktree_removal_persistence_failed'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&failure_details).unwrap(),
        serde_json::json!({"worktree_id": worktree.id})
    );
    assert!(!failure_details.contains(&worktree.canonical_path));
    assert!(!failure_details.contains("redacted failure target"));
    let reconciliation_details: String = connection
        .query_row(
            "SELECT details_json FROM audit_events WHERE action = 'worktree_missing'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(reconciliation_details, "{}");
}

fn install_delete_failure(database: &Path, table: &str, entity_id: &str) {
    assert!(matches!(table, "sessions" | "projects" | "workspaces"));
    let connection = Connection::open(database).unwrap();
    connection
        .execute_batch(&format!(
            "CREATE TRIGGER injected_removal_failure BEFORE DELETE ON {table} \
             WHEN OLD.id = '{entity_id}' BEGIN SELECT RAISE(ABORT, 'injected failure'); END;"
        ))
        .unwrap();
}

fn install_worktree_update_failure(database: &Path, worktree_id: WorktreeId) {
    let connection = Connection::open(database).unwrap();
    connection
        .execute_batch(&format!(
            "CREATE TRIGGER injected_removal_failure BEFORE UPDATE OF status ON worktrees \
             WHEN OLD.id = '{worktree_id}' AND NEW.status = 'removed' \
             BEGIN SELECT RAISE(ABORT, 'injected failure'); END;"
        ))
        .unwrap();
}

fn drop_failure(database: &Path) {
    Connection::open(database)
        .unwrap()
        .execute_batch("DROP TRIGGER IF EXISTS injected_removal_failure;")
        .unwrap();
}

async fn add_workspace(client: &DaemonClient) -> WorkspaceId {
    match client
        .request(&ClientRequest::AddWorkspace {
            name: "Rollback target".into(),
        })
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
) -> (ProjectId, WorktreeId) {
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
        } => (project.id, root_worktree.id),
        other => panic!("unexpected Project response: {other:?}"),
    }
}

async fn create_stopped_session(client: &DaemonClient, worktree_id: WorktreeId) -> SessionId {
    let session = match client
        .request(&ClientRequest::CreateSession {
            worktree_id,
            provider: ProviderKind::Shell,
            display_name: Some("Rollback Session".into()),
            model: None,
            effort: None,
            initial_prompt: None,
            columns: 80,
            rows: 24,
        })
        .await
        .unwrap()
    {
        DaemonResponse::SessionCreated { session, .. } => session,
        other => panic!("unexpected Session response: {other:?}"),
    };
    client
        .request(&ClientRequest::StopSession {
            session_id: session.id,
        })
        .await
        .unwrap();
    wait_for_session_state(client, session.id, SessionLifecycle::Terminated).await;
    session.id
}

async fn session_token(
    client: &DaemonClient,
    session_id: SessionId,
) -> sylvops_core::protocol::SessionDeletionAuthorization {
    match client
        .request(&ClientRequest::InspectSessionDeletion { session_id })
        .await
        .unwrap()
    {
        DaemonResponse::SessionDeletionInspected(inspection) => {
            inspection.authorization_token.unwrap()
        }
        other => panic!("unexpected Session inspection: {other:?}"),
    }
}

async fn delete_session(client: &DaemonClient, session_id: SessionId) {
    let authorization_token = session_token(client, session_id).await;
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

async fn project_token(
    client: &DaemonClient,
    project_id: ProjectId,
) -> sylvops_core::protocol::ProjectRemovalAuthorization {
    match client
        .request(&ClientRequest::InspectProjectRemoval { project_id })
        .await
        .unwrap()
    {
        DaemonResponse::ProjectRemovalInspected(inspection) => {
            inspection.authorization_token.unwrap()
        }
        other => panic!("unexpected Project inspection: {other:?}"),
    }
}

async fn remove_project(client: &DaemonClient, project_id: ProjectId) {
    let authorization_token = project_token(client, project_id).await;
    assert!(matches!(
        client
            .request(&ClientRequest::RemoveProject {
                project_id,
                authorization_token,
            })
            .await
            .unwrap(),
        DaemonResponse::ProjectRemoved { .. }
    ));
}

async fn workspace_token(
    client: &DaemonClient,
    workspace_id: WorkspaceId,
) -> sylvops_core::protocol::WorkspaceDeletionAuthorization {
    match client
        .request(&ClientRequest::InspectWorkspaceDeletion { workspace_id })
        .await
        .unwrap()
    {
        DaemonResponse::WorkspaceDeletionInspected(inspection) => {
            inspection.authorization_token.unwrap()
        }
        other => panic!("unexpected Workspace inspection: {other:?}"),
    }
}

async fn delete_workspace(client: &DaemonClient, workspace_id: WorkspaceId) {
    let authorization_token = workspace_token(client, workspace_id).await;
    assert!(matches!(
        client
            .request(&ClientRequest::DeleteWorkspace {
                workspace_id,
                authorization_token,
            })
            .await
            .unwrap(),
        DaemonResponse::WorkspaceDeleted { .. }
    ));
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

fn assert_failure(response: DaemonResponse) {
    assert!(matches!(
        response,
        DaemonResponse::Error(failure) if failure.code == "state_operation_failed"
    ));
}

async fn connect_eventually(paths: &RuntimePaths) -> DaemonClient {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match DaemonClient::connect(paths, "removal-failure-test").await {
            Ok(client) => return client,
            Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(error) => panic!("daemon did not become ready: {error}"),
        }
    }
}
