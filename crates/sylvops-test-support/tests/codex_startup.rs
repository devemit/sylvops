use std::{process::Stdio, time::Duration};

use sylvops_core::{
    domain::ProviderKind,
    protocol::{ClientRequest, DaemonResponse},
};
use sylvops_daemon::{client::DaemonClient, runtime::RuntimePaths};
use sylvops_test_support::TemporaryRepository;

/// No prompt is submitted and no trust or permission setting is changed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "opt-in native startup: set SYLVOPS_REAL_CODEX_PATH and SYLVOPS_REAL_CODEX_REPOSITORY to an already trusted repository"]
async fn native_codex_remains_running_after_startup() {
    tokio::time::timeout(Duration::from_secs(40), startup())
        .await
        .expect("bounded native startup");
}

async fn startup() {
    let executable = std::env::var_os("SYLVOPS_REAL_CODEX_PATH")
        .map(std::path::PathBuf::from)
        .expect("explicit native Codex path");
    let cwd = std::env::var_os("SYLVOPS_REAL_CODEX_REPOSITORY")
        .map(std::path::PathBuf::from)
        .expect("explicit trusted repository");
    assert!(executable.is_absolute() && executable.is_file() && cwd.is_absolute() && cwd.is_dir());
    let repository = TemporaryRepository::initialize().await.unwrap();
    let state = repository.root().join("state");
    let paths = RuntimePaths::discover(Some(&state)).unwrap();
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_hook-daemon"));
    command
        .arg("daemon")
        .arg(&state)
        .env("CODEX_CLI_PATH", executable)
        .env("TERM", "xterm-256color")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x0800_0000);
    let mut daemon = command.spawn().unwrap();
    let client = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(client) = DaemonClient::connect(&paths, "startup-test").await {
                break client;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    let session = create_session(&client, &cwd).await;
    assert!(matches!(
        client
            .request(&ClientRequest::AttachSession {
                session_id: session.id,
                from_sequence: 0,
                columns: 100,
                rows: 30
            })
            .await
            .unwrap(),
        DaemonResponse::Attached { .. }
    ));
    tokio::time::sleep(Duration::from_secs(10)).await;
    let DaemonResponse::Snapshot(snapshot) =
        client.request(&ClientRequest::GetSnapshot).await.unwrap()
    else {
        panic!("snapshot");
    };
    let current = snapshot
        .sessions
        .iter()
        .find(|s| s.id == session.id)
        .unwrap();
    let alive = current.process_id.is_some() && current.external_session_id.is_some();
    let input = client
        .request(&ClientRequest::SessionInput {
            session_id: session.id,
            bytes: vec![12],
        })
        .await
        .unwrap();
    client
        .request(&ClientRequest::ShutdownDaemon)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), daemon.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(
        alive,
        "native Codex stopped during startup or did not establish a Conversation"
    );
    assert!(
        matches!(input, DaemonResponse::Acknowledged),
        "native terminal input became unavailable"
    );
}

async fn create_session(
    client: &DaemonClient,
    cwd: &std::path::Path,
) -> sylvops_core::domain::Session {
    let DaemonResponse::WorkspaceAdded { workspace, .. } = client
        .request(&ClientRequest::AddWorkspace {
            name: "startup test".into(),
        })
        .await
        .unwrap()
    else {
        panic!("workspace");
    };
    let DaemonResponse::ProjectAdded { root_worktree, .. } = client
        .request(&ClientRequest::AddProject {
            workspace_id: workspace.id,
            repository_path: cwd.to_str().unwrap().into(),
        })
        .await
        .unwrap()
    else {
        panic!("project");
    };
    let response = client
        .request(&ClientRequest::CreateSession {
            worktree_id: root_worktree.id,
            provider: ProviderKind::Codex,
            display_name: None,
            model: None,
            effort: None,
            initial_prompt: None,
            columns: 100,
            rows: 30,
        })
        .await
        .unwrap();
    let DaemonResponse::SessionCreated { session, .. } = response else {
        panic!("native Session creation failed");
    };
    session
}
