use std::{path::Path, time::Duration};

use rusqlite::OptionalExtension;
use sylvops_core::{
    domain::{ProviderKind, Session, SessionState},
    ids::{SessionId, WorktreeId},
    protocol::{ClientRequest, DaemonEvent, DaemonResponse},
    status::{ConversationIdentityTransition, NormalizedProviderEvent},
};
use sylvops_daemon::{
    client::DaemonClient, daemon, provider::ProviderRegistry, runtime::RuntimePaths,
};
use sylvops_test_support::TemporaryRepository;

const SMOKE_DEADLINE: Duration = Duration::from_secs(300);
const STATE_DEADLINE: Duration = Duration::from_secs(90);
const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(15);
const INPUT_PROMPT: &str = "Use AskUserQuestion to ask whether the SylvOps smoke test should continue, with Yes and No choices. After the answer, reply briefly and finish.";
const PERMISSION_PROMPT: &str = "Use the Bash tool once to run pwd. Wait for permission when requested. After the result, reply briefly and finish.";
const RESUME_PROMPT: &str =
    "Reply briefly that the resumed SylvOps smoke Session is usable, then finish.";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires SYLVOPS_RUN_REAL_CLAUDE_SMOKE=1 and a pre-authenticated Claude Code account"]
async fn real_claude_account_smoke_is_bounded_and_redacted() {
    assert_eq!(
        std::env::var("SYLVOPS_RUN_REAL_CLAUDE_SMOKE").as_deref(),
        Ok("1"),
        "set SYLVOPS_RUN_REAL_CLAUDE_SMOKE=1 to acknowledge the real-account smoke test"
    );

    let started = tokio::time::Instant::now();
    let registry = ProviderRegistry::new(None, &[ProviderKind::Claude])
        .expect("create real Claude provider registry");
    let health = tokio::time::timeout(
        Duration::from_secs(15),
        registry.probe(ProviderKind::Claude),
    )
    .await
    .expect("real Claude probe timeout")
    .expect("probe real Claude Code");
    assert!(health.available, "Claude Code is not available");
    assert!(health.authenticated, "Claude Code is not authenticated");
    assert!(
        health
            .executable_path
            .as_deref()
            .is_some_and(|path| Path::new(path).is_absolute()),
        "Claude Code did not resolve to an absolute native executable"
    );
    assert!(
        health.version.is_some(),
        "Claude Code version is unavailable"
    );

    let repository =
        tokio::time::timeout(Duration::from_secs(45), TemporaryRepository::initialize())
            .await
            .expect("real Claude repository setup timeout")
            .expect("temporary smoke repository");
    let paths = RuntimePaths::discover(Some(&repository.root().join("state")))
        .expect("discover smoke runtime paths");
    paths.prepare().expect("prepare smoke runtime paths");
    std::fs::write(
        &paths.config,
        "version = 2\nupdate_check_policy = 'disabled'\n",
    )
    .expect("write smoke configuration");

    let daemon_paths = paths.clone();
    let mut daemon_task = tokio::spawn(async move { daemon::run(daemon_paths).await });
    let client = tokio::select! {
        result = &mut daemon_task => panic!("daemon exited before smoke connection: {result:?}"),
        client = connect_eventually(&paths) => client,
    };

    let scenario_deadline = SMOKE_DEADLINE
        .checked_sub(started.elapsed().saturating_add(SHUTDOWN_DEADLINE))
        .expect("real Claude setup exhausted the smoke deadline");
    let smoke = tokio::time::timeout(
        scenario_deadline,
        run_real_smoke(&client, &paths, repository.path()),
    )
    .await;
    let shutdown = tokio::time::timeout(
        Duration::from_secs(5),
        client.request(&ClientRequest::ShutdownDaemon),
    )
    .await;
    let daemon_exit = tokio::time::timeout(Duration::from_secs(10), daemon_task).await;

    shutdown
        .expect("smoke daemon shutdown request timeout")
        .expect("request smoke daemon shutdown");
    daemon_exit
        .expect("smoke daemon shutdown timeout")
        .expect("smoke daemon task")
        .expect("smoke daemon result");
    smoke
        .expect("real Claude smoke exceeded its five-minute deadline")
        .expect("real Claude smoke scenario");

    println!(
        "real Claude smoke passed: probe, concurrency, permission, input, completion, resume, cleanup"
    );
}

