use std::{path::Path, time::Duration};

use sylvops_core::{
    domain::{ProviderKind, SessionState},
    ids::{ProjectId, WorktreeId},
    protocol::{ClientRequest, DaemonEvent, DaemonResponse, ProjectRemovalRefusal},
};
use sylvops_daemon::{
    client::DaemonClient, daemon, database::DatabaseHandle, git, runtime::RuntimePaths,
};
use sylvops_test_support::AbortOnDropTask;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn project_removal_is_state_bound_revisioned_and_preserves_git_content() {
    tokio::time::timeout(Duration::from_secs(40), exercise_project_removal())
        .await
        .expect("project removal end-to-end timeout");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn large_cleared_project_inspection_is_bounded_and_removable() {
    tokio::time::timeout(Duration::from_secs(30), exercise_oversized_inspection())
        .await
        .expect("oversized Project inspection timeout");
}

async fn exercise_oversized_inspection() {
    let temporary = tempfile::tempdir().unwrap();
    let repository = temporary.path().join("repository");
    initialize_repository(&repository).await;
    let paths = RuntimePaths::discover(Some(&temporary.path().join("state"))).unwrap();
    paths.prepare().unwrap();
    let database = DatabaseHandle::open(&paths.database).unwrap();
    let (_, workspace) = database
        .add_workspace("bounded inspection".into())
        .await
        .unwrap();
    let registration = git::inspect_repository(workspace.id, &repository)
        .await
        .unwrap();
    let (_, project, _) = database.add_project(registration).await.unwrap();
    database.shutdown().await.unwrap();

    let mut connection = rusqlite::Connection::open(&paths.database).unwrap();
    connection
        .pragma_update(None, "foreign_keys", true)
        .unwrap();
    let transaction = connection.transaction().unwrap();
    for index in 0..1_100 {
        let path = format!("C:/bounded-project-removal/{index}");
        transaction
            .execute(
                "INSERT INTO worktrees(id, project_id, name, path, canonical_path, branch, \
                 base_ref, base_commit, is_root_checkout, status, created_at, last_activity_at, \
                 removed_at) VALUES (?1, ?2, 'removed', ?3, ?3, NULL, 'main', 'deadbeef', 0, \
                 'removed', 1, 1, 1)",
                rusqlite::params![WorktreeId::new().to_string(), project.id.to_string(), path],
            )
            .unwrap();
    }
    transaction.commit().unwrap();
    drop(connection);

    let daemon_paths = paths.clone();
    let mut daemon_task = AbortOnDropTask::spawn(async move { daemon::run(daemon_paths).await });
    let client = connect_eventually(&paths).await;
    let inspection = inspect(&client, project.id).await;
    assert!(inspection.refusals.is_empty());
    assert!(serde_json::to_vec(&inspection).unwrap().len() < 4 * 1024);
    let authorization_token = inspection.authorization_token.unwrap();
    let response = client
        .request(&ClientRequest::RemoveProject {
            project_id: project.id,
            authorization_token,
        })
        .await
        .unwrap();
    assert!(matches!(response, DaemonResponse::ProjectRemoved { .. }));
    assert!(
        !snapshot(&client)
            .await
            .projects
            .iter()
            .any(|value| value.id == project.id)
    );
    assert!(repository.join(".git").exists());
    client
        .request(&ClientRequest::ShutdownDaemon)
        .await
        .unwrap();
    daemon_task.join().await.unwrap().unwrap();
}

#[allow(clippy::too_many_lines)]
async fn exercise_project_removal() {
    let temporary = tempfile::tempdir().unwrap();
    let repository = temporary.path().join("repository");
    initialize_repository(&repository).await;
    let repository_sentinel = repository.join("repository-preserved.txt");
    std::fs::write(&repository_sentinel, "repository preserved\n").unwrap();
    run_git(&repository, &["branch", "external-preserved"]).await;
    let external_worktree = temporary.path().join("external-worktree");
    run_git(
        &repository,
        &[
            "worktree",
            "add",
            external_worktree.to_str().unwrap(),
            "external-preserved",
        ],
    )
    .await;
    let external_sentinel = external_worktree.join("external-preserved.txt");
    std::fs::write(&external_sentinel, "external preserved\n").unwrap();

    let paths = RuntimePaths::discover(Some(&temporary.path().join("state"))).unwrap();
    let daemon_paths = paths.clone();
    let mut daemon_task = AbortOnDropTask::spawn(async move { daemon::run(daemon_paths).await });
    let client = connect_eventually(&paths).await;
    let competing = connect_eventually(&paths).await;
    let mut events = client.subscribe();
    let workspace = match client
        .request(&ClientRequest::AddWorkspace {
            name: "project-removal".into(),
        })
        .await
        .unwrap()
    {
        DaemonResponse::WorkspaceAdded { workspace, .. } => workspace,
        other => panic!("unexpected workspace response: {other:?}"),
    };
    let (project, root_worktree) = match client
        .request(&ClientRequest::AddProject {
            workspace_id: workspace.id,
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
        other => panic!("unexpected project response: {other:?}"),
    };

    let stale_for_session = inspection_token(&client, project.id).await;
    let unauthorized = "0".repeat(64).try_into().unwrap();
    assert_project_refused(
        &client
            .request(&ClientRequest::RemoveProject {
                project_id: project.id,
                authorization_token: unauthorized,
            })
            .await
            .unwrap(),
    );
    let session = match client
        .request(&ClientRequest::CreateSession {
            worktree_id: root_worktree.id,
            provider: ProviderKind::Shell,
            display_name: Some("blocking Session".into()),
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
        other => panic!("unexpected session response: {other:?}"),
    };
    let blocked = inspect(&client, project.id).await;
    assert!(blocked.authorization_token.is_none());
    assert!(matches!(
        blocked.refusals.as_slice(),
        [ProjectRemovalRefusal::SessionsRemain { count: 1, ids, .. }] if ids == &[session.id]
    ));
    assert_project_refused(
        &client
            .request(&ClientRequest::RemoveProject {
                project_id: project.id,
                authorization_token: stale_for_session,
            })
            .await
            .unwrap(),
    );
    client
        .request(&ClientRequest::StopSession {
            session_id: session.id,
        })
        .await
        .unwrap();
    wait_for_session_state(&client, session.id, SessionState::Terminated).await;
    let deletion_token = match client
        .request(&ClientRequest::InspectSessionDeletion {
            session_id: session.id,
        })
        .await
        .unwrap()
    {
        DaemonResponse::SessionDeletionInspected(inspection) => {
            inspection.authorization_token.unwrap()
        }
        other => panic!("unexpected Session deletion inspection: {other:?}"),
    };
    client
        .request(&ClientRequest::DeleteSession {
            session_id: session.id,
            authorization_token: deletion_token,
        })
        .await
        .unwrap();

    let stale_for_worktree = inspection_token(&client, project.id).await;
    let managed_worktree = match client
        .request(&ClientRequest::CreateWorktree {
            project_id: project.id,
            name: Some("blocking Worktree".into()),
            branch: "feature/project-removal-blocker".into(),
            base_ref: Some("main".into()),
        })
        .await
        .unwrap()
    {
        DaemonResponse::WorktreeCreated { worktree, .. } => worktree,
        other => panic!("unexpected Worktree response: {other:?}"),
    };
    let blocked = inspect(&client, project.id).await;
    assert!(blocked.authorization_token.is_none());
    assert!(matches!(
        blocked.refusals.as_slice(),
        [ProjectRemovalRefusal::ManagedWorktreesRemain { count: 1, ids, .. }]
            if ids == &[managed_worktree.id]
    ));
    assert_project_refused(
        &client
            .request(&ClientRequest::RemoveProject {
                project_id: project.id,
                authorization_token: stale_for_worktree,
            })
            .await
            .unwrap(),
    );
    let status = match client
        .request(&ClientRequest::GetWorktreeStatus {
            worktree_id: managed_worktree.id,
        })
        .await
        .unwrap()
    {
        DaemonResponse::WorktreeStatus(status) => status,
        other => panic!("unexpected Worktree status: {other:?}"),
    };
    client
        .request(&ClientRequest::RemoveWorktree {
            worktree_id: managed_worktree.id,
            confirmation_token: status.removal_confirmation_token.unwrap(),
        })
        .await
        .unwrap();

    let token = inspection_token(&client, project.id).await;
    let request = ClientRequest::RemoveProject {
        project_id: project.id,
        authorization_token: token,
    };
    let (first, second) = tokio::join!(client.request(&request), competing.request(&request));
    let responses = [first.unwrap(), second.unwrap()];
    assert_eq!(
        responses
            .iter()
            .filter(|response| matches!(response, DaemonResponse::ProjectRemoved { .. }))
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
            DaemonEvent::ProjectRemoved { project_id, workspace_id, .. }
                if project_id == project.id && workspace_id == workspace.id
        ) {
            break;
        }
    }
    let current = snapshot(&client).await;
    assert!(!current.projects.iter().any(|value| value.id == project.id));
    assert!(
        !current
            .worktrees
            .iter()
            .any(|value| value.project_id == project.id)
    );
    assert!(
        current
            .workspaces
            .iter()
            .any(|value| value.id == workspace.id)
    );
    assert_eq!(
        std::fs::read_to_string(&repository_sentinel).unwrap(),
        "repository preserved\n"
    );
    assert_eq!(
        std::fs::read_to_string(&external_sentinel).unwrap(),
        "external preserved\n"
    );
    assert!(repository.join(".git").exists());
    assert!(external_worktree.join(".git").exists());

    let unknown = client
        .request(&ClientRequest::InspectProjectRemoval {
            project_id: ProjectId::new(),
        })
        .await
        .unwrap();
    assert!(matches!(unknown, DaemonResponse::Error(_)));
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
            .projects
            .iter()
            .any(|value| value.id == project.id)
    );
    assert_eq!(
        std::fs::read_to_string(&repository_sentinel).unwrap(),
        "repository preserved\n"
    );
    assert_eq!(
        std::fs::read_to_string(&external_sentinel).unwrap(),
        "external preserved\n"
    );
    restarted
        .request(&ClientRequest::ShutdownDaemon)
        .await
        .unwrap();
    restart_task.join().await.unwrap().unwrap();
}

async fn inspect(
    client: &DaemonClient,
    project_id: ProjectId,
) -> sylvops_core::protocol::ProjectRemovalInspection {
    match client
        .request(&ClientRequest::InspectProjectRemoval { project_id })
        .await
        .unwrap()
    {
        DaemonResponse::ProjectRemovalInspected(inspection) => inspection,
        other => panic!("unexpected Project removal inspection: {other:?}"),
    }
}

async fn inspection_token(
    client: &DaemonClient,
    project_id: ProjectId,
) -> sylvops_core::protocol::ProjectRemovalAuthorization {
    inspect(client, project_id)
        .await
        .authorization_token
        .expect("Project removal authorization")
}

fn assert_project_refused(response: &DaemonResponse) {
    assert!(matches!(
        response,
        DaemonResponse::Error(failure) if failure.code == "project_request_refused"
    ));
}

async fn snapshot(client: &DaemonClient) -> sylvops_core::domain::DaemonSnapshot {
    match client.request(&ClientRequest::GetSnapshot).await.unwrap() {
        DaemonResponse::Snapshot(snapshot) => snapshot,
        other => panic!("unexpected snapshot response: {other:?}"),
    }
}

async fn wait_for_session_state(
    client: &DaemonClient,
    session_id: sylvops_core::ids::SessionId,
    state: SessionState,
) {
    loop {
        if snapshot(client)
            .await
            .sessions
            .iter()
            .any(|session| session.id == session_id && session.state == state)
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
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
        match DaemonClient::connect(paths, "project-removal-test").await {
            Ok(client) => return client,
            Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(error) => panic!("daemon did not become ready: {error}"),
        }
    }
}
