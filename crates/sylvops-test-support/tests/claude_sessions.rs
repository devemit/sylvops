#![allow(unsafe_code)]

use std::{ffi::OsString, path::Path, process::Command, time::Duration};

use rusqlite::OptionalExtension;
use sylvops_core::{
    domain::{ProviderKind, Session, SessionState},
    ids::SessionId,
    protocol::{ClientRequest, DaemonEvent, DaemonResponse},
    status::NormalizedProviderEvent,
    ui_forms::Form,
};
use sylvops_daemon::{client::DaemonClient, daemon, runtime::RuntimePaths};

const PROMPT_SENTINEL: &str = "SYLVOPS_PRIVATE_CLAUDE_PROMPT_74_22f1";
const FAILURE_SENTINEL: &str = "SYLVOPS_PRIVATE_CLAUDE_FAILURE_75";
const MALFORMED_SENTINEL: &str = "SYLVOPS_PRIVATE_MALFORMED_HOOK_75";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn managed_claude_sessions_cover_attention_terminal_and_hook_recovery() {
    tokio::time::timeout(Duration::from_secs(120), async {
        interactive_client_request_creates_claude_session().await;
        managed_claude_session_inner().await;
        late_session_start_recovers_after_warning().await;
    })
    .await
    .expect("managed Claude scenarios timeout");
}