#[allow(clippy::too_many_lines)]
async fn run_real_smoke(
    client: &DaemonClient,
    paths: &RuntimePaths,
    repository: &Path,
) -> Result<(), String> {
    let workspace_id = match client
        .request(&ClientRequest::AddWorkspace {
            name: "real-claude-smoke".into(),
        })
        .await
        .map_err(|error| error.to_string())?
    {
        DaemonResponse::WorkspaceAdded { workspace, .. } => workspace.id,
        response => return Err(format!("unexpected workspace response: {response:?}")),
    };
    let worktree_id = match client
        .request(&ClientRequest::AddProject {
            workspace_id,
            repository_path: repository.to_string_lossy().into_owned(),
        })
        .await
        .map_err(|error| error.to_string())?
    {
        DaemonResponse::ProjectAdded { root_worktree, .. } => root_worktree.id,
        response => return Err(format!("unexpected project response: {response:?}")),
    };

    let mut events = client.subscribe();
    let (input_session, permission_session) = tokio::join!(
        create_session(client, worktree_id, "Claude input smoke", INPUT_PROMPT),
        create_session(
            client,
            worktree_id,
            "Claude permission smoke",
            PERMISSION_PROMPT
        )
    );
    let input_session = input_session?;
    let permission_session = permission_session?;
    if input_session.arguments_json.contains(INPUT_PROMPT)
        || permission_session
            .arguments_json
            .contains(PERMISSION_PROMPT)
    {
        return Err("a real Claude prompt was persisted in launch arguments".into());
    }

    let (input_running, permission_running) = tokio::join!(
        wait_for_session(client, input_session.id, |session| {
            session.external_session_id.is_some() && session.process_id.is_some()
        }),
        wait_for_session(client, permission_session.id, |session| {
            session.external_session_id.is_some() && session.process_id.is_some()
        })
    );
    let input_running = input_running?;
    permission_running?;
    let external_id = input_running
        .external_session_id
        .clone()
        .ok_or_else(|| "input smoke Session has no verified Claude identity".to_owned())?;

    wait_for_attention_events(&mut events, input_session.id, permission_session.id).await?;
    answer_attention(client, input_session.id).await?;
    answer_attention(client, permission_session.id).await?;
    wait_for_completed_turns(&mut events, &[input_session.id, permission_session.id]).await?;
    send_session_input(client, input_session.id, "/exit").await?;
    send_session_input(client, permission_session.id, "/exit").await?;

    let (input_finished, permission_finished) = tokio::join!(
        wait_for_session(client, input_session.id, session_finished),
        wait_for_session(client, permission_session.id, session_finished)
    );
    let input_finished = input_finished?;
    permission_finished?;
    if input_finished.external_session_id.as_deref() != Some(external_id.as_str()) {
        return Err("completed Claude Session lost its verified identity".into());
    }

    let resumed = match client
        .request(&ClientRequest::ResumeSession {
            session_id: input_session.id,
            columns: 80,
            rows: 24,
        })
        .await
        .map_err(|error| error.to_string())?
    {
        DaemonResponse::SessionResumed { session, .. } => session,
        response => return Err(format!("unexpected Claude resume response: {response:?}")),
    };
    if resumed.id == input_session.id
        || resumed.external_session_id.as_deref() != Some(external_id.as_str())
    {
        return Err("Claude resume did not create a successor for the verified identity".into());
    }
    wait_for_resumed_identity(&mut events, resumed.id, &external_id).await?;
    attach_session(client, resumed.id).await?;
    send_session_input(client, resumed.id, RESUME_PROMPT).await?;
    wait_for_completed_turns(&mut events, &[resumed.id]).await?;
    send_session_input(client, resumed.id, "/exit").await?;
    wait_for_session(client, resumed.id, session_finished).await?;

    let snapshot = match client
        .request(&ClientRequest::GetSnapshot)
        .await
        .map_err(|error| error.to_string())?
    {
        DaemonResponse::Snapshot(snapshot) => snapshot,
        response => return Err(format!("unexpected smoke snapshot response: {response:?}")),
    };
    if !snapshot.sessions.iter().any(|session| {
        session.id == input_session.id
            && session.external_session_id.as_deref() == Some(external_id.as_str())
    }) {
        return Err("resume removed or rewrote the source Claude Session".into());
    }
    assert_not_persisted(&paths.database, INPUT_PROMPT)?;
    assert_not_persisted(&paths.database, PERMISSION_PROMPT)?;
    assert_not_persisted(&paths.database, RESUME_PROMPT)?;
    Ok(())
}

