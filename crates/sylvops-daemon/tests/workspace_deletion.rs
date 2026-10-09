use std::{path::Path, time::Duration};

use sylvops_core::{
    ids::{ProjectId, WorkspaceId},
    protocol::{ClientRequest, DaemonEvent, DaemonResponse, WorkspaceDeletionRefusal},
};
use sylvops_daemon::{
    client::DaemonClient, daemon, database::DatabaseHandle, runtime::RuntimePaths,
};
use sylvops_test_support::AbortOnDropTask;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn workspace_deletion_is_state_bound_revisioned_and_preserves_content() {
    tokio::time::timeout(Duration::from_secs(40), exercise_workspace_deletion())
        .await
        .expect("Workspace deletion end-to-end timeout");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn workspace_deletion_inspection_is_bounded() {
    tokio::time::timeout(Duration::from_secs(20), exercise_bounded_inspection())
        .await
        .expect("bounded Workspace inspection timeout");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inactive_and_final_workspace_deletion_produces_first_run_state() {
    tokio::time::timeout(Duration::from_secs(20), exercise_final_workspace_deletion())
        .await
        .expect("final Workspace deletion timeout");
}

#[allow(clippy::too_many_lines)]
async fn exercise_workspace_deletion() {
    let temporary = tempfile::tempdir().unwrap();
    let repository = temporary.path().join("repository");
    initialize_repository(&repository).await;
    let sentinel = repository.join("preserved.txt");
    std::fs::write(&sentinel, "preserved\n").unwrap();
    let protected_repository = temporary.path().join("protected-repository");
    initialize_repository(&protected_repository).await;
    let protected_sentinel = protected_repository.join("protected.txt");
    std::fs::write(&protected_sentinel, "protected\n").unwrap();
    run_git(&protected_repository, &["branch", "external-preserved"]).await;
    let external_worktree = temporary.path().join("external-worktree");
    run_git(
        &protected_repository,
        &[
            "worktree",
            "add",
            external_worktree.to_str().unwrap(),
            "external-preserved",
        ],
    )
    .await;
    let external_sentinel = external_worktree.join("external.txt");
    std::fs::write(&external_sentinel, "external\n").unwrap();

    let paths = RuntimePaths::discover(Some(&temporary.path().join("state"))).unwrap();
    let daemon_paths = paths.clone();
    let mut daemon_task = AbortOnDropTask::spawn(async move { daemon::run(daemon_paths).await });
    let client = connect_eventually(&paths).await;
    let competing = connect_eventually(&paths).await;
    let mut events = client.subscribe();

    let older = add_workspace(&client, "older").await;
    let _recent = add_workspace(&client, "recent").await;
    let target = add_workspace(&client, "target").await;
    let protected_project = match client
        .request(&ClientRequest::AddProject {
            workspace_id: older,
            repository_path: protected_repository.to_string_lossy().into_owned(),
        })
        .await
        .unwrap()
    {
        DaemonResponse::ProjectAdded { project, .. } => project,
        other => panic!("unexpected protected Project response: {other:?}"),
    };
    let managed_worktree = match client
        .request(&ClientRequest::CreateWorktree {
            project_id: protected_project.id,
            name: Some("preserved managed Worktree".into()),
            branch: "feature/managed-preserved".into(),
            base_ref: Some("main".into()),
        })
        .await
        .unwrap()
    {
        DaemonResponse::WorktreeCreated { worktree, .. } => worktree,
        other => panic!("unexpected managed Worktree response: {other:?}"),
    };
    let managed_sentinel = Path::new(&managed_worktree.canonical_path).join("managed.txt");
    std::fs::write(&managed_sentinel, "managed\n").unwrap();
    open_workspace(&client, older).await;
    tokio::time::sleep(Duration::from_millis(2)).await;
    open_workspace(&client, target).await;

    let stale_token = inspection_token(&client, target).await;
    let project = match client
        .request(&ClientRequest::AddProject {
            workspace_id: target,
            repository_path: repository.to_string_lossy().into_owned(),
        })
        .await
        .unwrap()
    {
        DaemonResponse::ProjectAdded { project, .. } => project,
        other => panic!("unexpected Project response: {other:?}"),
    };
    assert_workspace_refused(
        &client
            .request(&ClientRequest::DeleteWorkspace {
                workspace_id: target,
                authorization_token: stale_token,
            })
            .await
            .unwrap(),
    );
    let blocked = inspect(&client, target).await;
    assert!(blocked.authorization_token.is_none());
    assert!(matches!(
        blocked.refusals.as_slice(),
        [WorkspaceDeletionRefusal::ProjectsRemain { count: 1, ids, .. }]
            if ids == &[project.id]
    ));
    let project_token = match client
        .request(&ClientRequest::InspectProjectRemoval {
            project_id: project.id,
        })
        .await
        .unwrap()
    {
        DaemonResponse::ProjectRemovalInspected(inspection) => {
            inspection.authorization_token.unwrap()
        }
        other => panic!("unexpected Project removal inspection: {other:?}"),
    };
    client
        .request(&ClientRequest::RemoveProject {
            project_id: project.id,
            authorization_token: project_token,
        })
        .await
        .unwrap();

    let inspection = inspect(&client, target).await;
    assert!(inspection.is_open);
    assert_eq!(inspection.fallback_workspace_id, Some(older));
    let request = ClientRequest::DeleteWorkspace {
        workspace_id: target,
        authorization_token: inspection.authorization_token.unwrap(),
    };
    let (first, second) = tokio::join!(client.request(&request), competing.request(&request));
    let responses = [first.unwrap(), second.unwrap()];
    assert_eq!(
        responses
            .iter()
            .filter(|response| matches!(response, DaemonResponse::WorkspaceDeleted { .. }))
            .count(),
        1
    );
    assert_eq!(
        responses
            .iter()
            .filter(|response| matches!(response, DaemonResponse::Error(_)))
            .count(),
        1
    );
    loop {
        if matches!(
            events.recv().await.unwrap(),
            DaemonEvent::WorkspaceDeleted {
                workspace_id,
                opened_workspace_id: Some(opened),
                ..
            } if workspace_id == target && opened == older
        ) {
            break;
        }
    }
    let current = snapshot(&client).await;
    assert!(
        !current
            .workspaces
            .iter()
            .any(|workspace| workspace.id == target)
    );
    assert!(
        current
            .workspaces
            .iter()
            .any(|workspace| workspace.id == older && workspace.is_open)
    );
    assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "preserved\n");
    assert!(repository.join(".git").exists());
    assert_eq!(
        std::fs::read_to_string(&protected_sentinel).unwrap(),
        "protected\n"
    );
    assert_eq!(
        std::fs::read_to_string(&external_sentinel).unwrap(),
        "external\n"
    );
    assert_eq!(
        std::fs::read_to_string(&managed_sentinel).unwrap(),
        "managed\n"
    );
    assert!(external_worktree.join(".git").exists());
    assert!(
        Path::new(&managed_worktree.canonical_path)
            .join(".git")
            .exists()
    );
    run_git(
        &protected_repository,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            "refs/heads/external-preserved",
        ],
    )
    .await;
    run_git(
        &protected_repository,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            "refs/heads/feature/managed-preserved",
        ],
    )
    .await;
    assert!(matches!(
        client
            .request(&ClientRequest::InspectWorkspaceDeletion {
                workspace_id: WorkspaceId::new(),
            })
            .await
            .unwrap(),
        DaemonResponse::Error(_)
    ));

    client
        .request(&ClientRequest::ShutdownDaemon)
        .await
        .unwrap();
    daemon_task.join().await.unwrap().unwrap();
    drop(client);
    drop(competing);

    let restart_paths = paths.clone();
    let mut restart_task = AbortOnDropTask::spawn(async move { daemon::run(restart_paths).await });
    let restarted = connect_eventually(&paths).await;
    let restarted_snapshot = snapshot(&restarted).await;
    assert!(
        !restarted_snapshot
            .workspaces
            .iter()
            .any(|workspace| workspace.id == target)
    );
    assert!(
        restarted_snapshot
            .workspaces
            .iter()
            .any(|workspace| workspace.id == older && workspace.is_open)
    );

    assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "preserved\n");
    assert_eq!(
        std::fs::read_to_string(&managed_sentinel).unwrap(),
        "managed\n"
    );
    assert_eq!(
        std::fs::read_to_string(&external_sentinel).unwrap(),
        "external\n"
    );
    restarted
        .request(&ClientRequest::ShutdownDaemon)
        .await
        .unwrap();
    restart_task.join().await.unwrap().unwrap();
}