async fn interactive_client_request_creates_claude_session() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let repository = temporary.path().join("repository");
    initialize_repository(&repository);
    let bin_directory = temporary.path().join("bin");
    std::fs::create_dir(&bin_directory).expect("fake provider bin directory");
    let fake_claude = install_fake_claude(&bin_directory);
    let _claude_cli_path = EnvironmentGuard::set("CLAUDE_CLI_PATH", fake_claude);

    let paths = RuntimePaths::discover(Some(&temporary.path().join("state"))).expect("paths");
    paths.prepare().expect("prepare runtime paths");
    std::fs::write(
        &paths.config,
        "enabled_providers = ['shell', 'claude']\nupdate_check_policy = 'disabled'\n",
    )
    .expect("Claude-enabled configuration");
    let daemon_paths = paths.clone();
    let mut daemon_task = tokio::spawn(async move { daemon::run(daemon_paths).await });
    let client = tokio::select! {
        result = &mut daemon_task => panic!("daemon exited before connection: {result:?}"),
        client = connect_eventually(&paths) => client,
    };

    let workspace_id = match client
        .request(&ClientRequest::AddWorkspace {
            name: "interactive-client".into(),
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
    let providers = match client
        .request(&ClientRequest::ListProviders)
        .await
        .expect("provider response")
    {
        DaemonResponse::Providers(providers) => providers,
        response => panic!("unexpected provider response: {response:?}"),
    };
    let mut form = Form::session(worktree_id, providers);
    assert!(form.select_provider(ProviderKind::Claude));
    form.fields[0].value = "Interactive Claude".into();
    assert!(form.validate());

    let session = match client
        .request(&form.session_request(80, 24).expect("session request"))
        .await
        .expect("session response")
    {
        DaemonResponse::SessionCreated { session, .. } => session,
        response => panic!("unexpected session response: {response:?}"),
    };
    assert_eq!(session.provider_kind, ProviderKind::Claude);
    assert_eq!(session.worktree_id, worktree_id);
    assert_eq!(session.display_name, "Interactive Claude");
    assert!(!session.arguments_json.contains("--model"));
    assert!(!session.arguments_json.contains("--effort"));
    wait_for_session(&client, session.id, |session| {
        session.state == SessionState::Running && session.external_session_id.is_some()
    })
    .await;

    assert_claude_descendant_stops_with_session(&client, session.id, temporary.path()).await;
    client
        .request(&ClientRequest::ShutdownDaemon)
        .await
        .expect("shutdown response");
    tokio::time::timeout(Duration::from_secs(10), daemon_task)
        .await
        .expect("daemon shutdown timeout")
        .expect("daemon task")
        .expect("daemon result");
}

#[allow(clippy::too_many_lines)]
async fn managed_claude_session_inner() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let repository = temporary.path().join("repository");
    initialize_repository(&repository);
    let bin_directory = temporary.path().join("bin");
    std::fs::create_dir(&bin_directory).expect("fake provider bin directory");
    let fake_claude = install_fake_claude(&bin_directory);
    let _claude_cli_path = EnvironmentGuard::set("CLAUDE_CLI_PATH", fake_claude);

    let paths = RuntimePaths::discover(Some(&temporary.path().join("state"))).expect("paths");
    paths.prepare().expect("prepare runtime paths");
    std::fs::write(
        &paths.config,
        "enabled_providers = ['shell', 'claude']\nupdate_check_policy = 'disabled'\n",
    )
    .expect("Claude-enabled configuration");
    let daemon_paths = paths.clone();
    let mut daemon_task = tokio::spawn(async move { daemon::run(daemon_paths).await });
    let client = tokio::select! {
        result = &mut daemon_task => panic!("daemon exited before connection: {result:?}"),
        client = connect_eventually(&paths) => client,
    };

    let workspace_id = match client
        .request(&ClientRequest::AddWorkspace {
            name: "claude-session".into(),
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

    let session_id = match client
        .request(&ClientRequest::CreateSession {
            worktree_id,
            provider: ProviderKind::Claude,
            display_name: Some("fake Claude".into()),
            model: Some("sonnet".into()),
            effort: Some("high".into()),
            initial_prompt: Some(PROMPT_SENTINEL.into()),
            columns: 80,
            rows: 24,
        })
        .await
        .expect("session response")
    {
        DaemonResponse::SessionCreated { session, .. } => {
            assert!(!session.arguments_json.contains(PROMPT_SENTINEL));
            session.id
        }
        response => panic!("unexpected session response: {response:?}"),
    };
    let running = wait_for_session(&client, session_id, |session| {
        session.state == SessionState::Running && session.external_session_id.is_some()
    })
    .await;
    let mut verified_id = running
        .external_session_id
        .clone()
        .filter(|id| id.starts_with("fake-claude-"))
        .expect("verified Claude conversation ID");
    let established_audit = wait_for_audit(
        &paths.database,
        "provider_conversation_changed",
        Some("established"),
        1,
    )
    .await;
    assert!(!established_audit.contains("fake-claude-"));

    let settings = managed_claude_settings(&paths.runtime_directory);
    assert_eq!(settings.len(), 1);
    let source = std::fs::read_to_string(&settings[0]).expect("managed settings source");
    assert!(source.contains("SessionStart"));
    assert!(source.contains("PermissionRequest"));
    assert!(source.contains("AskUserQuestion|ExitPlanMode"));
    assert!(source.contains("SubagentStart"));
    assert!(source.contains("SubagentStop"));
    assert!(source.contains("StopFailure"));
    assert!(source.contains("$SYLVOPS_HOOK_TOKEN"));
    assert!(!source.contains(PROMPT_SENTINEL));
    assert!(!source.contains("permissionMode"));
    let duplicate = tokio::time::timeout(Duration::from_secs(5), daemon::run(paths.clone()))
        .await
        .expect("duplicate daemon bind is bounded");
    assert!(duplicate.is_err());
    assert!(settings[0].exists());

    let mut events = client.subscribe();
    attach(&client, session_id, 0).await;
    client
        .request(&ClientRequest::ResizeSession {
            session_id,
            columns: 100,
            rows: 35,
        })
        .await
        .expect("resize response");
    client
        .request(&ClientRequest::SessionInput {
            session_id,
            bytes: input_line("clear-conversation"),
        })
        .await
        .expect("clear transition input");
    verified_id.push_str("-cleared");
    wait_for_session(&client, session_id, |session| {
        session.external_session_id.as_deref() == Some(verified_id.as_str())
    })
    .await;
    let cleared_audit = wait_for_audit(
        &paths.database,
        "provider_conversation_changed",
        Some("cleared"),
        1,
    )
    .await;
    assert!(!cleared_audit.contains("fake-claude-"));

    client
        .request(&ClientRequest::SessionInput {
            session_id,
            bytes: input_line("resume-conversation"),
        })
        .await
        .expect("resume transition input");
    verified_id.push_str("-resumed");
    wait_for_session(&client, session_id, |session| {
        session.external_session_id.as_deref() == Some(verified_id.as_str())
    })
    .await;
    let resumed_audit = wait_for_audit(
        &paths.database,
        "provider_conversation_changed",
        Some("resumed"),
        1,
    )
    .await;
    assert!(!resumed_audit.contains("fake-claude-"));

    client
        .request(&ClientRequest::SessionInput {
            session_id,
            bytes: input_line("unexpected-id-change"),
        })
        .await
        .expect("unexpected identity input");
    wait_for_audit(
        &paths.database,
        "provider_hook_refused",
        Some("external_session_id_changed"),
        1,
    )
    .await;
    wait_for_session(&client, session_id, |session| {
        session.state == SessionState::Running
            && session.external_session_id.as_deref() == Some(verified_id.as_str())
    })
    .await;

    client
        .request(&ClientRequest::SessionInput {
            session_id,
            bytes: input_line("replay-me"),
        })
        .await
        .expect("terminal input");
    let output_sequence =
        wait_for_output(&mut events, session_id, "FAKE_CLAUDE_ECHO=replay-me").await;
    client
        .request(&ClientRequest::DetachSession { session_id })
        .await
        .expect("detach response");

    let reconnected = connect_eventually(&paths).await;
    let mut replay_events = reconnected.subscribe();
    let mut lifecycle_events = reconnected.subscribe();
    attach(&reconnected, session_id, output_sequence.saturating_sub(1)).await;
    wait_for_output(&mut replay_events, session_id, "FAKE_CLAUDE_ECHO=replay-me").await;

    for (index, request) in ["permission", "ask-user-question", "exit-plan-mode"]
        .into_iter()
        .enumerate()
    {
        reconnected
            .request(&ClientRequest::SessionInput {
                session_id,
                bytes: input_line(request),
            })
            .await
            .expect("attention hook input");
        wait_for_session(&reconnected, session_id, |session| {
            session.state == SessionState::NeedsFeedback
        })
        .await;
        reconnected
            .request(&ClientRequest::SessionInput {
                session_id,
                bytes: input_line(&format!("user-prompt-{index}")),
            })
            .await
            .expect("UserPromptSubmit input");
        wait_for_session(&reconnected, session_id, |session| {
            session.state == SessionState::Running
        })
        .await;
    }

    reconnected
        .request(&ClientRequest::SessionInput {
            session_id,
            bytes: input_line("subagent-start"),
        })
        .await
        .expect("SubagentStart input");
    wait_for_provider_event(&mut lifecycle_events, session_id, |event| {
        matches!(event, NormalizedProviderEvent::SubagentStarted { .. })
    })
    .await;
    reconnected
        .request(&ClientRequest::SessionInput {
            session_id,
            bytes: input_line("stop-normal-while-subagent-active"),
        })
        .await
        .expect("Stop input with active subagent");
    wait_for_provider_event(&mut lifecycle_events, session_id, |event| {
        matches!(event, NormalizedProviderEvent::TurnStopped { .. })
    })
    .await;
    wait_for_session(&reconnected, session_id, |session| {
        session.state == SessionState::Running
    })
    .await;
    reconnected
        .request(&ClientRequest::SessionInput {
            session_id,
            bytes: input_line("subagent-stop"),
        })
        .await
        .expect("SubagentStop input");
    wait_for_provider_event(&mut lifecycle_events, session_id, |event| {
        matches!(event, NormalizedProviderEvent::SubagentStopped { .. })
    })
    .await;

    for (request, has_background, has_schedule) in [
        ("stop-background", true, false),
        ("stop-scheduled", false, true),
    ] {
        reconnected
            .request(&ClientRequest::SessionInput {
                session_id,
                bytes: input_line(request),
            })
            .await
            .expect("Stop input with remaining work");
        wait_for_provider_event(&mut lifecycle_events, session_id, |event| {
            matches!(
                event,
                NormalizedProviderEvent::TurnStopped { remaining_work }
                    if remaining_work.background_tasks == has_background
                        && remaining_work.scheduled_tasks == has_schedule
            )
        })
        .await;
        wait_for_session(&reconnected, session_id, |session| {
            session.state == SessionState::Running
        })
        .await;
    }

    reconnected
        .request(&ClientRequest::SessionInput {
            session_id,
            bytes: input_line("stop-normal"),
        })
        .await
        .expect("normal Stop input");
    wait_for_session(&reconnected, session_id, |session| {
        session.state == SessionState::FinishedUnseen
    })
    .await;
    reconnected
        .request(&ClientRequest::SessionInput {
            session_id,
            bytes: input_line("stale-permission"),
        })
        .await
        .expect("stale PermissionRequest input");
    wait_for_audit(&paths.database, "provider_hook_ignored", Some("stale"), 1).await;
    wait_for_session(&reconnected, session_id, |session| {
        session.state == SessionState::FinishedUnseen
    })
    .await;
    attach(&reconnected, session_id, 0).await;
    wait_for_session(&reconnected, session_id, |session| {
        session.state == SessionState::FinishedSeen
    })
    .await;

    reconnected
        .request(&ClientRequest::SessionInput {
            session_id,
            bytes: input_line("user-prompt-after-stop"),
        })
        .await
        .expect("prompt after completed turn");
    wait_for_session(&reconnected, session_id, |session| {
        session.state == SessionState::Running
    })
    .await;
    reconnected
        .request(&ClientRequest::SessionInput {
            session_id,
            bytes: input_line("stop-failure"),
        })
        .await
        .expect("StopFailure input");
    wait_for_provider_event(&mut lifecycle_events, session_id, |event| {
        matches!(
            event,
            NormalizedProviderEvent::TurnFailed { category }
                if category.as_str() == "rate_limit"
        )
    })
    .await;
    let recoverable = wait_for_session(&reconnected, session_id, |session| {
        session.state == SessionState::NeedsFeedback
    })
    .await;
    assert!(recoverable.failure_reason.is_none());

    reconnected
        .request(&ClientRequest::SessionInput {
            session_id,
            bytes: input_line("output-flood"),
        })
        .await
        .expect("output flood input");
    wait_for_session(&reconnected, session_id, |session| {
        session.state == SessionState::Running
    })
    .await;

    reconnected
        .request(&ClientRequest::SessionInput {
            session_id,
            bytes: input_line("malformed-hook"),
        })
        .await
        .expect("malformed hook input");
    let malformed_audit = wait_for_audit(
        &paths.database,
        "provider_hook_refused",
        Some("invalid_payload"),
        1,
    )
    .await;
    assert!(!malformed_audit.contains(MALFORMED_SENTINEL));

    reconnected
        .request(&ClientRequest::SessionInput {
            session_id,
            bytes: input_line("unknown-hook"),
        })
        .await
        .expect("unknown hook input");
    let unknown_audit = wait_for_audit(
        &paths.database,
        "provider_hook_refused",
        Some("invalid_payload"),
        2,
    )
    .await;
    assert!(!unknown_audit.contains("FutureClaudeEvent"));

    reconnected
        .request(&ClientRequest::SessionInput {
            session_id,
            bytes: input_line("duplicate-hook"),
        })
        .await
        .expect("duplicate hook input");
    wait_for_audit(
        &paths.database,
        "provider_hook_ignored",
        Some("duplicate"),
        1,
    )
    .await;
    wait_for_session(&reconnected, session_id, |session| {
        session.state == SessionState::Running
    })
    .await;

    reconnected
        .request(&ClientRequest::SessionInput {
            session_id,
            bytes: input_line("session-end"),
        })
        .await
        .expect("SessionEnd input");
    wait_for_session(&reconnected, session_id, |session| {
        session.state == SessionState::FinishedUnseen && session.process_id.is_some()
    })
    .await;
    reconnected
        .request(&ClientRequest::SessionInput {
            session_id,
            bytes: input_line("post-end-hook"),
        })
        .await
        .expect("post-terminal input");
    wait_for_output(
        &mut replay_events,
        session_id,
        "FAKE_CLAUDE_POST_END=refused",
    )
    .await;
    reconnected
        .request(&ClientRequest::StopSession { session_id })
        .await
        .expect("stop response");
    wait_for_session(&reconnected, session_id, |session| {
        session.state == SessionState::Terminated && session.process_id.is_none()
    })
    .await;

    reconnected
        .request(&ClientRequest::ShutdownDaemon)
        .await
        .expect("shutdown response");
    tokio::time::timeout(Duration::from_secs(10), daemon_task)
        .await
        .expect("daemon shutdown timeout")
        .expect("daemon task")
        .expect("daemon result");
    assert_eq!(
        managed_claude_settings(&paths.runtime_directory),
        Vec::<std::path::PathBuf>::new()
    );
    assert_no_persisted_text_contains(&paths.database, PROMPT_SENTINEL);
    assert_no_persisted_text_contains(&paths.database, FAILURE_SENTINEL);
    assert_no_persisted_text_contains(&paths.database, MALFORMED_SENTINEL);
}

#[allow(clippy::too_many_lines)]
async fn late_session_start_recovers_after_warning() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let repository = temporary.path().join("repository");
    initialize_repository(&repository);
    let bin_directory = temporary.path().join("bin");
    std::fs::create_dir(&bin_directory).expect("fake provider bin directory");
    let fake_claude = install_fake_claude(&bin_directory);
    let session_start_fixture = bin_directory.join("fake-claude-session-start");
    std::fs::write(&session_start_fixture, "none").expect("missing SessionStart fixture");
    let _claude_cli_path = EnvironmentGuard::set("CLAUDE_CLI_PATH", fake_claude);

    let paths = RuntimePaths::discover(Some(&temporary.path().join("state"))).expect("paths");
    paths.prepare().expect("prepare runtime paths");
    std::fs::write(
        &paths.config,
        "enabled_providers = ['shell', 'claude']\nupdate_check_policy = 'disabled'\n",
    )
    .expect("Claude-enabled configuration");
    let daemon_paths = paths.clone();
    let mut daemon_task = tokio::spawn(async move { daemon::run(daemon_paths).await });
    let client = tokio::select! {
        result = &mut daemon_task => panic!("daemon exited before connection: {result:?}"),
        client = connect_eventually(&paths) => client,
    };

    let workspace_id = match client
        .request(&ClientRequest::AddWorkspace {
            name: "claude-late-hook".into(),
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
    let session_id = match client
        .request(&ClientRequest::CreateSession {
            worktree_id,
            provider: ProviderKind::Claude,
            display_name: Some("late Claude".into()),
            model: None,
            effort: None,
            initial_prompt: None,
            columns: 80,
            rows: 24,
        })
        .await
        .expect("session response")
    {
        DaemonResponse::SessionCreated { session, .. } => session.id,
        response => panic!("unexpected session response: {response:?}"),
    };
    let mut events = client.subscribe();
    attach(&client, session_id, 0).await;

    let warning = wait_for_session_for(&client, session_id, Duration::from_secs(20), |session| {
        session.state == SessionState::NeedsFeedback
            && session.process_id.is_some()
            && session.external_session_id.is_none()
    })
    .await;
    assert!(
        warning
            .failure_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("lifecycle hooks"))
    );

    client
        .request(&ClientRequest::SessionInput {
            session_id,
            bytes: input_line("still-usable"),
        })
        .await
        .expect("input while hook warning is active");
    wait_for_output(&mut events, session_id, "FAKE_CLAUDE_ECHO=still-usable").await;
    let recovered = wait_for_session_for(&client, session_id, Duration::from_secs(10), |session| {
        session.state == SessionState::Running && session.external_session_id.is_none()
    })
    .await;
    assert!(recovered.failure_reason.is_none());

    client
        .request(&ClientRequest::SessionInput {
            session_id,
            bytes: input_line("session-start"),
        })
        .await
        .expect("late SessionStart input");
    wait_for_session(&client, session_id, |session| {
        session.state == SessionState::Running && session.external_session_id.is_some()
    })
    .await;

    client
        .request(&ClientRequest::StopSession { session_id })
        .await
        .expect("stop response");
    wait_for_session(&client, session_id, |session| {
        session.state == SessionState::Terminated
    })
    .await;

    std::fs::write(&session_start_fixture, "immediate").expect("immediate SessionStart fixture");
    let failed_session_id = match client
        .request(&ClientRequest::CreateSession {
            worktree_id,
            provider: ProviderKind::Claude,
            display_name: Some("crashing Claude".into()),
            model: None,
            effort: None,
            initial_prompt: None,
            columns: 80,
            rows: 24,
        })
        .await
        .expect("crashing session response")
    {
        DaemonResponse::SessionCreated { session, .. } => session.id,
        response => panic!("unexpected crashing session response: {response:?}"),
    };
    wait_for_session(&client, failed_session_id, |session| {
        session.state == SessionState::Running && session.external_session_id.is_some()
    })
    .await;
    attach(&client, failed_session_id, 0).await;
    client
        .request(&ClientRequest::SessionInput {
            session_id: failed_session_id,
            bytes: input_line("unexpected-exit"),
        })
        .await
        .expect("unexpected exit input");
    let failed = wait_for_session(&client, failed_session_id, |session| {
        session.state == SessionState::Failed && session.process_id.is_none()
    })
    .await;
    assert_eq!(failed.exit_code, Some(17));
    assert_eq!(
        failed.failure_reason.as_deref(),
        Some("process exited with code 17")
    );
    let external_id = failed
        .external_session_id
        .clone()
        .expect("verified Claude conversation ID");

    let first_resume = ClientRequest::ResumeSession {
        session_id: failed_session_id,
        columns: 80,
        rows: 24,
    };
    let concurrent_resume = first_resume.clone();
    let (first_response, concurrent_response) = tokio::join!(
        client.request(&first_resume),
        client.request(&concurrent_resume)
    );
    let mut resumed = None;
    let mut refused = 0;
    for response in [first_response, concurrent_response] {
        match response.expect("concurrent Claude resume response") {
            DaemonResponse::SessionResumed { session, .. } => {
                assert!(resumed.replace(session).is_none());
            }
            DaemonResponse::Error(error) => {
                assert!(
                    error.message.contains("already been resumed"),
                    "concurrent Claude resume error should be actionable: {}",
                    error.message
                );
                refused += 1;
            }
            response => panic!("unexpected concurrent Claude resume response: {response:?}"),
        }
    }
    assert_eq!(refused, 1);
    let resumed = resumed.expect("one concurrent Claude resume succeeds");
    assert_ne!(resumed.id, failed_session_id);
    assert_eq!(resumed.provider_kind, ProviderKind::Claude);
    assert_eq!(resumed.worktree_id, worktree_id);
    assert_eq!(resumed.cwd, failed.cwd);
    assert_eq!(
        resumed.external_session_id.as_deref(),
        Some(external_id.as_str())
    );
    let arguments: Vec<String> =
        serde_json::from_str(&resumed.arguments_json).expect("persisted Claude resume arguments");
    let resume_index = arguments
        .iter()
        .position(|argument| argument == "--resume")
        .expect("Claude --resume argument");
    assert_eq!(
        arguments.get(resume_index + 1).map(String::as_str),
        Some(external_id.as_str())
    );
    assert!(!arguments.iter().any(|argument| {
        matches!(
            argument.as_str(),
            "--continue" | "--fork-session" | "--session-id"
        )
    }));
    wait_for_session(&client, resumed.id, |session| {
        session.state == SessionState::Running
            && session.external_session_id.as_deref() == Some(external_id.as_str())
    })
    .await;
    match client
        .request(&first_resume)
        .await
        .expect("stale Claude resume response")
    {
        DaemonResponse::Error(error) => assert!(error.message.contains("already been resumed")),
        response => panic!("stale Claude resume unexpectedly succeeded: {response:?}"),
    }

    client
        .request(&ClientRequest::ShutdownDaemon)
        .await
        .expect("shutdown response");
    tokio::time::timeout(Duration::from_secs(10), daemon_task)
        .await
        .expect("daemon shutdown timeout")
        .expect("daemon task")
        .expect("daemon result");
    assert_eq!(
        managed_claude_settings(&paths.runtime_directory),
        Vec::<std::path::PathBuf>::new()
    );

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
        session.id == failed_session_id
            && session.external_session_id.as_deref() == Some(external_id.as_str())
    }));
    match restarted
        .request(&first_resume)
        .await
        .expect("restart stale Claude resume response")
    {
        DaemonResponse::Error(error) => assert!(error.message.contains("already been resumed")),
        response => panic!("restart stale Claude resume unexpectedly succeeded: {response:?}"),
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
}