async fn create_session(
    client: &DaemonClient,
    worktree_id: WorktreeId,
    display_name: &str,
    prompt: &str,
) -> Result<Session, String> {
    match client
        .request(&ClientRequest::CreateSession {
            worktree_id,
            provider: ProviderKind::Claude,
            display_name: Some(display_name.into()),
            model: None,
            effort: None,
            initial_prompt: Some(prompt.into()),
            columns: 80,
            rows: 24,
        })
        .await
        .map_err(|error| error.to_string())?
    {
        DaemonResponse::SessionCreated { session, .. } => Ok(session),
        response => Err(format!("unexpected Claude create response: {response:?}")),
    }
}

async fn wait_for_attention_events(
    events: &mut tokio::sync::broadcast::Receiver<DaemonEvent>,
    input_session_id: SessionId,
    permission_session_id: SessionId,
) -> Result<(), String> {
    tokio::time::timeout(STATE_DEADLINE, async {
        let mut saw_input = false;
        let mut saw_permission = false;
        while !saw_input || !saw_permission {
            if let Ok(DaemonEvent::ProviderEvent {
                session_id, event, ..
            }) = events.recv().await
            {
                saw_input |= session_id == input_session_id
                    && matches!(event, NormalizedProviderEvent::UserInputRequested);
                saw_permission |= session_id == permission_session_id
                    && matches!(event, NormalizedProviderEvent::PermissionRequested);
            }
        }
    })
    .await
    .map_err(|_| "Claude did not emit both input and permission attention in time".to_owned())
}

async fn answer_attention(client: &DaemonClient, session_id: SessionId) -> Result<(), String> {
    attach_session(client, session_id).await?;
    send_session_input(client, session_id, "1").await
}

async fn attach_session(client: &DaemonClient, session_id: SessionId) -> Result<(), String> {
    match client
        .request(&ClientRequest::AttachSession {
            session_id,
            from_sequence: 0,
            columns: 80,
            rows: 24,
        })
        .await
        .map_err(|error| error.to_string())?
    {
        DaemonResponse::Attached { .. } => {}
        response => return Err(format!("unexpected Claude attach response: {response:?}")),
    }
    Ok(())
}

async fn wait_for_completed_turns(
    events: &mut tokio::sync::broadcast::Receiver<DaemonEvent>,
    session_ids: &[SessionId],
) -> Result<(), String> {
    tokio::time::timeout(STATE_DEADLINE, async {
        let mut pending = session_ids.to_vec();
        while !pending.is_empty() {
            if let Ok(DaemonEvent::ProviderEvent {
                session_id,
                event: NormalizedProviderEvent::TurnStopped { remaining_work },
                ..
            }) = events.recv().await
                && remaining_work.is_empty()
            {
                pending.retain(|expected| *expected != session_id);
            }
        }
    })
    .await
    .map_err(|_| "Claude smoke-test turn completion timed out".to_owned())
}

