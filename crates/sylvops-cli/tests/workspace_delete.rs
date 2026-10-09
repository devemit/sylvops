use std::{path::Path, process::Output, process::Stdio, time::Duration};

use sylvops_core::{
    ids::WorkspaceId,
    protocol::{ClientRequest, DaemonResponse},
};
use sylvops_daemon::{client::DaemonClient, daemon, runtime::RuntimePaths};
use sylvops_test_support::{AbortOnDropTask, TemporaryRepository, bounded};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cli_deletes_only_a_confirmed_empty_workspace_and_explains_fallback() {
    tokio::time::timeout(Duration::from_secs(30), exercise_cli_workspace_deletion())
        .await
        .expect("CLI Workspace deletion timeout");
}

async fn exercise_cli_workspace_deletion() {
    let repository = TemporaryRepository::initialize().await.unwrap();
    let sentinel = repository.root().join("repository-preserved.txt");
    std::fs::write(&sentinel, "preserved\n").unwrap();
    let state_directory = repository.root().join("state");
    let paths = RuntimePaths::discover(Some(&state_directory)).unwrap();
    let daemon_paths = paths.clone();
    let mut daemon_task = AbortOnDropTask::spawn(async move { daemon::run(daemon_paths).await });
    let client = connect_eventually(&paths).await;

    let fallback = add_workspace(&client, "fallback").await;
    let target = add_workspace(&client, "target").await;
    let project = match client
        .request(&ClientRequest::AddProject {
            workspace_id: target,
            repository_path: repository.path().to_string_lossy().into_owned(),
        })
        .await
        .unwrap()
    {
        DaemonResponse::ProjectAdded { project, .. } => project,
        other => panic!("unexpected Project response: {other:?}"),
    };

    let refused = run_delete(&state_directory, target, true).await;
    assert!(!refused.status.success());
    let refused_stdout = String::from_utf8_lossy(&refused.stdout);
    assert!(refused_stdout.contains("Preserved: repository roots"));
    assert!(refused_stdout.contains("1 Project remains"));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("Workspace deletion refused"));

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

    let unconfirmed = run_delete(&state_directory, target, false).await;
    assert!(!unconfirmed.status.success());
    assert!(String::from_utf8_lossy(&unconfirmed.stderr).contains("--confirm"));

    let deleted = run_delete(&state_directory, target, true).await;
    assert!(
        deleted.status.success(),
        "CLI Workspace deletion failed: {}",
        String::from_utf8_lossy(&deleted.stderr)
    );
    let output = String::from_utf8_lossy(&deleted.stdout);
    assert!(output.contains("Preserved: repository roots, managed and external Worktrees"));
    assert!(output.contains(&format!("Navigation: Workspace {fallback} will open")));
    assert!(output.contains(&format!("Deleted Workspace {target}")));
    assert!(output.contains(&format!("Opened Workspace {fallback}")));
    assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "preserved\n");
    assert!(repository.path().join(".git").exists());

    let snapshot = match client.request(&ClientRequest::GetSnapshot).await.unwrap() {
        DaemonResponse::Snapshot(snapshot) => snapshot,
        other => panic!("unexpected snapshot response: {other:?}"),
    };
    assert!(
        !snapshot
            .workspaces
            .iter()
            .any(|workspace| workspace.id == target)
    );
    assert!(
        snapshot
            .workspaces
            .iter()
            .any(|workspace| workspace.id == fallback && workspace.is_open)
    );
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

async fn run_delete(state_directory: &Path, workspace_id: WorkspaceId, confirm: bool) -> Output {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_sylvops"));
    command
        .arg("--state-dir")
        .arg(state_directory)
        .arg("workspace")
        .arg("delete")
        .arg(workspace_id.to_string());
    if confirm {
        command.arg("--confirm");
    }
    command.stdin(Stdio::null()).kill_on_drop(true);
    bounded(command.output())
        .await
        .expect("CLI Workspace deletion command timeout")
        .expect("CLI Workspace deletion command")
}

async fn connect_eventually(paths: &RuntimePaths) -> DaemonClient {
    loop {
        if let Ok(client) = DaemonClient::connect(paths, "cli-workspace-delete-test").await {
            return client;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
