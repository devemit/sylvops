use std::{fmt::Write as _, path::Path, process::Stdio, time::Duration};

use sylvops_core::{
    domain::{ProviderKind, Session},
    ids::SessionId,
    protocol::{ClientRequest, DaemonResponse},
};
use sylvops_daemon::{client::DaemonClient, runtime::RuntimePaths};
use sylvops_test_support::{TemporaryRepository, bounded};

/// Uses an explicitly chosen, already trusted production relay and repository.
/// Does not change trust, user configuration, or the installed application.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "opt-in real account: set SYLVOPS_REAL_RELAY_PATH and SYLVOPS_REAL_CODEX_REPOSITORY"]
#[allow(clippy::too_many_lines)] // Fresh launch and exact-conversation resume share one bounded fixture.
async fn real_codex_private_runtime_uses_the_production_relay() {
    use sylvops_core::{
        provider::{LaunchContext, ResumeContext},
        status::NormalizedProviderEvent,
    };
    use sylvops_daemon::{
        hook::HookReceiver,
        provider::ProviderRegistry,
        session::{SessionHandle, SessionSpec},
    };
    let relay = std::path::PathBuf::from(
        std::env::var_os("SYLVOPS_REAL_RELAY_PATH").expect("explicit production relay"),
    );
    let cwd = std::path::PathBuf::from(
        std::env::var_os("SYLVOPS_REAL_CODEX_REPOSITORY").expect("explicit trusted repository"),
    );
    assert!(relay.is_absolute() && relay.is_file() && cwd.is_absolute() && cwd.is_dir());
    let mut receiver = HookReceiver::bind(relay.clone(), 64 * 1024, 600)
        .await
        .unwrap();
    let mut deliveries = receiver.take_deliveries();
    let registry = ProviderRegistry::new(Some(&relay), &[ProviderKind::Codex]).unwrap();
    let health = registry.probe(ProviderKind::Codex).await.unwrap();
    assert!(
        health.available && health.authenticated,
        "real Codex must be installed and logged in"
    );
    let worktree_id = sylvops_core::ids::WorktreeId::new();
    let mut conversation = None;
    for resumed in [false, true] {
        let session_id = SessionId::new();
        let endpoint = receiver
            .register(ProviderKind::Codex, session_id, worktree_id)
            .unwrap();
        let mut runtime = if resumed {
            registry
                .resume(
                    ProviderKind::Codex,
                    ResumeContext {
                        session_id,
                        worktree_id,
                        cwd: cwd.clone(),
                        model: None,
                        effort: None,
                        external_session_id: conversation
                            .clone()
                            .expect("identity from fresh Session"),
                        lifecycle_endpoint: Some(endpoint),
                    },
                )
                .unwrap()
        } else {
            registry
                .launch(
                    ProviderKind::Codex,
                    LaunchContext {
                        session_id,
                        worktree_id,
                        cwd: cwd.clone(),
                        model: None,
                        effort: None,
                        initial_prompt: Some(
                            "Reply with exactly OK. Do not use tools or change files.".into(),
                        ),
                        lifecycle_endpoint: Some(endpoint),
                    },
                )
                .unwrap()
        };
        // The noninteractive test runner exports TERM=dumb; the owned PTY is VT-capable.
        runtime
            .launch
            .environment
            .insert("TERM".into(), "xterm-256color".into());
        // Avoid interactive upgrade prompts; this does not modify saved settings.
        runtime.launch.arguments.prepend_persisted([
            "--config".into(),
            "check_for_update_on_startup=false".into(),
        ]);
        assert!(
            runtime
                .launch
                .arguments
                .all()
                .iter()
                .any(|argument| argument == "--no-daemon"),
            "installed Codex must advertise private runtime support"
        );
        let mut handle = SessionHandle::spawn_sanitized(
            &SessionSpec {
                program: runtime.launch.executable,
                arguments: runtime.launch.arguments.all().to_vec(),
                cwd: cwd.clone(),
                columns: 100,
                rows: 30,
                scrollback_bytes: 64 * 1024,
            },
            &runtime.launch.environment,
        )
        .unwrap();
        let mut callback_count = 0_usize;
        let delivered = tokio::time::timeout(Duration::from_secs(45), async {
            loop {
                let delivery = deliveries.recv().await.expect("hook receiver open");
                if delivery.session_id != session_id {
                    continue;
                }
                callback_count += 1;
                let normalized = registry
                    .normalize_lifecycle_event(ProviderKind::Codex, &delivery.payload)
                    .unwrap();
                if let Some(identity) = normalized.conversation_id {
                    if resumed {
                        assert!(
                            conversation.as_ref() == Some(&identity),
                            "resumed runtime must keep the verified Conversation identity"
                        );
                    }
                    conversation = Some(identity);
                }
                if matches!(
                    normalized.event,
                    Some(NormalizedProviderEvent::TurnStopped { .. })
                ) || (resumed
                    && conversation.is_some()
                    && matches!(
                        normalized.event,
                        Some(NormalizedProviderEvent::TurnStarted { .. })
                    ))
                {
                    break;
                }
            }
        })
        .await;
        let stopped = bounded(handle.stop())
            .await
            .expect("real provider stop timeout");
        let exit = bounded(handle.wait())
            .await
            .expect("real provider exit timeout");
        receiver.invalidate(session_id);
        assert!(
            stopped.is_ok() && exit.is_ok_and(|exit| exit.stop_requested),
            "owned provider process must stop"
        );
        assert!(
            delivered.is_ok(),
            "real callback missing ({callback_count} callbacks); verify repository and hook trust manually; no trust was bypassed"
        );
        assert!(
            conversation.is_some(),
            "real callback must capture Conversation identity"
        );
    }
    bounded(receiver.shutdown())
        .await
        .expect("hook receiver shutdown timeout");
}