async fn exercise_final_workspace_deletion() {
    let temporary = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::discover(Some(&temporary.path().join("state"))).unwrap();
    let daemon_paths = paths.clone();
    let mut daemon_task = AbortOnDropTask::spawn(async move { daemon::run(daemon_paths).await });
    let client = connect_eventually(&paths).await;
    let inactive_id = add_workspace(&client, "inactive").await;
    let final_id = add_workspace(&client, "final").await;

    let inactive_token = inspection_token(&client, inactive_id).await;
    let inactive = client
        .request(&ClientRequest::DeleteWorkspace {
            workspace_id: inactive_id,
            authorization_token: inactive_token,
        })
        .await
        .unwrap();
    assert!(matches!(
        inactive,
        DaemonResponse::WorkspaceDeleted {
            opened_workspace_id: None,
            ..
        }
    ));
    assert!(snapshot(&client).await.workspaces[0].is_open);

    let final_token = inspection_token(&client, final_id).await;
    let final_request = ClientRequest::DeleteWorkspace {
        workspace_id: final_id,
        authorization_token: final_token,
    };
    client.request(&final_request).await.unwrap();
    assert!(snapshot(&client).await.workspaces.is_empty());
    assert_workspace_refused(&client.request(&final_request).await.unwrap());
    client
        .request(&ClientRequest::ShutdownDaemon)
        .await
        .unwrap();
    daemon_task.join().await.unwrap().unwrap();
}