fn managed_claude_settings(directory: &Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(directory)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.starts_with("sylvops-claude-") && name.ends_with(".settings.json")
                })
        })
        .collect()
}

async fn attach(client: &DaemonClient, session_id: SessionId, from_sequence: u64) {
    assert!(matches!(
        client
            .request(&ClientRequest::AttachSession {
                session_id,
                from_sequence,
                columns: 80,
                rows: 24,
            })
            .await
            .expect("attach response"),
        DaemonResponse::Attached { .. }
    ));
}

async fn assert_claude_descendant_stops_with_session(
    client: &DaemonClient,
    session_id: SessionId,
    directory: &Path,
) {
    let heartbeat = directory.join("claude-descendant-heartbeat.txt");
    attach(client, session_id, 0).await;
    client
        .request(&ClientRequest::SessionInput {
            session_id,
            bytes: input_line(&format!("spawn-descendant:{}", heartbeat.to_string_lossy())),
        })
        .await
        .expect("spawn descendant input");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if heartbeat.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("Claude descendant heartbeat timeout");

    client
        .request(&ClientRequest::StopSession { session_id })
        .await
        .expect("stop response");
    wait_for_session(client, session_id, |session| {
        session.state == SessionState::Terminated && session.process_id.is_none()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let stopped_heartbeat = std::fs::read_to_string(&heartbeat).expect("stopped heartbeat");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        std::fs::read_to_string(&heartbeat).expect("later heartbeat"),
        stopped_heartbeat,
        "Claude descendant remained alive after Session stop"
    );
}

async fn wait_for_output(
    events: &mut tokio::sync::broadcast::Receiver<DaemonEvent>,
    session_id: SessionId,
    expected: &str,
) -> u64 {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(DaemonEvent::SessionOutput {
                session_id: observed,
                sequence,
                bytes,
                ..
            }) = events.recv().await
                && observed == session_id
                && String::from_utf8_lossy(&bytes).contains(expected)
            {
                return sequence;
            }
        }
    })
    .await
    .expect("terminal output timeout")
}

