use std::{path::Path, process::Command, time::Duration};

use sylvops_core::{
    domain::SessionState,
    protocol::{ClientRequest, DaemonEvent, DaemonResponse},
};
use sylvops_daemon::{client::DaemonClient, daemon, runtime::RuntimePaths};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shell_survives_detach_and_replays_after_reconnect() {
    tokio::time::timeout(Duration::from_secs(20), exercise_persistent_session())
        .await
        .expect("Phase 2 end-to-end timeout");
}

#[allow(clippy::too_many_lines)]
async fn exercise_persistent_session() {
    let temporary = tempfile::tempdir().unwrap();
    let repository = temporary.path().join("repository");
    initialize_repository(&repository);
    let paths = RuntimePaths::discover(Some(&temporary.path().join("state"))).unwrap();
    let daemon_paths = paths.clone();
    let daemon_task = tokio::spawn(async move { daemon::run(daemon_paths).await });
    let first = connect_eventually(&paths).await;

    let workspace = match first
        .request(&ClientRequest::AddWorkspace { name: "e2e".into() })
        .await
        .unwrap()
    {
        DaemonResponse::WorkspaceAdded { workspace, .. } => workspace,
        other => panic!("unexpected workspace response: {other:?}"),
    };
    let worktree = match first
        .request(&ClientRequest::AddProject {
            workspace_id: workspace.id,
            repository_path: repository.to_string_lossy().into_owned(),
        })
        .await
        .unwrap()
    {
        DaemonResponse::ProjectAdded { root_worktree, .. } => root_worktree,
        other => panic!("unexpected project response: {other:?}"),
    };
    let session = match first
        .request(&ClientRequest::CreateSession {
            worktree_id: worktree.id,
            provider: sylvops_core::domain::ProviderKind::Shell,
            display_name: Some("e2e shell".into()),
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

    let mut events = first.subscribe();
    assert!(matches!(
        first
            .request(&ClientRequest::AttachSession {
                session_id: session.id,
                from_sequence: 0,
                columns: 80,
                rows: 24,
            })
            .await
            .unwrap(),
        DaemonResponse::Attached { .. }
    ));
    first
        .request(&ClientRequest::SessionInput {
            session_id: session.id,
            bytes: shell_echo_command(),
        })
        .await
        .unwrap();
    let first_output = collect_until(&mut events, session.id, "SYLVOPS_E2E").await;
    assert!(!first_output.is_empty());
    first
        .request(&ClientRequest::DetachSession {
            session_id: session.id,
        })
        .await
        .unwrap();
    drop(first);

    let second = connect_eventually(&paths).await;
    let mut replay_events = second.subscribe();
    second
        .request(&ClientRequest::AttachSession {
            session_id: session.id,
            from_sequence: 0,
            columns: 100,
            rows: 30,
        })
        .await
        .unwrap();
    let replay = collect_until(&mut replay_events, session.id, "SYLVOPS_E2E").await;
    assert!(!replay.is_empty());
    second
        .request(&ClientRequest::StopSession {
            session_id: session.id,
        })
        .await
        .unwrap();

    loop {
        let snapshot = match second.request(&ClientRequest::GetSnapshot).await.unwrap() {
            DaemonResponse::Snapshot(snapshot) => snapshot,
            other => panic!("unexpected snapshot response: {other:?}"),
        };
        if snapshot
            .sessions
            .iter()
            .any(|value| value.id == session.id && value.state == SessionState::Terminated)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    second
        .request(&ClientRequest::ShutdownDaemon)
        .await
        .unwrap();
    daemon_task.await.unwrap().unwrap();
}

async fn connect_eventually(paths: &RuntimePaths) -> DaemonClient {
    loop {
        if let Ok(client) = DaemonClient::connect(paths, "phase-two-e2e").await {
            return client;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn collect_until(
    events: &mut tokio::sync::broadcast::Receiver<DaemonEvent>,
    session_id: sylvops_core::ids::SessionId,
    marker: &str,
) -> Vec<u8> {
    let mut output = Vec::new();
    loop {
        if let DaemonEvent::SessionOutput {
            session_id: id,
            bytes,
            ..
        } = events.recv().await.unwrap()
            && id == session_id
        {
            output.extend(bytes);
            if String::from_utf8_lossy(&output).contains(marker) {
                return output;
            }
        }
    }
}

fn initialize_repository(path: &Path) {
    std::fs::create_dir(path).unwrap();
    run_git(path, &["init"]);
    std::fs::write(path.join("README.md"), "test\n").unwrap();
    run_git(path, &["add", "README.md"]);
    run_git(
        path,
        &[
            "-c",
            "user.name=SylvOps Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-m",
            "initial",
        ],
    );
}

fn run_git(path: &Path, arguments: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(arguments)
        .status()
        .unwrap();
    assert!(status.success());
}

#[cfg(windows)]
fn shell_echo_command() -> Vec<u8> {
    b"echo SYLVOPS_E2E\r".to_vec()
}

#[cfg(unix)]
fn shell_echo_command() -> Vec<u8> {
    b"printf 'SYLVOPS_E2E\\n'\n".to_vec()
}
