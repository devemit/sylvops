#![allow(unsafe_code)]

use std::{ffi::OsString, time::Duration};

use sylvops_core::{
    domain::{ProviderKind, Session},
    ids::SessionId,
    protocol::{ClientRequest, DaemonEvent, DaemonResponse},
    session_state::{AttentionReason, ProviderActivity},
};
use sylvops_daemon::{client::DaemonClient, daemon, runtime::RuntimePaths};
use sylvops_test_support::{AbortOnDropTask, TemporaryRepository};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_codex_questions_and_approvals_block_until_answered_and_notify() {
    tokio::time::timeout(Duration::from_secs(60), scenario())
        .await
        .expect("bounded Codex input scenario");
}

#[allow(clippy::too_many_lines)]
async fn scenario() {
    let repository = TemporaryRepository::initialize().await.unwrap();
    let home = repository.root().join("codex-home");
    std::fs::create_dir(&home).unwrap();
    std::fs::write(
        home.join(".fake-backend-child"),
        env!("CARGO_BIN_EXE_fake-agent"),
    )
    .unwrap();
    let heartbeat = home.join("backend-heartbeat");
    let _home = EnvironmentGuard::set("CODEX_HOME", home.as_os_str().to_owned());
    let _executable =
        EnvironmentGuard::set("CODEX_CLI_PATH", env!("CARGO_BIN_EXE_fake-codex").into());
    let paths = RuntimePaths::discover(Some(&repository.root().join("state"))).unwrap();
    let daemon_paths = paths.clone();
    let mut daemon_task = AbortOnDropTask::spawn(daemon::run(daemon_paths));
    let client = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(client) = DaemonClient::connect(&paths, "codex-input-test").await {
                break client;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let workspace_id = match client
        .request(&ClientRequest::AddWorkspace {
            name: "input test".into(),
        })
        .await
        .unwrap()
    {
        DaemonResponse::WorkspaceAdded { workspace, .. } => workspace.id,
        response => panic!("workspace: {response:?}"),
    };
    let worktree_id = match client
        .request(&ClientRequest::AddProject {
            workspace_id,
            repository_path: repository.path().to_string_lossy().into_owned(),
        })
        .await
        .unwrap()
    {
        DaemonResponse::ProjectAdded { root_worktree, .. } => root_worktree.id,
        response => panic!("project: {response:?}"),
    };
    let session_id = match client
        .request(&ClientRequest::CreateSession {
            worktree_id,
            provider: ProviderKind::Codex,
            display_name: Some("Questions".into()),
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
        response => panic!("session: {response:?}"),
    };
    let mut events = client.subscribe();
    assert!(matches!(
        client
            .request(&ClientRequest::AttachSession {
                session_id,
                from_sequence: 0,
                columns: 80,
                rows: 24
            })
            .await
            .unwrap(),
        DaemonResponse::Attached { .. }
    ));
    wait_output(&mut events, session_id, "FAKE_CODEX_READY").await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while !heartbeat.is_file() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("backend descendant started");

    for (command, answer) in [
        ("question", "answer"),
        ("approval-real", "deny"),
        ("file-approval", "cancel"),
        ("permissions-real", "answer"),
        ("elicitation", "answer"),
        ("async-question", "answer"),
    ] {
        input(&client, session_id, command).await;
        wait_output(&mut events, session_id, "FAKE_CODEX_AWAITING_INPUT").await;
        let waiting = wait_status(&client, session_id, |s| {
            s.status.activity == ProviderActivity::AwaitingInput
        })
        .await;
        assert_eq!(
            waiting.status.pending_review().unwrap().reason,
            AttentionReason::InputRequested
        );
        let claim = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let DaemonResponse::AttentionNotificationClaim(Some(claim)) = client
                    .request(&ClientRequest::ClaimAttentionNotification)
                    .await
                    .unwrap()
                {
                    break claim;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("unanswered input produces notification");
        assert_eq!(claim.session_id, session_id);
        assert_eq!(claim.reason, AttentionReason::InputRequested);

        client
            .request(&ClientRequest::AcknowledgeAttention {
                session_id,
                attention_id: waiting.status.attention.unwrap().id,
            })
            .await
            .unwrap();
        assert_eq!(
            snapshot(&client, session_id).await.status.activity,
            ProviderActivity::AwaitingInput
        );
        input(&client, session_id, "continue-without-answer").await;
        wait_output(&mut events, session_id, "FAKE_CODEX_WAITING_FOR_ANSWER").await;
        assert_eq!(
            snapshot(&client, session_id).await.status.activity,
            ProviderActivity::AwaitingInput
        );

        input(&client, session_id, answer).await;
        wait_output(&mut events, session_id, "FAKE_CODEX_CONTINUED").await;
        let working = wait_status(&client, session_id, |s| {
            s.status.activity == ProviderActivity::Working
        })
        .await;
        assert!(working.status.pending_review().is_none());
    }
    client
        .request(&ClientRequest::StopSession { session_id })
        .await
        .unwrap();
    wait_status(&client, session_id, |s| s.process_id.is_none()).await;
    let stopped_heartbeat = std::fs::read(&heartbeat).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        stopped_heartbeat,
        std::fs::read(&heartbeat).unwrap(),
        "backend descendant survived Stop Session"
    );
    backend_failure_retains_cleanup_confirmation(&client, worktree_id).await;
    client
        .request(&ClientRequest::ShutdownDaemon)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), daemon_task.join())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

async fn backend_failure_retains_cleanup_confirmation(
    client: &DaemonClient,
    worktree_id: sylvops_core::ids::WorktreeId,
) {
    let session_id = match client
        .request(&ClientRequest::CreateSession {
            worktree_id,
            provider: ProviderKind::Codex,
            display_name: Some("Backend failure".into()),
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
        response => panic!("session: {response:?}"),
    };
    assert!(matches!(
        client
            .request(&ClientRequest::AttachSession {
                session_id,
                from_sequence: 0,
                columns: 80,
                rows: 24,
            })
            .await
            .unwrap(),
        DaemonResponse::Attached { .. }
    ));
    wait_status(client, session_id, |session| {
        session.external_session_id.is_some()
    })
    .await;
    input(client, session_id, "backend-exit").await;
    wait_status(client, session_id, |session| session.process_id.is_none()).await;
    let authorization_token = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let response = client
                .request(&ClientRequest::InspectSessionDeletion { session_id })
                .await
                .unwrap();
            let DaemonResponse::SessionDeletionInspected(inspection) = response else {
                panic!("deletion inspection: {response:?}");
            };
            if let Some(token) = inspection.authorization_token {
                break token;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("backend failure must retain proof of empty process trees");
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

async fn input(client: &DaemonClient, session_id: SessionId, text: &str) {
    let end = if cfg!(windows) { "\r" } else { "\n" };
    assert!(matches!(
        client
            .request(&ClientRequest::SessionInput {
                session_id,
                bytes: format!("{text}{end}").into_bytes()
            })
            .await
            .unwrap(),
        DaemonResponse::Acknowledged
    ));
}

async fn snapshot(client: &DaemonClient, id: SessionId) -> Session {
    match client.request(&ClientRequest::GetSnapshot).await.unwrap() {
        DaemonResponse::Snapshot(snapshot) => {
            snapshot.sessions.into_iter().find(|s| s.id == id).unwrap()
        }
        response => panic!("snapshot: {response:?}"),
    }
}

async fn wait_status(
    client: &DaemonClient,
    id: SessionId,
    predicate: impl Fn(&Session) -> bool,
) -> Session {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let session = snapshot(client, id).await;
            if predicate(&session) {
                break session;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("expected input state")
}

async fn wait_output(
    events: &mut tokio::sync::broadcast::Receiver<DaemonEvent>,
    id: SessionId,
    expected: &str,
) {
    let mut output = String::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let DaemonEvent::SessionOutput {
                session_id, bytes, ..
            } = events.recv().await.unwrap()
                && session_id == id
            {
                output.push_str(&String::from_utf8_lossy(&bytes));
                assert!(
                    !output.contains("FAKE_CODEX_CONTINUED") || expected == "FAKE_CODEX_CONTINUED",
                    "agent continued before answer"
                );
                if output.contains(expected) {
                    break;
                }
                assert!(output.len() < 64 * 1024, "bounded fixture output");
            }
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!("native terminal did not emit {expected}; offline fixture output: {output:?}")
    });
}

struct EnvironmentGuard {
    name: &'static str,
    previous: Option<OsString>,
}
impl EnvironmentGuard {
    fn set(name: &'static str, value: OsString) -> Self {
        let previous = std::env::var_os(name);
        // This test is the only test in its executable; configure before spawning the daemon.
        unsafe {
            std::env::set_var(name, value);
        }
        Self { name, previous }
    }
}
impl Drop for EnvironmentGuard {
    fn drop(&mut self) {
        // The daemon and its children have stopped before restoring fixture configuration.
        unsafe {
            if let Some(previous) = &self.previous {
                std::env::set_var(self.name, previous);
            } else {
                std::env::remove_var(self.name);
            }
        }
    }
}