async fn wait_for_provider_event(
    events: &mut tokio::sync::broadcast::Receiver<DaemonEvent>,
    session_id: SessionId,
    predicate: impl Fn(&NormalizedProviderEvent) -> bool,
) -> NormalizedProviderEvent {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(DaemonEvent::ProviderEvent {
                session_id: observed,
                event,
                ..
            }) = events.recv().await
                && observed == session_id
                && predicate(&event)
            {
                return event;
            }
        }
    })
    .await
    .expect("provider event timeout")
}

async fn wait_for_audit(
    database: &Path,
    action: &str,
    reason: Option<&str>,
    expected_count: i64,
) -> String {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let connection = rusqlite::Connection::open(database).expect("open audit database");
            let mut statement = connection
                .prepare(
                    "SELECT details_json FROM audit_events WHERE action = ?1 ORDER BY occurred_at",
                )
                .expect("prepare audit query");
            let details = statement
                .query_map([action], |row| row.get::<_, String>(0))
                .expect("query audit events")
                .collect::<rusqlite::Result<Vec<_>>>()
                .expect("collect audit events");
            let matching: Vec<_> = details
                .iter()
                .filter(|details| reason.is_none_or(|reason| details.contains(reason)))
                .collect();
            if i64::try_from(matching.len()).unwrap_or(i64::MAX) >= expected_count {
                return (*matching.last().expect("matching audit event")).clone();
            }
            drop(statement);
            drop(connection);
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("audit event timeout")
}

