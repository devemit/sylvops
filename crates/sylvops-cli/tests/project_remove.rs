use std::{path::Path, process::Output, process::Stdio, time::Duration};

use sylvops_core::{
    domain::{ProviderKind, SessionState},
    ids::{ProjectId, SessionId},
    protocol::{ClientRequest, DaemonResponse},
};
use sylvops_daemon::{client::DaemonClient, daemon, runtime::RuntimePaths};
use sylvops_test_support::{AbortOnDropTask, TemporaryRepository, bounded};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cli_removes_only_a_confirmed_cleared_project_and_explains_preservation() {
    tokio::time::timeout(Duration::from_secs(30), exercise_cli_project_removal())
        .await
        .expect("CLI Project removal timeout");
}

async fn exercise_cli_project_removal() {
    let repository = TemporaryRepository::initialize().await.unwrap();
    let sentinel = repository.root().join("repository-preserved.txt");
    std::fs::write(&sentinel, "preserved\n").unwrap();
    let state_directory = repository.root().join("state");
    let paths = RuntimePaths::discover(Some(&state_directory)).unwrap();
    let daemon_paths = paths.clone();
    let mut daemon_task = AbortOnDropTask::spawn(async move { daemon::run(daemon_paths).await });
    let client = connect_eventually(&paths).await;

    let workspace = match client
        .request(&ClientRequest::AddWorkspace {
            name: "CLI Project removal".into(),
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
            repository_path: repository.path().to_string_lossy().into_owned(),
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
    };
    let session = match client
        .request(&ClientRequest::CreateSession {
            worktree_id: root_worktree.id,
            provider: ProviderKind::Shell,
            display_name: Some("CLI blocker".into()),
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

    let refused = run_remove(&state_directory, project.id, true).await;
    assert!(!refused.status.success());
    let refused_stdout = String::from_utf8_lossy(&refused.stdout);
    assert!(refused_stdout.contains("Preserved: repository root"));
    assert!(refused_stdout.contains("1 Session remains"));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("Project removal refused"));

    client
        .request(&ClientRequest::StopSession {
            session_id: session.id,
        })
        .await
        .unwrap();
    wait_for_state(&client, session.id, SessionState::Terminated).await;
    delete_session(&client, session.id).await;

    let unconfirmed = run_remove(&state_directory, project.id, false).await;
    assert!(!unconfirmed.status.success());
    assert!(String::from_utf8_lossy(&unconfirmed.stderr).contains("--confirm"));

    let removed = run_remove(&state_directory, project.id, true).await;
    assert!(
        removed.status.success(),
        "CLI Project removal failed: {}",
        String::from_utf8_lossy(&removed.stderr)
    );
    let output = String::from_utf8_lossy(&removed.stdout);
    assert!(output.contains("Preserved: repository root, files, commits, branches, remotes"));
    assert!(output.contains(&format!("Removed Project {}", project.id)));
    assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "preserved\n");
    assert!(repository.path().join(".git").exists());

    let snapshot = match client.request(&ClientRequest::GetSnapshot).await.unwrap() {
        DaemonResponse::Snapshot(snapshot) => snapshot,
        other => panic!("unexpected snapshot response: {other:?}"),
    };
    assert!(!snapshot.projects.iter().any(|value| value.id == project.id));
    client
        .request(&ClientRequest::ShutdownDaemon)
        .await
        .unwrap();
    daemon_task.join().await.unwrap().unwrap();
}

async fn run_remove(state_directory: &Path, project_id: ProjectId, confirm: bool) -> Output {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_sylvops"));
    command
        .arg("--state-dir")
        .arg(state_directory)
        .arg("project")
        .arg("remove")
        .arg(project_id.to_string());
    if confirm {
        command.arg("--confirm");
    }
    command.stdin(Stdio::null()).kill_on_drop(true);
    bounded(command.output())
        .await
        .expect("CLI Project removal command timeout")
        .expect("CLI Project removal command")
}

async fn delete_session(client: &DaemonClient, session_id: SessionId) {
    let authorization_token = match client
        .request(&ClientRequest::InspectSessionDeletion { session_id })
        .await
        .unwrap()
    {
        DaemonResponse::SessionDeletionInspected(inspection) => {
            inspection.authorization_token.unwrap()
        }
        other => panic!("unexpected Session deletion inspection: {other:?}"),
    };
    let response = client
        .request(&ClientRequest::DeleteSession {
            session_id,
            authorization_token,
        })
        .await
        .unwrap();
    assert!(matches!(response, DaemonResponse::SessionDeleted { .. }));
}

async fn connect_eventually(paths: &RuntimePaths) -> DaemonClient {
    loop {
        if let Ok(client) = DaemonClient::connect(paths, "cli-project-remove-test").await {
            return client;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for_state(client: &DaemonClient, session_id: SessionId, state: SessionState) {
    loop {
        let snapshot = match client.request(&ClientRequest::GetSnapshot).await.unwrap() {
            DaemonResponse::Snapshot(snapshot) => snapshot,
            other => panic!("unexpected snapshot response: {other:?}"),
        };
        if snapshot
            .sessions
            .iter()
            .any(|session| session.id == session_id && session.state == state)
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
