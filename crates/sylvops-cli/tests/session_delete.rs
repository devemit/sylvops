use std::{path::Path, process::Output, process::Stdio, time::Duration};

use sylvops_core::{
    domain::{ProviderKind, SessionState},
    ids::SessionId,
    protocol::{ClientRequest, DaemonResponse},
};
use sylvops_daemon::{client::DaemonClient, daemon, runtime::RuntimePaths};
use sylvops_test_support::{TemporaryRepository, bounded};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::too_many_lines)]
async fn cli_deletes_only_a_confirmed_inactive_session_and_prints_the_boundary() {
    tokio::time::timeout(Duration::from_secs(30), exercise_cli_session_deletion())
        .await
        .expect("CLI session deletion timeout");
}

async fn exercise_cli_session_deletion() {
    let repository = TemporaryRepository::initialize().await.unwrap();
    let state_directory = repository.root().join("state");
    let paths = RuntimePaths::discover(Some(&state_directory)).unwrap();
    let daemon_paths = paths.clone();
    let daemon_task = tokio::spawn(async move { daemon::run(daemon_paths).await });
    let client = connect_eventually(&paths).await;

    let workspace = match client
        .request(&ClientRequest::AddWorkspace {
            name: "CLI deletion".into(),
        })
        .await
        .unwrap()
    {
        DaemonResponse::WorkspaceAdded { workspace, .. } => workspace,
        other => panic!("unexpected workspace response: {other:?}"),
    };
    let worktree = match client
        .request(&ClientRequest::AddProject {
            workspace_id: workspace.id,
            repository_path: repository.path().to_string_lossy().into_owned(),
        })
        .await
        .unwrap()
    {
        DaemonResponse::ProjectAdded { root_worktree, .. } => root_worktree,
        other => panic!("unexpected project response: {other:?}"),
    };
    let session = match client
        .request(&ClientRequest::CreateSession {
            worktree_id: worktree.id,
            provider: ProviderKind::Shell,
            display_name: Some("CLI deletion".into()),
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

    let refused = run_delete(&state_directory, session.id).await;
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stdout).contains("Preserved: worktree"));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("session deletion refused"));

    client
        .request(&ClientRequest::StopSession {
            session_id: session.id,
        })
        .await
        .unwrap();
    wait_for_state(&client, session.id, SessionState::Terminated).await;

    let deleted = run_delete(&state_directory, session.id).await;
    assert!(
        deleted.status.success(),
        "CLI deletion failed: {}",
        String::from_utf8_lossy(&deleted.stderr)
    );
    let output = String::from_utf8_lossy(&deleted.stdout);
    assert!(output.contains("Preserved: worktree"));
    assert!(output.contains("repository files, Git state, and neighboring resume Sessions"));
    assert!(output.contains(&format!("Deleted Session {}", session.id)));

    let snapshot = match client.request(&ClientRequest::GetSnapshot).await.unwrap() {
        DaemonResponse::Snapshot(snapshot) => snapshot,
        other => panic!("unexpected snapshot response: {other:?}"),
    };
    assert!(!snapshot.sessions.iter().any(|value| value.id == session.id));
    client
        .request(&ClientRequest::ShutdownDaemon)
        .await
        .unwrap();
    daemon_task.await.unwrap().unwrap();
}

async fn run_delete(state_directory: &Path, session_id: SessionId) -> Output {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_sylvops"));
    command
        .arg("--state-dir")
        .arg(state_directory)
        .arg("session")
        .arg("delete")
        .arg(session_id.to_string())
        .arg("--confirm")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    bounded(command.output())
        .await
        .expect("CLI deletion command timeout")
        .expect("CLI deletion command")
}

async fn connect_eventually(paths: &RuntimePaths) -> DaemonClient {
    loop {
        if let Ok(client) = DaemonClient::connect(paths, "cli-session-delete-test").await {
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
