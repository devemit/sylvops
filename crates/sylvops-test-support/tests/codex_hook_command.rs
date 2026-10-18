use std::{path::Path, process::Stdio, time::Duration};

use sylvops_core::{
    domain::{ProviderKind, Session},
    ids::SessionId,
    protocol::{ClientRequest, DaemonResponse},
};
use sylvops_daemon::{client::DaemonClient, runtime::RuntimePaths};
use sylvops_test_support::{TemporaryRepository, bounded};

#[tokio::test]
async fn generated_codex_hook_captures_and_persists_conversation_identity() {
    tokio::time::timeout(
        Duration::from_secs(60),
        exercise_generated_hook("SylvOps é Ж with spaces", false),
    )
    .await
    .expect("generated Codex hook scenario timed out");
}

#[cfg(windows)]
#[tokio::test]
async fn generated_codex_hook_refuses_environment_expansion_in_relay_paths() {
    tokio::time::timeout(Duration::from_secs(60), async {
        for directory in ["SylvOps %PATH%", "SylvOps !PATH!"] {
            exercise_generated_hook(directory, true).await;
        }
    })
    .await
    .expect("unsafe hook path scenario timed out");
}

#[allow(clippy::too_many_lines)]
async fn exercise_generated_hook(directory: &str, expect_refusal: bool) {
    let repository = TemporaryRepository::initialize().await.unwrap();
    let bin = repository.root().join(directory);
    std::fs::create_dir(&bin).unwrap();
    let relay = bin.join(if cfg!(windows) {
        "sylvops.exe"
    } else {
        "sylvops"
    });
    std::fs::copy(env!("CARGO_BIN_EXE_hook-daemon"), &relay).unwrap();
    let codex_home = repository.root().join("codex home é Ж");
    std::fs::create_dir(&codex_home).unwrap();
    std::fs::write(codex_home.join(".fake-codex-execute-hooks"), b"").unwrap();
    let state_root = repository.root().join("state");
    let paths = RuntimePaths::discover(Some(&state_root)).unwrap();

    let mut daemon = start_daemon(&relay, &codex_home, &state_root);
    let client = connect(&paths).await;
    let workspace_id = match client
        .request(&ClientRequest::AddWorkspace {
            name: "hooks".into(),
        })
        .await
        .unwrap()
    {
        DaemonResponse::WorkspaceAdded { workspace, .. } => workspace.id,
        response => panic!("unexpected workspace response: {response:?}"),
    };
    let worktree_id = match client
        .request(&ClientRequest::AddProject {
            workspace_id,
            repository_path: repository.path().to_str().unwrap().into(),
        })
        .await
        .unwrap()
    {
        DaemonResponse::ProjectAdded { root_worktree, .. } => root_worktree.id,
        response => panic!("unexpected Project response: {response:?}"),
    };
    let created = client
        .request(&ClientRequest::CreateSession {
            worktree_id,
            provider: ProviderKind::Codex,
            display_name: Some("generated hooks".into()),
            model: Some("model & untrusted repository data".into()),
            effort: None,
            initial_prompt: Some("untrusted prompt \" & echo extra-arguments".into()),
            columns: 80,
            rows: 24,
        })
        .await
        .unwrap();
    if expect_refusal {
        assert!(
            matches!(created, DaemonResponse::Error(ref error)
            if error.message.contains("hook relay path contains unsafe characters")),
            "unsafe command path was not refused: {created:?}"
        );
        client
            .request(&ClientRequest::ShutdownDaemon)
            .await
            .unwrap();
        assert!(bounded(daemon.wait()).await.unwrap().unwrap().success());
        return;
    }
    let session_id = match created {
        DaemonResponse::SessionCreated { session, .. } => session.id,
        response => panic!("unexpected Session response: {response:?}"),
    };
    let expected_id = format!("fake-{session_id}");
    bounded(async {
        loop {
            let session = session(&client, session_id).await;
            if session.external_session_id.as_deref() == Some(&expected_id) {
                break;
            }
            assert!(
                session.process_id.is_some(),
                "fake Codex exited before identity capture: {session:?}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("generated command did not deliver verified Conversation identity");

    client
        .request(&ClientRequest::StopSession { session_id })
        .await
        .unwrap();
    bounded(async {
        loop {
            if session(&client, session_id).await.status.lifecycle
                == sylvops_core::session_state::SessionLifecycle::Terminated
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    client
        .request(&ClientRequest::ShutdownDaemon)
        .await
        .unwrap();
    assert!(bounded(daemon.wait()).await.unwrap().unwrap().success());
    drop(client);

    let mut restarted = start_daemon(&relay, &codex_home, &state_root);
    let client = connect(&paths).await;
    assert_eq!(
        session(&client, session_id)
            .await
            .external_session_id
            .as_deref(),
        Some(expected_id.as_str())
    );
    client
        .request(&ClientRequest::ShutdownDaemon)
        .await
        .unwrap();
    assert!(bounded(restarted.wait()).await.unwrap().unwrap().success());
}

fn start_daemon(relay: &Path, codex_home: &Path, state_root: &Path) -> tokio::process::Child {
    let mut command = tokio::process::Command::new(relay);
    command
        .arg("daemon")
        .arg(state_root)
        .env("CODEX_CLI_PATH", env!("CARGO_BIN_EXE_fake-codex"))
        .env("CODEX_HOME", codex_home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x0800_0000);
    command.spawn().expect("start hook daemon fixture")
}

async fn connect(paths: &RuntimePaths) -> DaemonClient {
    bounded(async {
        loop {
            if let Ok(client) = DaemonClient::connect(paths, "generated-hook-test").await {
                return client;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("hook daemon did not accept IPC")
}

async fn session(client: &DaemonClient, id: SessionId) -> Session {
    match client.request(&ClientRequest::GetSnapshot).await.unwrap() {
        DaemonResponse::Snapshot(snapshot) => {
            snapshot.sessions.into_iter().find(|s| s.id == id).unwrap()
        }
        response => panic!("unexpected snapshot response: {response:?}"),
    }
}