async fn wait_for_resumed_identity(
    events: &mut tokio::sync::broadcast::Receiver<DaemonEvent>,
    session_id: SessionId,
    external_id: &str,
) -> Result<(), String> {
    tokio::time::timeout(STATE_DEADLINE, async {
        loop {
            if let Ok(DaemonEvent::ProviderEvent {
                session_id: observed,
                event:
                    NormalizedProviderEvent::TurnStarted {
                        conversation: Some(conversation),
                    },
                ..
            }) = events.recv().await
                && observed == session_id
                && conversation.transition == ConversationIdentityTransition::Resumed
                && conversation.id.as_str() == external_id
            {
                break;
            }
        }
    })
    .await
    .map_err(|_| "Claude did not authenticate the resumed conversation in time".to_owned())
}

async fn send_session_input(
    client: &DaemonClient,
    session_id: SessionId,
    value: &str,
) -> Result<(), String> {
    client
        .request(&ClientRequest::SessionInput {
            session_id,
            bytes: input_line(value),
        })
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

async fn wait_for_session(
    client: &DaemonClient,
    session_id: SessionId,
    predicate: impl Fn(&Session) -> bool,
) -> Result<Session, String> {
    tokio::time::timeout(STATE_DEADLINE, async {
        loop {
            let snapshot = match client
                .request(&ClientRequest::GetSnapshot)
                .await
                .map_err(|error| error.to_string())?
            {
                DaemonResponse::Snapshot(snapshot) => snapshot,
                response => return Err(format!("unexpected snapshot response: {response:?}")),
            };
            if let Some(session) = snapshot
                .sessions
                .into_iter()
                .find(|session| session.id == session_id && predicate(session))
            {
                return Ok(session);
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .map_err(|_| "Claude Session state transition timed out".to_owned())?
}

fn session_finished(session: &Session) -> bool {
    session.process_id.is_none()
        && matches!(
            session.state,
            SessionState::FinishedSeen | SessionState::FinishedUnseen
        )
}

async fn connect_eventually(paths: &RuntimePaths) -> DaemonClient {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(client) = DaemonClient::connect(paths, "real-claude-smoke").await {
                return client;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("daemon connection timeout")
}

#[cfg(windows)]
fn input_line(value: &str) -> Vec<u8> {
    format!("{value}\r\n").into_bytes()
}

#[cfg(not(windows))]
fn input_line(value: &str) -> Vec<u8> {
    format!("{value}\n").into_bytes()
}

fn assert_not_persisted(path: &Path, sentinel: &str) -> Result<(), String> {
    let connection = rusqlite::Connection::open(path).map_err(|error| error.to_string())?;
    let tables = connection
        .prepare("SELECT name FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE 'sqlite_%'")
        .map_err(|error| error.to_string())?
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| error.to_string())?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| error.to_string())?;
    for table in tables {
        let quoted_table = quote_identifier(&table);
        let columns = connection
            .prepare(&format!("PRAGMA table_info({quoted_table})"))
            .map_err(|error| error.to_string())?
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(|error| error.to_string())?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|error| error.to_string())?;
        for column in columns {
            let quoted_column = quote_identifier(&column);
            let query = format!(
                "SELECT {quoted_column} FROM {quoted_table} WHERE typeof({quoted_column}) = 'text' AND instr({quoted_column}, ?1) > 0"
            );
            let found = connection
                .query_row(&query, [sentinel], |row| row.get::<_, String>(0))
                .optional()
                .map_err(|error| error.to_string())?;
            if found.is_some() {
                return Err("a real Claude smoke prompt was persisted".into());
            }
        }
    }
    Ok(())
}

fn quote_identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}