async fn exercise_bounded_inspection() {
    let temporary = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::discover(Some(&temporary.path().join("state"))).unwrap();
    paths.prepare().unwrap();
    let database = DatabaseHandle::open(&paths.database).unwrap();
    let (_, workspace) = database.add_workspace("bounded".into()).await.unwrap();
    database.shutdown().await.unwrap();

    let mut connection = rusqlite::Connection::open(&paths.database).unwrap();
    connection
        .pragma_update(None, "foreign_keys", true)
        .unwrap();
    let transaction = connection.transaction().unwrap();
    for index in 0..1_100 {
        let path = format!("C:/bounded-workspace-deletion/{index}");
        transaction
            .execute(
                "INSERT INTO projects(id, workspace_id, name, repository_path, \
                 canonical_repository_path, created_at, last_activity_at) \
                 VALUES (?1, ?2, 'project', ?3, ?3, 1, 1)",
                rusqlite::params![ProjectId::new().to_string(), workspace.id.to_string(), path],
            )
            .unwrap();
    }
    transaction.commit().unwrap();
    drop(connection);

    let daemon_paths = paths.clone();
    let mut daemon_task = AbortOnDropTask::spawn(async move { daemon::run(daemon_paths).await });
    let client = connect_eventually(&paths).await;
    let inspection = inspect(&client, workspace.id).await;
    assert!(inspection.authorization_token.is_none());
    assert!(matches!(
        inspection.refusals.as_slice(),
        [WorkspaceDeletionRefusal::ProjectsRemain {
            count: 33,
            truncated: true,
            ids,
        }] if ids.len() == 32
    ));
    assert!(serde_json::to_vec(&inspection).unwrap().len() < 4 * 1024);
    client
        .request(&ClientRequest::ShutdownDaemon)
        .await
        .unwrap();
    daemon_task.join().await.unwrap().unwrap();
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

async fn open_workspace(client: &DaemonClient, workspace_id: WorkspaceId) {
    let response = client
        .request(&ClientRequest::OpenWorkspace { workspace_id })
        .await
        .unwrap();
    assert!(matches!(response, DaemonResponse::WorkspaceOpened { .. }));
}

async fn inspect(
    client: &DaemonClient,
    workspace_id: WorkspaceId,
) -> sylvops_core::protocol::WorkspaceDeletionInspection {
    match client
        .request(&ClientRequest::InspectWorkspaceDeletion { workspace_id })
        .await
        .unwrap()
    {
        DaemonResponse::WorkspaceDeletionInspected(inspection) => inspection,
        other => panic!("unexpected Workspace deletion inspection: {other:?}"),
    }
}

async fn inspection_token(
    client: &DaemonClient,
    workspace_id: WorkspaceId,
) -> sylvops_core::protocol::WorkspaceDeletionAuthorization {
    inspect(client, workspace_id)
        .await
        .authorization_token
        .expect("Workspace deletion authorization")
}

fn assert_workspace_refused(response: &DaemonResponse) {
    assert!(matches!(
        response,
        DaemonResponse::Error(failure)
            if failure.code == "workspace_request_refused" || failure.code == "state_operation_failed"
    ));
}

async fn snapshot(client: &DaemonClient) -> sylvops_core::domain::DaemonSnapshot {
    match client.request(&ClientRequest::GetSnapshot).await.unwrap() {
        DaemonResponse::Snapshot(snapshot) => snapshot,
        other => panic!("unexpected snapshot response: {other:?}"),
    }
}

async fn initialize_repository(path: &Path) {
    std::fs::create_dir_all(path).unwrap();
    run_git(path, &["init", "-b", "main"]).await;
    run_git(path, &["config", "user.name", "SylvOps Tests"]).await;
    run_git(path, &["config", "user.email", "tests@sylvops.invalid"]).await;
    std::fs::write(path.join("README.md"), "seed\n").unwrap();
    run_git(path, &["add", "README.md"]).await;
    run_git(path, &["commit", "-m", "seed"]).await;
}

async fn run_git(path: &Path, arguments: &[&str]) {
    let mut command = tokio::process::Command::new("git");
    command.args(arguments).current_dir(path).kill_on_drop(true);
    let status = tokio::time::timeout(Duration::from_secs(10), command.status())
        .await
        .expect("Git fixture command timed out")
        .expect("Git fixture command failed to start");
    assert!(status.success(), "git command failed: {arguments:?}");
}

async fn connect_eventually(paths: &RuntimePaths) -> DaemonClient {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match DaemonClient::connect(paths, "workspace-deletion-test").await {
            Ok(client) => return client,
            Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(error) => panic!("daemon did not become ready: {error}"),
        }
    }
}
