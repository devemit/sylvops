use std::{path::Path, process::Command, time::Duration};

use sylvops_core::{
    ids::SessionId,
    protocol::{ClientRequest, DaemonEvent, DaemonResponse},
};
use sylvops_daemon::{client::DaemonClient, daemon, runtime::RuntimePaths};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_managed_worktrees_run_independent_sessions_and_remove_cleanly() {
    tokio::time::timeout(Duration::from_secs(40), exercise_two_worktrees())
        .await
        .expect("Phase 3 end-to-end timeout");
}

async fn exercise_two_worktrees() {
    let temporary = tempfile::tempdir().unwrap();
    let repository = temporary.path().join("repository");
    initialize_repository(&repository);
    let paths = RuntimePaths::discover(Some(&temporary.path().join("state"))).unwrap();
    let daemon_paths = paths.clone();
    let daemon_task = tokio::spawn(async move { daemon::run(daemon_paths).await });
    let client = connect_eventually(&paths).await;

    let workspace = match client
        .request(&ClientRequest::AddWorkspace {
            name: "phase-three".into(),
        })
        .await
        .unwrap()
    {
        DaemonResponse::WorkspaceAdded { workspace, .. } => workspace,
        other => panic!("unexpected workspace response: {other:?}"),
    };
    let project = match client
        .request(&ClientRequest::AddProject {
            workspace_id: workspace.id,
            repository_path: repository.to_string_lossy().into_owned(),
        })
        .await
        .unwrap()
    {
        DaemonResponse::ProjectAdded { project, .. } => project,
        other => panic!("unexpected project response: {other:?}"),
    };

    let first = create_worktree(&client, project.id, "feature/one").await;
    let second = create_worktree(&client, project.id, "feature/two").await;
    let first_session = create_session(&client, first.id, "first").await;
    let second_session = create_session(&client, second.id, "second").await;
    let mut events = client.subscribe();
    attach_and_echo(&client, first_session, "SYLVOPS_FIRST").await;
    collect_until(&mut events, first_session, "SYLVOPS_FIRST").await;
    attach_and_echo(&client, second_session, "SYLVOPS_SECOND").await;
    collect_until(&mut events, second_session, "SYLVOPS_SECOND").await;

    let live_status = worktree_status(&client, first.id).await;
    let refusal = client
        .request(&ClientRequest::RemoveWorktree {
            worktree_id: first.id,
            confirmation_token: live_status.removal_confirmation_token.unwrap(),
        })
        .await
        .unwrap();
    assert!(matches!(
        refusal,
        DaemonResponse::Error(ref failure) if failure.code == "git_validation_failed"
    ));
    assert!(Path::new(&first.canonical_path).exists());

    for session_id in [first_session, second_session] {
        client
            .request(&ClientRequest::StopSession { session_id })
            .await
            .unwrap();
    }
    for worktree in [&first, &second] {
        let status = worktree_status(&client, worktree.id).await;
        assert!(status.clean);
        client
            .request(&ClientRequest::RemoveWorktree {
                worktree_id: worktree.id,
                confirmation_token: status.removal_confirmation_token.unwrap(),
            })
            .await
            .unwrap();
        assert!(!Path::new(&worktree.canonical_path).exists());
    }
    assert!(branch_exists(&repository, "feature/one"));
    assert!(branch_exists(&repository, "feature/two"));
    let snapshot = match client.request(&ClientRequest::GetSnapshot).await.unwrap() {
        DaemonResponse::Snapshot(snapshot) => snapshot,
        other => panic!("unexpected snapshot response: {other:?}"),
    };
    for worktree_id in [first.id, second.id] {
        assert!(snapshot.worktrees.iter().any(|worktree| {
            worktree.id == worktree_id
                && worktree.status == sylvops_core::domain::WorktreeStatus::Removed
        }));
    }
    for session_id in [first_session, second_session] {
        assert!(snapshot.sessions.iter().any(|session| {
            session.id == session_id
                && session.state == sylvops_core::domain::SessionState::Terminated
        }));
    }

    client
        .request(&ClientRequest::ShutdownDaemon)
        .await
        .unwrap();
    daemon_task.await.unwrap().unwrap();
}

async fn worktree_status(
    client: &DaemonClient,
    worktree_id: sylvops_core::ids::WorktreeId,
) -> sylvops_core::domain::GitWorktreeState {
    match client
        .request(&ClientRequest::GetWorktreeStatus { worktree_id })
        .await
        .unwrap()
    {
        DaemonResponse::WorktreeStatus(status) => status,
        other => panic!("unexpected status response: {other:?}"),
    }
}

async fn create_worktree(
    client: &DaemonClient,
    project_id: sylvops_core::ids::ProjectId,
    branch: &str,
) -> sylvops_core::domain::Worktree {
    match client
        .request(&ClientRequest::CreateWorktree {
            project_id,
            name: None,
            branch: branch.into(),
            base_ref: Some("HEAD".into()),
        })
        .await
        .unwrap()
    {
        DaemonResponse::WorktreeCreated { worktree, .. } => worktree,
        other => panic!("unexpected worktree response: {other:?}"),
    }
}

async fn create_session(
    client: &DaemonClient,
    worktree_id: sylvops_core::ids::WorktreeId,
    name: &str,
) -> SessionId {
    match client
        .request(&ClientRequest::CreateSession {
            worktree_id,
            provider: sylvops_core::domain::ProviderKind::Shell,
            display_name: Some(name.into()),
            model: None,
            effort: None,
            initial_prompt: None,
            columns: 80,
            rows: 24,
        })
        .await
        .unwrap()
    {
        DaemonResponse::SessionCreated { session, .. } => session.id,
        other => panic!("unexpected session response: {other:?}"),
    }
}

async fn attach_and_echo(client: &DaemonClient, session_id: SessionId, marker: &str) {
    client
        .request(&ClientRequest::AttachSession {
            session_id,
            from_sequence: 0,
            columns: 80,
            rows: 24,
        })
        .await
        .unwrap();
    client
        .request(&ClientRequest::SessionInput {
            session_id,
            bytes: shell_echo_command(marker),
        })
        .await
        .unwrap();
}

async fn collect_until(
    events: &mut tokio::sync::broadcast::Receiver<DaemonEvent>,
    session_id: SessionId,
    marker: &str,
) {
    let mut output = Vec::new();
    loop {
        if let DaemonEvent::SessionOutput {
            session_id: id,
            bytes,
            ..
        } = events.recv().await.unwrap()
            && id == session_id
        {
            output.extend_from_slice(&bytes);
            if String::from_utf8_lossy(&output).contains(marker) {
                return;
            }
        }
    }
}

async fn connect_eventually(paths: &RuntimePaths) -> DaemonClient {
    loop {
        if let Ok(client) = DaemonClient::connect(paths, "phase-three-e2e").await {
            return client;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
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

fn branch_exists(path: &Path, branch: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["show-ref", "--verify", "--quiet"])
        .arg(format!("refs/heads/{branch}"))
        .status()
        .unwrap()
        .success()
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
fn shell_echo_command(marker: &str) -> Vec<u8> {
    format!("echo {marker}\r").into_bytes()
}

#[cfg(unix)]
fn shell_echo_command(marker: &str) -> Vec<u8> {
    format!("printf '{marker}\\n'\n").into_bytes()
}
