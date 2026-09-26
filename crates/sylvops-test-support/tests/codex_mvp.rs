#![allow(unsafe_code)]

use std::{ffi::OsString, path::Path, process::Command, time::Duration};

use rusqlite::OptionalExtension;
use sylvops_core::{
    domain::{ProviderKind, Session, SessionState},
    ids::{SessionId, WorktreeId},
    protocol::{ClientRequest, DaemonResponse},
};
use sylvops_daemon::{client::DaemonClient, daemon, runtime::RuntimePaths};

const PROMPT_SENTINEL: &str = "SYLVOPS_PRIVATE_PROMPT_25_7f8cbfea";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::too_many_lines)]
async fn fake_codex_hooks_attention_and_resume() {
    tokio::time::timeout(
        Duration::from_secs(120),
        fake_codex_hooks_attention_and_resume_inner(),
    )
    .await
    .expect("fake Codex scenario timeout");
}

#[allow(clippy::too_many_lines)]
async fn fake_codex_hooks_attention_and_resume_inner() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let repository = temporary.path().join("repository");
    initialize_repository(&repository);
    let bin_directory = temporary.path().join("bin");
    std::fs::create_dir(&bin_directory).expect("fake provider bin directory");
    install_fake_codex(&bin_directory);
    let _path =
        EnvironmentGuard::set(
            "PATH",
            std::env::join_paths(std::iter::once(bin_directory.clone()).chain(
                std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
            ))
            .expect("test PATH"),
        );
    let _codex_home = EnvironmentGuard::set("CODEX_HOME", temporary.path().join("codex-home"));

    let paths = RuntimePaths::discover(Some(&temporary.path().join("state"))).expect("paths");
    let daemon_paths = paths.clone();
    let mut daemon_task = tokio::spawn(async move { daemon::run(daemon_paths).await });
    let client = tokio::select! {
        result = &mut daemon_task => panic!("daemon exited before connection: {result:?}"),
        client = connect_eventually(&paths) => client,
    };

    let workspace_id = match client
        .request(&ClientRequest::AddWorkspace {
            name: "codex-mvp".into(),
        })
        .await
        .expect("workspace response")
    {
        DaemonResponse::WorkspaceAdded { workspace, .. } => workspace.id,
        response => panic!("unexpected workspace response: {response:?}"),
    };
    let worktree_id = match client
        .request(&ClientRequest::AddProject {
            workspace_id,
            repository_path: repository.to_string_lossy().into_owned(),
        })
        .await
        .expect("project response")
    {
        DaemonResponse::ProjectAdded { root_worktree, .. } => root_worktree.id,
        response => panic!("unexpected project response: {response:?}"),
    };
    let shell_session_id = create_shell_session(&client, worktree_id).await;
    match client
        .request(&ClientRequest::ResumeSession {
            session_id: shell_session_id,
            columns: 80,
            rows: 24,
        })
        .await
        .expect("missing resume ID response")
    {
        DaemonResponse::Error(error) => assert!(
            error
                .message
                .contains("no verified provider resume identifier"),
            "missing resume ID error should be actionable: {}",
            error.message
        ),
        response => panic!("session without resume ID unexpectedly resumed: {response:?}"),
    }
    client
        .request(&ClientRequest::StopSession {
            session_id: shell_session_id,
        })
        .await
        .expect("stop shell response");

    let terminated_session_id = create_codex_session(&client, worktree_id).await;
    attach(&client, terminated_session_id).await;
    client
        .request(&ClientRequest::SessionInput {
            session_id: terminated_session_id,
            bytes: input_line("permission"),
        })
        .await
        .expect("terminated-session permission input");
    wait_for_session(&client, terminated_session_id, |session| {
        session.state == SessionState::NeedsFeedback && session.external_session_id.is_some()
    })
    .await;
    client
        .request(&ClientRequest::StopSession {
            session_id: terminated_session_id,
        })
        .await
        .expect("terminate response");
    wait_for_session(&client, terminated_session_id, |session| {
        session.state == SessionState::Terminated
    })
    .await;
    match client
        .request(&ClientRequest::ResumeSession {
            session_id: terminated_session_id,
            columns: 80,
            rows: 24,
        })
        .await
        .expect("terminated resume response")
    {
        DaemonResponse::Error(error) => assert!(
            error.message.contains("not eligible for resume"),
            "terminated resume error should be actionable: {}",
            error.message
        ),
        response => panic!("terminated session unexpectedly resumed: {response:?}"),
    }

    let source_session_id = create_codex_session(&client, worktree_id).await;
    attach(&client, source_session_id).await;
    client
        .request(&ClientRequest::SessionInput {
            session_id: source_session_id,
            bytes: input_line("permission"),
        })
        .await
        .expect("permission input");
    let waiting = wait_for_session(&client, source_session_id, |session| {
        session.state == SessionState::NeedsFeedback && session.external_session_id.is_some()
    })
    .await;
    let external_id = waiting.external_session_id.expect("external Codex ID");

    client
        .request(&ClientRequest::SessionInput {
            session_id: source_session_id,
            bytes: input_line("finish"),
        })
        .await
        .expect("finish input");
    wait_for_session(&client, source_session_id, |session| {
        session.process_id.is_none()
            && matches!(
                session.state,
                SessionState::FinishedUnseen | SessionState::FinishedSeen
            )
    })
    .await;

    let first_resume = ClientRequest::ResumeSession {
        session_id: source_session_id,
        columns: 80,
        rows: 24,
    };
    let concurrent_resume = first_resume.clone();
    let (first_response, concurrent_response) = tokio::join!(
        client.request(&first_resume),
        client.request(&concurrent_resume)
    );
    let mut resumed_id = None;
    let mut refused = 0;
    for response in [first_response, concurrent_response] {
        match response.expect("concurrent resume response") {
            DaemonResponse::SessionResumed { session, .. } => {
                assert_eq!(
                    session.external_session_id.as_deref(),
                    Some(external_id.as_str())
                );
                assert!(resumed_id.replace(session.id).is_none());
            }
            DaemonResponse::Error(error) => {
                assert!(
                    error.message.contains("already been resumed"),
                    "concurrent resume error should be actionable: {}",
                    error.message
                );
                refused += 1;
            }
            response => panic!("unexpected concurrent resume response: {response:?}"),
        }
    }
    assert_eq!(refused, 1);
    let resumed_id = resumed_id.expect("one concurrent resume succeeds");
    match client
        .request(&ClientRequest::ResumeSession {
            session_id: source_session_id,
            columns: 80,
            rows: 24,
        })
        .await
        .expect("stale resume response")
    {
        DaemonResponse::Error(error) => assert!(
            error.message.contains("already been resumed"),
            "stale resume error should be actionable: {}",
            error.message
        ),
        response => panic!("stale resume unexpectedly succeeded: {response:?}"),
    }
    let history = match client
        .request(&ClientRequest::GetSnapshot)
        .await
        .expect("history snapshot response")
    {
        DaemonResponse::Snapshot(snapshot) => snapshot,
        response => panic!("unexpected history snapshot response: {response:?}"),
    };
    let source_history = history
        .sessions
        .iter()
        .find(|session| session.id == source_session_id)
        .expect("historical source session");
    assert!(matches!(
        source_history.state,
        SessionState::FinishedUnseen | SessionState::FinishedSeen
    ));
    assert_eq!(
        source_history.external_session_id.as_deref(),
        Some(external_id.as_str())
    );
    assert!(
        history
            .sessions
            .iter()
            .any(|session| session.id == resumed_id)
    );
    attach(&client, resumed_id).await;
    client
        .request(&ClientRequest::SessionInput {
            session_id: resumed_id,
            bytes: input_line("finish"),
        })
        .await
        .expect("resumed finish input");
    wait_for_session(&client, resumed_id, |session| session.process_id.is_none()).await;

    client
        .request(&ClientRequest::ShutdownDaemon)
        .await
        .expect("shutdown response");
    tokio::time::timeout(Duration::from_secs(10), daemon_task)
        .await
        .expect("daemon shutdown timeout")
        .expect("daemon task")
        .expect("daemon result");

    let restart_paths = paths.clone();
    let mut restart_task = tokio::spawn(async move { daemon::run(restart_paths).await });
    let restarted = tokio::select! {
        result = &mut restart_task => panic!("restarted daemon exited before connection: {result:?}"),
        client = connect_eventually(&paths) => client,
    };
    let snapshot = match restarted
        .request(&ClientRequest::GetSnapshot)
        .await
        .expect("restart snapshot response")
    {
        DaemonResponse::Snapshot(snapshot) => snapshot,
        response => panic!("unexpected restart response: {response:?}"),
    };
    assert!(snapshot.sessions.iter().any(|session| {
        session.id == source_session_id
            && session.external_session_id.as_deref() == Some(external_id.as_str())
    }));
    match restarted
        .request(&ClientRequest::ResumeSession {
            session_id: source_session_id,
            columns: 80,
            rows: 24,
        })
        .await
        .expect("restart stale resume response")
    {
        DaemonResponse::Error(error) => assert!(error.message.contains("already been resumed")),
        response => panic!("restart stale resume unexpectedly succeeded: {response:?}"),
    }
    restarted
        .request(&ClientRequest::ShutdownDaemon)
        .await
        .expect("restart shutdown response");
    tokio::time::timeout(Duration::from_secs(10), restart_task)
        .await
        .expect("restart daemon shutdown timeout")
        .expect("restart daemon task")
        .expect("restart daemon result");
    assert_no_persisted_text_contains(&paths.database, PROMPT_SENTINEL);
}