async fn wait_for_session(
    client: &DaemonClient,
    session_id: SessionId,
    predicate: impl Fn(&Session) -> bool,
) -> Session {
    wait_for_session_for(client, session_id, Duration::from_secs(10), predicate).await
}

async fn wait_for_session_for(
    client: &DaemonClient,
    session_id: SessionId,
    duration: Duration,
    predicate: impl Fn(&Session) -> bool,
) -> Session {
    let deadline = tokio::time::Instant::now() + duration;
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
            if let Ok(client) = DaemonClient::connect(paths, "fake-claude-session-test").await {
                return client;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("daemon connection timeout")
}

fn install_fake_claude(directory: &Path) -> std::path::PathBuf {
    let target = directory.join(if cfg!(windows) {
        "claude.exe"
    } else {
        "claude"
    });
    std::fs::copy(env!("CARGO_BIN_EXE_fake-claude"), &target).expect("copy fake Claude");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mut permissions = std::fs::metadata(&target).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&target, permissions).unwrap();
    }
    target
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

fn assert_no_persisted_text_contains(path: &Path, sentinel: &str) {
    let connection = rusqlite::Connection::open(path).expect("open persisted database");
    let tables = connection
        .prepare("SELECT name FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE 'sqlite_%'")
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
                "SELECT {quoted_column} FROM {quoted_table} WHERE typeof({quoted_column}) = 'text' AND instr({quoted_column}, ?1) > 0"
            );
            let found = connection
                .query_row(&query, [sentinel], |row| row.get::<_, String>(0))
                .optional()
                .expect("scan persisted text");
            assert!(found.is_none(), "prompt persisted in {table}.{column}");
        }
    }
}

fn quote_identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

struct EnvironmentGuard {
    name: &'static str,
    previous: Option<OsString>,
}

impl EnvironmentGuard {
    fn set(name: &'static str, value: impl Into<OsString>) -> Self {
        let previous = std::env::var_os(name);
        unsafe { std::env::set_var(name, value.into()) };
        Self { name, previous }
    }
}

impl Drop for EnvironmentGuard {
    fn drop(&mut self) {
        unsafe {
            if let Some(previous) = self.previous.take() {
                std::env::set_var(self.name, previous);
            } else {
                std::env::remove_var(self.name);
            }
        }
    }
}