/// Requires an existing real Codex login and prior /hooks approval of the stable
/// hook-daemon source. Never approves hooks, bypasses trust, or prints PTY contents.
#[tokio::test]
#[ignore = "opt-in real account: set SYLVOPS_REAL_CODEX_PATH to an absolute native executable"]
async fn real_codex_delivers_completion_across_sessions_and_daemon_restart() {
    let executable = std::env::var_os("SYLVOPS_REAL_CODEX_PATH")
        .map(std::path::PathBuf::from)
        .expect("set SYLVOPS_REAL_CODEX_PATH explicitly");
    assert!(executable.is_absolute() && executable.is_file());
    tokio::time::timeout(Duration::from_secs(180), async {
        let repository = TemporaryRepository::initialize().await.unwrap();
        let state_root = repository.root().join("real-codex-state");
        let paths = RuntimePaths::discover(Some(&state_root)).unwrap();
        let start = || {
            let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_hook-daemon"));
            command.arg("daemon").arg(&state_root).env("CODEX_CLI_PATH", &executable)
                .env("TERM", "xterm-256color")
                .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).kill_on_drop(true);
            #[cfg(windows)]
            command.creation_flags(0x0800_0000);
            command.spawn().expect("real-provider daemon")
        };
        let mut daemon = start();
        let client = connect(&paths).await;
        let workspace_id = match client.request(&ClientRequest::AddWorkspace { name: "Real Codex smoke".into() }).await.unwrap() {
            DaemonResponse::WorkspaceAdded { workspace, .. } => workspace.id,
            _ => panic!("workspace creation failed"),
        };
        let worktree_id = match client.request(&ClientRequest::AddProject {
            workspace_id, repository_path: repository.path().to_str().unwrap().into(),
        }).await.unwrap() {
            DaemonResponse::ProjectAdded { root_worktree, .. } => root_worktree.id,
            _ => panic!("scratch Project creation failed"),
        };
        for _ in 0..2 {
            let id = match client.request(&ClientRequest::CreateSession {
                worktree_id, provider: ProviderKind::Codex, display_name: Some("Real hook smoke".into()),
                model: None, effort: None,
                initial_prompt: Some("Reply with exactly SYLVOPS_SMOKE_OK. Do not use tools or change files.".into()),
                columns: 80, rows: 24,
            }).await.unwrap() {
                DaemonResponse::SessionCreated { session, .. } => session.id,
                _ => panic!("real Codex launch failed; check provider installation and login"),
            };
            let event = tokio::time::timeout(Duration::from_secs(45), async {
                loop {
                    let current = session(&client, id).await;
                    if let Some(event) = current.status.attention
                        && event.reason == sylvops_core::session_state::AttentionReason::SuccessfulCompletion
                    { break event; }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }).await.expect("No completion callback: review the stable hook-daemon source with Codex /hooks, verify login, and rerun. No trust bypass is permitted.");
            let claim = client.request(&ClientRequest::ClaimAttentionNotification).await.unwrap();
            assert!(matches!(claim, DaemonResponse::AttentionNotificationClaim(Some(claim)) if claim.session_id == id && claim.attention_id == event.id));
            assert!(matches!(client.request(&ClientRequest::GetSessionLifecycleHealth { session_id: id }).await.unwrap(),
                DaemonResponse::SessionLifecycleHealth { health: sylvops_core::attention::LifecycleHealth::Ready, .. }));
            assert!(session(&client, id).await.external_session_id.is_some(), "completion must capture Conversation identity");
            resume_and_stop(&client, id, false).await;
        }
        client.request(&ClientRequest::ShutdownDaemon).await.unwrap();
        assert!(bounded(daemon.wait()).await.unwrap().unwrap().success());
        drop(client);
        daemon = start();
        // Reconnect after restart and prove callbacks arrive without modifying hook trust.
        let restarted = connect(&paths).await;
        let id = match restarted.request(&ClientRequest::CreateSession {
            worktree_id, provider: ProviderKind::Codex, display_name: Some("Restart hook smoke".into()),
            model: None, effort: None, initial_prompt: Some("Reply OK. Do not use tools or change files.".into()), columns: 80, rows: 24,
        }).await.unwrap() {
            DaemonResponse::SessionCreated { session, .. } => session.id,
            _ => panic!("real Codex restart launch failed"),
        };
        tokio::time::timeout(Duration::from_secs(45), async {
            loop {
                if session(&restarted, id).await.status.attention.is_some_and(|event|
                    event.reason == sylvops_core::session_state::AttentionReason::SuccessfulCompletion) { break; }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }).await.expect("completion callback missing after daemon restart");
        resume_and_stop(&restarted, id, false).await;
        restarted.request(&ClientRequest::ShutdownDaemon).await.unwrap();
        assert!(bounded(daemon.wait()).await.unwrap().unwrap().success());
    }).await.expect("bounded real Codex smoke timeout");
}

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

#[cfg(windows)]
#[tokio::test]
async fn generated_powershell_hook_preserves_literal_path_characters() {
    tokio::time::timeout(
        Duration::from_secs(60),
        exercise_generated_hook("SylvOps $missing `literal é Ж", false),
    )
    .await
    .expect("PowerShell literal relay path scenario timed out");
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
    std::fs::write(codex_home.join(".fake-codex-shared-daemon"), b"").unwrap();
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

    assert!(matches!(
        client
            .request(&ClientRequest::GetSessionLifecycleHealth { session_id })
            .await
            .unwrap(),
        DaemonResponse::SessionLifecycleHealth {
            health: sylvops_core::attention::LifecycleHealth::Ready,
            ..
        }
    ));
    let attached = client
        .request(&ClientRequest::AttachSession {
            session_id,
            from_sequence: 0,
            columns: 80,
            rows: 24,
        })
        .await
        .unwrap();
    assert!(!matches!(attached, DaemonResponse::Error(_)));
    client
        .request(&ClientRequest::ReportAttentionFocus {
            session_id: Some(session_id),
        })
        .await
        .unwrap();
    permission_check(&client, session_id).await;
    assert!(matches!(
        client
            .request(&ClientRequest::ClaimAttentionNotification)
            .await
            .unwrap(),
        DaemonResponse::AttentionNotificationClaim(None)
    ));
    client
        .request(&ClientRequest::ReportAttentionFocus { session_id: None })
        .await
        .unwrap();
    input(&client, session_id, "complete-live").await;
    let completion = wait_for_attention(
        &client,
        session_id,
        sylvops_core::session_state::AttentionReason::SuccessfulCompletion,
    )
    .await;
    assert!(
        matches!(client.request(&ClientRequest::ClaimAttentionNotification).await.unwrap(),
        DaemonResponse::AttentionNotificationClaim(Some(claim)) if claim.attention_id == completion.id)
    );
    assert!(matches!(
        client
            .request(&ClientRequest::ClaimAttentionNotification)
            .await
            .unwrap(),
        DaemonResponse::AttentionNotificationClaim(None)
    ));

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
    let stopped = session(&client, session_id).await;
    assert!(stopped.failure_reason.is_none());
    assert!(
        stopped
            .status
            .attention
            .is_none_or(|event| event.acknowledgement
                == sylvops_core::session_state::AttentionAcknowledgement::Resolved)
    );
    assert!(matches!(
        client
            .request(&ClientRequest::ClaimAttentionNotification)
            .await
            .unwrap(),
        DaemonResponse::AttentionNotificationClaim(None)
    ));
    client
        .request(&ClientRequest::ShutdownDaemon)
        .await
        .unwrap();
    assert!(bounded(daemon.wait()).await.unwrap().unwrap().success());
    drop(client);

    // Fake Codex does not implement trust review. Replay the approval metadata written
    // by real Codex so restart and resume exercise the profile validation boundary.
    let profile = std::fs::read_dir(&codex_home)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("sylvops-hooks-v1-")
        })
        .unwrap();
    let original = std::fs::read_to_string(&profile).unwrap();
    let mut approvals = String::new();
    for event in [
        "session_start",
        "session_end",
        "user_prompt_submit",
        "permission_request",
        "pre_tool_use",
        "post_tool_use",
        "subagent_start",
        "subagent_stop",
        "stop",
        "interrupt",
    ] {
        let profile_key = toml::Value::String(format!("{}:{event}:0:0", profile.to_string_lossy()));
        write!(
            approvals,
            "\n[hooks.state.{profile_key}]\ntrusted_hash = 'sha256:{}'\n",
            "a".repeat(64)
        )
        .unwrap();
    }
    let approved = format!(
        "model = 'gpt-test'\nmodel_reasoning_effort = 'medium'\n{original}{approvals}\n[tui]\nstatus_line = ['model-name', 'context-remaining']\nstatus_line_use_colors = true\ntheme = 'example'\n[future_preferences]\nunknown = true\n"
    );
    std::fs::write(&profile, &approved).unwrap();
    let mut restarted = start_daemon(&relay, &codex_home, &state_root);
    let client = connect(&paths).await;
    assert_eq!(
        session(&client, session_id)
            .await
            .external_session_id
            .as_deref(),
        Some(expected_id.as_str())
    );
    resume_and_stop(&client, session_id, true).await;
    let fresh = match client
        .request(&ClientRequest::CreateSession {
            worktree_id,
            provider: ProviderKind::Codex,
            display_name: Some("after preferences".into()),
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
        response => panic!("preferences prevented a fresh Session: {response:?}"),
    };
    bounded(async {
        while session(&client, fresh).await.external_session_id.is_none() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    assert_fake_hook_notifications(&client, fresh).await;
    client
        .request(&ClientRequest::StopSession { session_id: fresh })
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(&profile).unwrap(), approved);
    client
        .request(&ClientRequest::ShutdownDaemon)
        .await
        .unwrap();
    assert!(bounded(restarted.wait()).await.unwrap().unwrap().success());
}

async fn resume_and_stop(
    client: &DaemonClient,
    source_id: SessionId,
    verify_fake_notifications: bool,
) {
    let identity = session(client, source_id)
        .await
        .external_session_id
        .expect("verified Conversation identity before exit");
    client
        .request(&ClientRequest::StopSession {
            session_id: source_id,
        })
        .await
        .unwrap();
    bounded(async {
        while session(client, source_id).await.process_id.is_some() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    let response = client
        .request(&ClientRequest::ResumeSession {
            session_id: source_id,
            columns: 80,
            rows: 24,
        })
        .await
        .unwrap();
    let DaemonResponse::SessionResumed {
        session: successor,
        outcome,
        predecessor_session_id,
        ..
    } = response
    else {
        panic!("verified Conversation failed to resume");
    };
    assert_eq!(
        outcome,
        sylvops_core::protocol::SessionResumeOutcome::CreatedSuccessor
    );
    assert_eq!(predecessor_session_id, Some(source_id));
    assert_ne!(successor.id, source_id);
    assert_eq!(
        successor.external_session_id.as_deref(),
        Some(identity.as_str())
    );
    let DaemonResponse::Snapshot(snapshot) =
        client.request(&ClientRequest::GetSnapshot).await.unwrap()
    else {
        panic!("missing lineage snapshot");
    };
    assert!(
        snapshot
            .session_lineage
            .iter()
            .any(|link| link.predecessor_session_id == source_id
                && link.successor_session_id == successor.id)
    );
    bounded(async {
        loop {
            if matches!(
                client
                    .request(&ClientRequest::GetSessionLifecycleHealth {
                        session_id: successor.id
                    })
                    .await
                    .unwrap(),
                DaemonResponse::SessionLifecycleHealth {
                    health: sylvops_core::attention::LifecycleHealth::Ready,
                    ..
                }
            ) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("resumed Provider did not deliver a lifecycle callback");
    if verify_fake_notifications {
        assert_fake_hook_notifications(client, successor.id).await;
    }
    client
        .request(&ClientRequest::StopSession {
            session_id: successor.id,
        })
        .await
        .unwrap();
    bounded(async {
        while session(client, successor.id).await.process_id.is_some() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
}

async fn assert_fake_hook_notifications(client: &DaemonClient, session_id: SessionId) {
    let attached = client
        .request(&ClientRequest::AttachSession {
            session_id,
            from_sequence: 0,
            columns: 80,
            rows: 24,
        })
        .await
        .unwrap();
    assert!(!matches!(attached, DaemonResponse::Error(_)));
    assert!(matches!(
        client
            .request(&ClientRequest::GetSessionLifecycleHealth { session_id })
            .await
            .unwrap(),
        DaemonResponse::SessionLifecycleHealth {
            health: sylvops_core::attention::LifecycleHealth::Ready,
            ..
        }
    ));
    client
        .request(&ClientRequest::ReportAttentionFocus {
            session_id: Some(session_id),
        })
        .await
        .unwrap();
    permission_check(client, session_id).await;
    assert!(matches!(
        client
            .request(&ClientRequest::ClaimAttentionNotification)
            .await
            .unwrap(),
        DaemonResponse::AttentionNotificationClaim(None)
    ));
    client
        .request(&ClientRequest::ReportAttentionFocus { session_id: None })
        .await
        .unwrap();
    input(client, session_id, "complete-live").await;
    let completed = wait_for_attention(
        client,
        session_id,
        sylvops_core::session_state::AttentionReason::SuccessfulCompletion,
    )
    .await;
    assert!(matches!(
        client.request(&ClientRequest::ClaimAttentionNotification).await.unwrap(),
        DaemonResponse::AttentionNotificationClaim(Some(claim))
            if claim.session_id == session_id && claim.attention_id == completed.id
    ));
    assert!(matches!(
        client
            .request(&ClientRequest::ClaimAttentionNotification)
            .await
            .unwrap(),
        DaemonResponse::AttentionNotificationClaim(None)
    ));
}

async fn permission_check(client: &DaemonClient, session_id: SessionId) {
    let mut events = client.subscribe();
    input(client, session_id, "permission").await;
    bounded(async {
        loop {
            if let sylvops_core::protocol::DaemonEvent::ProviderEvent {
                session_id: received,
                event: sylvops_core::status::NormalizedProviderEvent::PermissionCheckStarted,
                ..
            } = events.recv().await.unwrap()
                && received == session_id
            {
                break;
            }
        }
    })
    .await
    .unwrap();
    let current = session(client, session_id).await;
    assert_eq!(
        current.status.activity,
        sylvops_core::session_state::ProviderActivity::Working
    );
    assert!(
        current
            .status
            .attention
            .is_none_or(|event| event.acknowledgement
                == sylvops_core::session_state::AttentionAcknowledgement::Resolved)
    );
}

async fn input(client: &DaemonClient, session_id: SessionId, line: &str) {
    let newline = if cfg!(windows) { "\r" } else { "\n" };
    assert!(matches!(
        client
            .request(&ClientRequest::SessionInput {
                session_id,
                bytes: format!("{line}{newline}").into_bytes(),
            })
            .await
            .unwrap(),
        DaemonResponse::Acknowledged
    ));
}

async fn wait_for_attention(
    client: &DaemonClient,
    session_id: SessionId,
    reason: sylvops_core::session_state::AttentionReason,
) -> sylvops_core::session_state::AttentionEvent {
    bounded(async {
        loop {
            if let Some(event) = session(client, session_id).await.status.attention
                && event.reason == reason
                && event.acknowledgement
                    == sylvops_core::session_state::AttentionAcknowledgement::Pending
            {
                return event;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("generated hook did not create pending attention")
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