async fn create_codex_session(client: &DaemonClient, worktree_id: WorktreeId) -> SessionId {
    match client
        .request(&ClientRequest::CreateSession {
            worktree_id,
            provider: ProviderKind::Codex,
            display_name: Some("fake Codex".into()),
            model: Some("fake-model".into()),
            effort: Some("high".into()),
            initial_prompt: Some(PROMPT_SENTINEL.into()),
            columns: 80,
            rows: 24,
        })
        .await
        .expect("session response")
    {
        DaemonResponse::SessionCreated { session, .. } => session.id,
        response => panic!("unexpected session response: {response:?}"),
    }
}

async fn create_shell_session(client: &DaemonClient, worktree_id: WorktreeId) -> SessionId {
    match client
        .request(&ClientRequest::CreateSession {
            worktree_id,
            provider: ProviderKind::Shell,
            display_name: Some("plain shell".into()),
            model: None,
            effort: None,
            initial_prompt: None,
            columns: 80,
            rows: 24,
        })
        .await
        .expect("shell session response")
    {
        DaemonResponse::SessionCreated { session, .. } => session.id,
        response => panic!("unexpected shell session response: {response:?}"),
    }
}

fn assert_no_persisted_text_contains(path: &Path, sentinel: &str) {
    let connection = rusqlite::Connection::open(path).expect("open persisted database");
    let tables = connection
        .prepare(
            "SELECT name FROM sqlite_schema \
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .expect("prepare table query")
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query tables")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("collect tables");

    for table in tables {
        let quoted_table = quote_identifier(&table);
        let columns = connection
            .prepare(&format!("PRAGMA table_info({quoted_table})"))
            .expect("prepare column query")
            .query_map([], |row| row.get::<_, String>(1))
            .expect("query columns")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("collect columns");
        for column in columns {
            let quoted_column = quote_identifier(&column);
            let query = format!(
                "SELECT {quoted_column} FROM {quoted_table} \
                 WHERE typeof({quoted_column}) = 'text' AND instr({quoted_column}, ?1) > 0"
            );
            let found = connection
                .query_row(&query, [sentinel], |row| row.get::<_, String>(0))
                .optional()
                .expect("scan persisted text");
            assert!(
                found.is_none(),
                "prompt sentinel persisted in {table}.{column}"
            );
        }
    }
}

fn quote_identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

async fn attach(client: &DaemonClient, session_id: SessionId) {
    assert!(matches!(
        client
            .request(&ClientRequest::AttachSession {
                session_id,
                from_sequence: 0,
                columns: 80,
                rows: 24,
            })
            .await
            .expect("attach response"),
        DaemonResponse::Attached { .. }
    ));
}

async fn wait_for_session(
    client: &DaemonClient,
    session_id: SessionId,
    predicate: impl Fn(&Session) -> bool,
) -> Session {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut last_observed = None;
    loop {
        if let DaemonResponse::Snapshot(snapshot) = client
            .request(&ClientRequest::GetSnapshot)
            .await
            .expect("snapshot response")
            && let Some(session) = snapshot
                .sessions
                .into_iter()
                .find(|session| session.id == session_id)
        {
            if predicate(&session) {
                return session;
            }
            last_observed = Some(session);
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "session state timeout; last observed: {last_observed:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn connect_eventually(paths: &RuntimePaths) -> DaemonClient {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(client) = DaemonClient::connect(paths, "fake-codex-test").await {
                return client;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("daemon connection timeout")
}

fn install_fake_codex(directory: &Path) {
    let target = directory.join(if cfg!(windows) { "codex.exe" } else { "codex" });
    let source = std::env::var_os("SYLVOPS_TEST_FAKE_CODEX").map_or_else(
        || std::path::PathBuf::from(env!("CARGO_BIN_EXE_fake-codex")),
        std::path::PathBuf::from,
    );
    std::fs::copy(source, &target).expect("copy fake Codex");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&target)
            .expect("fake metadata")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(target, permissions).expect("fake executable permissions");
    }
}

#[cfg(windows)]
fn input_line(value: &str) -> Vec<u8> {
    format!("{value}\r").into_bytes()
}

#[cfg(unix)]
fn input_line(value: &str) -> Vec<u8> {
    format!("{value}\n").into_bytes()
}

fn initialize_repository(path: &Path) {
    std::fs::create_dir(path).expect("repository directory");
    run_git(path, &["init", "--initial-branch=main"]);
    run_git(path, &["config", "user.name", "SylvOps Test"]);
    run_git(path, &["config", "user.email", "sylvops@example.invalid"]);
    std::fs::write(path.join("README.md"), "fixture\n").expect("fixture file");
    run_git(path, &["add", "README.md"]);
    run_git(path, &["commit", "-m", "fixture"]);
}

fn run_git(path: &Path, arguments: &[&str]) {
    let status = Command::new("git")
        .args(arguments)
        .current_dir(path)
        .status()
        .expect("run git");
    assert!(status.success(), "git {arguments:?} failed");
}

struct EnvironmentGuard {
    name: &'static str,
    previous: Option<OsString>,
}

impl EnvironmentGuard {
    fn set(name: &'static str, value: impl Into<OsString>) -> Self {
        let previous = std::env::var_os(name);
        // SAFETY: this test serializes all environment changes in its process and restores them.
        unsafe { std::env::set_var(name, value.into()) };
        Self { name, previous }
    }
}

impl Drop for EnvironmentGuard {
    fn drop(&mut self) {
        // SAFETY: this test serializes all environment changes in its process and restores them.
        unsafe {
            if let Some(previous) = self.previous.take() {
                std::env::set_var(self.name, previous);
            } else {
                std::env::remove_var(self.name);
            }
        }
    }
}
