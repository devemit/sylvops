//! Authoritative daemon lifecycle, entity mutations, and persistent PTY sessions.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};

use subtle::ConstantTimeEq;
use sylvops_core::{
    domain::{DaemonHealth, ProviderKind, ProviderProfile, Session, SessionState, WorktreeStatus},
    ids::{ProjectId, ProviderProfileId, SessionId, WorktreeId},
    protocol::{
        ClientRequest, DaemonEvent, DaemonResponse, Frame, HelloRequest, MessageClass,
        PROTOCOL_MAJOR, PROTOCOL_MINOR, ProtocolFailure, WelcomeResponse, read_frame,
        validate_terminal_size, write_frame,
    },
    provider::{LaunchContext, LaunchSpec, ResumeContext},
    status::{NormalizedProviderEvent, SessionStatusMachine},
};
use tokio::{
    sync::{Mutex, OwnedSemaphorePermit, RwLock, Semaphore, broadcast, mpsc, watch},
    task::{AbortHandle, JoinSet},
    time::timeout,
};
use uuid::Uuid;

use crate::{
    DaemonError, Result, config_store,
    database::{DatabaseHandle, NewSession},
    git,
    hook::{HookDelivery, HookReceiver},
    ipc::{BoxStream, LocalListener},
    provider::ProviderRegistry,
    runtime::{AuthenticationToken, RuntimePaths},
    session::{Attachment, SessionHandle, SessionSpec},
};

pub const CONTROL_OPCODE: u16 = 10;
pub const EVENT_OPCODE: u16 = 11;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const CLIENT_QUEUE_CAPACITY: usize = 256;
const CLIENT_BYTE_CAPACITY: usize = 4 * 1024 * 1024;

#[derive(Debug)]
struct OutboundFrame {
    frame: Frame,
    _permit: OwnedSemaphorePermit,
}

#[derive(Clone, Debug)]
struct ClientSink {
    sender: mpsc::Sender<OutboundFrame>,
    bytes: Arc<Semaphore>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TryQueueError {
    Full,
    Closed,
}

impl ClientSink {
    async fn send(&self, frame: Frame) -> Result<()> {
        let size = frame_size(&frame)?;
        let permit = timeout(WRITE_TIMEOUT, self.bytes.clone().acquire_many_owned(size))
            .await
            .map_err(|_| DaemonError::Lifecycle("client byte budget timed out".into()))?
            .map_err(|_| DaemonError::Lifecycle("client queue closed".into()))?;
        timeout(
            WRITE_TIMEOUT,
            self.sender.send(OutboundFrame {
                frame,
                _permit: permit,
            }),
        )
        .await
        .map_err(|_| DaemonError::Lifecycle("client queue timed out".into()))?
        .map_err(|_| DaemonError::Lifecycle("client writer stopped".into()))
    }

    fn try_send(&self, frame: Frame) -> std::result::Result<(), TryQueueError> {
        let size = frame_size(&frame).map_err(|_| TryQueueError::Full)?;
        let permit =
            self.bytes
                .clone()
                .try_acquire_many_owned(size)
                .map_err(|error| match error {
                    tokio::sync::TryAcquireError::Closed => TryQueueError::Closed,
                    tokio::sync::TryAcquireError::NoPermits => TryQueueError::Full,
                })?;
        self.sender
            .try_send(OutboundFrame {
                frame,
                _permit: permit,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => TryQueueError::Full,
                mpsc::error::TrySendError::Closed(_) => TryQueueError::Closed,
            })
    }
}

fn frame_size(frame: &Frame) -> Result<u32> {
    u32::try_from(frame.payload.len().saturating_add(52))
        .map_err(|_| DaemonError::Lifecycle("frame size does not fit client budget".into()))
}

#[derive(Clone, Debug)]
struct ManagedSession {
    handle: SessionHandle,
    record: Arc<RwLock<Session>>,
    completed: watch::Receiver<bool>,
}

#[derive(Clone, Debug)]
struct DaemonState {
    database: DatabaseHandle,
    authentication_token: AuthenticationToken,
    started: Instant,
    connected_clients: Arc<AtomicU32>,
    shutdown: watch::Sender<bool>,
    events: broadcast::Sender<DaemonEvent>,
    sessions: Arc<RwLock<HashMap<SessionId, ManagedSession>>>,
    project_locks: Arc<RwLock<HashMap<ProjectId, Arc<Mutex<()>>>>>,
    worktree_locks: Arc<RwLock<HashMap<WorktreeId, Arc<Mutex<()>>>>>,
    scrollback_bytes: usize,
    managed_worktree_root: PathBuf,
    providers: Arc<ProviderRegistry>,
    status_machines: Arc<Mutex<HashMap<SessionId, SessionStatusMachine>>>,
    hook_tracking: Arc<Mutex<HashMap<SessionId, HookTracking>>>,
}

#[derive(Debug, Default)]
struct HookTracking {
    fingerprints: HashSet<String>,
    stopped_turns: HashSet<String>,
}

impl HookTracking {
    fn accept_fingerprint(&mut self, fingerprint: String) -> bool {
        if self.fingerprints.contains(&fingerprint) {
            return false;
        }
        if self.fingerprints.len() >= 2_048 {
            self.fingerprints.clear();
        }
        self.fingerprints.insert(fingerprint);
        true
    }

    fn close_turn(&mut self, turn_id: String) {
        if self.stopped_turns.len() >= 1_024 {
            self.stopped_turns.clear();
        }
        self.stopped_turns.insert(turn_id);
    }
}

/// Runs the authoritative daemon until an authenticated shutdown request is received.
///
/// # Errors
///
/// Returns an error when runtime setup, persistence, IPC, hook binding, or cleanup fails.
pub async fn run(paths: RuntimePaths) -> Result<()> {
    paths.prepare()?;
    let config = config_store::load(&paths.config, &paths.machine_config)?;
    let managed_worktree_directory = config
        .managed_worktree_directory
        .clone()
        .unwrap_or_else(|| paths.data_directory.join("worktrees"));
    let managed_worktree_root = git::prepare_managed_root(&managed_worktree_directory).await?;
    let relay_executable = std::fs::canonicalize(std::env::current_exe().map_err(|error| {
        DaemonError::Lifecycle(format!("cannot resolve hook relay executable: {error}"))
    })?)
    .map_err(|error| {
        DaemonError::Lifecycle(format!(
            "cannot canonicalize hook relay executable: {error}"
        ))
    })?;
    let mut hook_receiver = HookReceiver::bind(
        relay_executable,
        config.hook_body_limit_bytes,
        config.hook_requests_per_minute,
    )
    .await?;
    let providers = Arc::new(ProviderRegistry::new(
        Some(&hook_receiver.endpoint),
        &config.enabled_providers,
    )?);
    let mut hook_deliveries = hook_receiver.take_deliveries();
    let mut listener = LocalListener::bind(&paths.endpoint)?;
    let database = DatabaseHandle::open(&paths.database)?;
    let reconciled = database.reconcile_after_restart().await?;
    let worktree_reconciliation = reconcile_worktrees(&database).await?;
    database
        .audit(
            "daemon_started",
            "succeeded",
            &serde_json::json!({
                "reconciled_sessions": reconciled
                ,"worktree_reconciliation": worktree_reconciliation
            })
            .to_string(),
        )
        .await?;
    let token = AuthenticationToken::generate();
    token.write(&paths.authentication_token)?;
    let (shutdown, mut shutdown_receiver) = watch::channel(false);
    let (events, _) = broadcast::channel(CLIENT_QUEUE_CAPACITY);
    let state = DaemonState {
        database: database.clone(),
        authentication_token: token.clone(),
        started: Instant::now(),
        connected_clients: Arc::new(AtomicU32::new(0)),
        shutdown,
        events,
        sessions: Arc::new(RwLock::new(HashMap::new())),
        project_locks: Arc::new(RwLock::new(HashMap::new())),
        worktree_locks: Arc::new(RwLock::new(HashMap::new())),
        scrollback_bytes: config.scrollback_capacity_bytes,
        managed_worktree_root,
        providers,
        status_machines: Arc::new(Mutex::new(HashMap::new())),
        hook_tracking: Arc::new(Mutex::new(HashMap::new())),
    };
    tracing::info!(reconciled, "SylvOps daemon is ready");

    let mut clients = JoinSet::new();
    let serving_result = loop {
        tokio::select! {
            accepted = listener.accept() => {
                let stream = match accepted { Ok(stream) => stream, Err(error) => break Err(error) };
                let connection_state = state.clone();
                clients.spawn(async move {
                    if let Err(error) = serve_client(stream, connection_state).await {
                        tracing::debug!(%error, "client connection ended");
                    }
                });
            }
            changed = shutdown_receiver.changed() => {
                if changed.is_err() || *shutdown_receiver.borrow() { break Ok(()); }
            }
            completed = clients.join_next(), if !clients.is_empty() => {
                if let Some(Err(error)) = completed { tracing::warn!(%error, "client task panicked"); }
            }
            delivery = hook_deliveries.recv() => {
                if let Some(delivery) = delivery
                    && let Err(error) = apply_hook_delivery(&state, delivery).await
                {
                    tracing::warn!(%error, "provider hook event was refused");
                }
            }
        }
    };

    state.shutdown.send_replace(true);
    while clients.join_next().await.is_some() {}
    stop_all_sessions(&state).await;
    hook_receiver.shutdown().await;
    let database_result = database.shutdown().await;
    remove_token_if_owned(&paths, &token);
    database_result?;
    tracing::info!("SylvOps daemon stopped");
    serving_result
}

async fn reconcile_worktrees(database: &DatabaseHandle) -> Result<serde_json::Value> {
    let snapshot = database.snapshot().await?;
    let registered: HashSet<_> = snapshot
        .worktrees
        .iter()
        .map(|worktree| worktree.canonical_path.clone())
        .collect();
    let mut missing = 0_u64;
    for worktree in snapshot
        .worktrees
        .iter()
        .filter(|worktree| worktree.status == WorktreeStatus::Active)
    {
        if tokio::fs::symlink_metadata(&worktree.canonical_path)
            .await
            .is_err()
        {
            database.mark_worktree_missing(worktree.id).await?;
            missing = missing.saturating_add(1);
        }
    }
    let mut external = Vec::new();
    for project in &snapshot.projects {
        match git::discover_worktree_paths(project).await {
            Ok(paths) => {
                external.extend(paths.into_iter().filter(|path| !registered.contains(path)));
            }
            Err(error) => {
                tracing::debug!(%error, project_id = %project.id, "worktree discovery skipped");
            }
        }
    }
    external.sort();
    external.dedup();
    let external_count = external.len();
    if !external.is_empty() {
        database
            .audit(
                "external_worktrees_discovered",
                "succeeded",
                &serde_json::json!({"paths": &external}).to_string(),
            )
            .await?;
    }
    Ok(serde_json::json!({"missing": missing, "external_read_only": external_count}))
}

#[allow(clippy::too_many_lines)]
async fn serve_client(mut stream: BoxStream, state: DaemonState) -> Result<()> {
    if !complete_handshake(&mut stream, &state).await? {
        return Ok(());
    }
    let client_id = Uuid::now_v7();
    state.connected_clients.fetch_add(1, Ordering::Relaxed);
    let _client_guard = ClientGuard(state.connected_clients.clone());
    let (mut reader, mut writer) = tokio::io::split(stream);
    let (incoming_tx, mut incoming) = mpsc::channel(64);
    let reader_task = tokio::spawn(async move {
        loop {
            let frame = read_frame(&mut reader).await;
            let finished = frame.is_err();
            if incoming_tx.send(frame).await.is_err() || finished {
                return;
            }
        }
    });
    let (outgoing, mut outgoing_rx) = mpsc::channel::<OutboundFrame>(CLIENT_QUEUE_CAPACITY);
    let outgoing = ClientSink {
        sender: outgoing,
        bytes: Arc::new(Semaphore::new(CLIENT_BYTE_CAPACITY)),
    };
    let mut writer_task = tokio::spawn(async move {
        while let Some(outbound) = outgoing_rx.recv().await {
            timeout(WRITE_TIMEOUT, write_frame(&mut writer, &outbound.frame))
                .await
                .map_err(|_| DaemonError::Lifecycle("client writer timed out".into()))??;
        }
        Ok::<(), DaemonError>(())
    });
    let mut lifecycle = state.events.subscribe();
    let mut shutdown = state.shutdown.subscribe();
    let mut attachments: HashMap<SessionId, AbortHandle> = HashMap::new();
    let (connection_failed, mut connection_failures) = mpsc::channel(1);
    let mut writer_finished = false;

    let result: Result<()> = async {
        loop {
            tokio::select! {
            received = incoming.recv() => {
                let frame = received
                    .ok_or_else(|| DaemonError::Lifecycle("client reader stopped".into()))??;
                let request = decode_request(&frame)?;
                let request_id = frame.message_id;
                if let Err(error) = handle_request(request, request_id, client_id, &state,
                    &outgoing, &connection_failed, &mut attachments).await {
                    send_failure_queue(
                        &outgoing,
                        request_id,
                        failure_code(&error),
                        &error.to_string(),
                        false,
                    ).await?;
                }
            }
            event = lifecycle.recv() => match event {
                Ok(event) => send_event(&outgoing, &event).await?,
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    let latest_revision = state.database.snapshot().await?.revision;
                    send_event(
                        &outgoing,
                        &DaemonEvent::StateResynchronizationRequired { latest_revision },
                    ).await?;
                },
                Err(broadcast::error::RecvError::Closed) => break Ok(()),
            },
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() { break Ok(()); }
            }
            failed = connection_failures.recv() => {
                if failed.is_some() {
                    break Err(DaemonError::Lifecycle("client output writer remained blocked".into()));
                }
            }
            joined = &mut writer_task => {
                writer_finished = true;
                match joined {
                    Ok(Ok(())) => break Ok(()),
                    Ok(Err(error)) => break Err(error),
                    Err(error) => break Err(DaemonError::Lifecycle(format!(
                        "client writer task failed: {error}"
                    ))),
                }
            }
            }
        }
    }
    .await;

    for (session_id, task) in attachments {
        task.abort();
        if let Some(session) = get_managed_session(&state, session_id).await {
            let _ = session.handle.detach(client_id).await;
        }
    }
    drop(outgoing);
    reader_task.abort();
    let _ = reader_task.await;
    if !writer_finished {
        let _ = writer_task.await;
    }
    result
}

#[allow(clippy::too_many_lines)]
async fn handle_request(
    request: ClientRequest,
    request_id: Uuid,
    client_id: Uuid,
    state: &DaemonState,
    outgoing: &ClientSink,
    connection_failed: &mpsc::Sender<()>,
    attachments: &mut HashMap<SessionId, AbortHandle>,
) -> Result<()> {
    match request {
        ClientRequest::Hello(_) => {
            send_failure_queue(
                outgoing,
                request_id,
                "already_authenticated",
                "Hello may only be sent once",
                false,
            )
            .await
        }
        ClientRequest::Health => {
            let health = DaemonHealth {
                daemon_version: env!("CARGO_PKG_VERSION").into(),
                process_id: std::process::id(),
                protocol_major: PROTOCOL_MAJOR,
                protocol_minor: PROTOCOL_MINOR,
                uptime_seconds: state.started.elapsed().as_secs(),
                database_ready: true,
                connected_clients: state.connected_clients.load(Ordering::Relaxed),
            };
            send_response_queue(outgoing, request_id, &DaemonResponse::Health(health)).await
        }
        ClientRequest::GetSnapshot => {
            send_response_queue(
                outgoing,
                request_id,
                &DaemonResponse::Snapshot(state.database.snapshot().await?),
            )
            .await
        }
        ClientRequest::GetTuiState => {
            send_response_queue(
                outgoing,
                request_id,
                &DaemonResponse::TuiState(state.database.tui_state().await?),
            )
            .await
        }
        ClientRequest::SaveTuiState { state: tui_state } => {
            state.database.save_tui_state(tui_state).await?;
            send_response_queue(outgoing, request_id, &DaemonResponse::TuiStateSaved).await
        }
        ClientRequest::ListProviders => {
            let health = state.providers.probe_all().await;
            for item in &health {
                let _ = persist_provider_health(state, item).await;
            }
            send_response_queue(outgoing, request_id, &DaemonResponse::Providers(health)).await
        }
        ClientRequest::ProbeProvider { kind } => {
            let health = state.providers.probe(kind).await?;
            persist_provider_health(state, &health).await?;
            send_response_queue(
                outgoing,
                request_id,
                &DaemonResponse::Provider(health.clone()),
            )
            .await?;
            let _ = state
                .events
                .send(DaemonEvent::ProviderHealthChanged { health });
            Ok(())
        }
        ClientRequest::AddWorkspace { name } => {
            let (revision, workspace) = state.database.add_workspace(name).await?;
            send_response_queue(
                outgoing,
                request_id,
                &DaemonResponse::WorkspaceAdded {
                    revision,
                    workspace: workspace.clone(),
                },
            )
            .await?;
            let _ = state.events.send(DaemonEvent::WorkspaceAdded {
                revision,
                workspace,
            });
            Ok(())
        }
        ClientRequest::OpenWorkspace { workspace_id } => {
            let (revision, workspace) = state.database.open_workspace(workspace_id).await?;
            send_response_queue(
                outgoing,
                request_id,
                &DaemonResponse::WorkspaceOpened {
                    revision,
                    workspace: workspace.clone(),
                },
            )
            .await?;
            let _ = state.events.send(DaemonEvent::WorkspaceOpened {
                revision,
                workspace,
            });
            Ok(())
        }
        ClientRequest::AddProject {
            workspace_id,
            repository_path,
        } => {
            let registration =
                git::inspect_repository(workspace_id, Path::new(&repository_path)).await?;
            let (revision, project, root_worktree) =
                state.database.add_project(registration).await?;
            send_response_queue(
                outgoing,
                request_id,
                &DaemonResponse::ProjectAdded {
                    revision,
                    project: project.clone(),
                    root_worktree: root_worktree.clone(),
                },
            )
            .await?;
            let _ = state.events.send(DaemonEvent::ProjectAdded {
                revision,
                project,
                root_worktree,
            });
            Ok(())
        }
        ClientRequest::EnsureProject {
            workspace_id,
            repository_path,
        } => {
            let registration =
                git::inspect_repository(workspace_id, Path::new(&repository_path)).await?;
            let canonical = registration.canonical_repository_path.clone();
            let snapshot = state.database.snapshot().await?;
            if let Some(project) = snapshot
                .projects
                .iter()
                .find(|project| project.canonical_repository_path == canonical)
                .cloned()
            {
                if project.workspace_id != workspace_id {
                    return Err(DaemonError::Git(format!(
                        "repository is already registered in workspace {}",
                        project.workspace_id
                    )));
                }
                let root_worktree = snapshot
                    .worktrees
                    .into_iter()
                    .find(|worktree| worktree.project_id == project.id && worktree.is_root_checkout)
                    .ok_or_else(|| {
                        DaemonError::Database(format!(
                            "project {} has no root checkout record",
                            project.id
                        ))
                    })?;
                return send_response_queue(
                    outgoing,
                    request_id,
                    &DaemonResponse::ProjectReady {
                        revision: snapshot.revision,
                        project,
                        root_worktree,
                        created: false,
                    },
                )
                .await;
            }

            let (revision, project, root_worktree) =
                match state.database.add_project(registration).await {
                    Ok(created) => created,
                    Err(insert_error) => {
                        let snapshot = state.database.snapshot().await?;
                        if let Some(project) = snapshot
                            .projects
                            .iter()
                            .find(|project| project.canonical_repository_path == canonical)
                            .cloned()
                        {
                            if project.workspace_id != workspace_id {
                                return Err(DaemonError::Git(format!(
                                    "repository is already registered in workspace {}",
                                    project.workspace_id
                                )));
                            }
                            let root_worktree = snapshot
                                .worktrees
                                .into_iter()
                                .find(|worktree| {
                                    worktree.project_id == project.id && worktree.is_root_checkout
                                })
                                .ok_or_else(|| {
                                    DaemonError::Database(format!(
                                        "project {} has no root checkout record",
                                        project.id
                                    ))
                                })?;
                            return send_response_queue(
                                outgoing,
                                request_id,
                                &DaemonResponse::ProjectReady {
                                    revision: snapshot.revision,
                                    project,
                                    root_worktree,
                                    created: false,
                                },
                            )
                            .await;
                        }
                        return Err(insert_error);
                    }
                };
            send_response_queue(
                outgoing,
                request_id,
                &DaemonResponse::ProjectReady {
                    revision,
                    project: project.clone(),
                    root_worktree: root_worktree.clone(),
                    created: true,
                },
            )
            .await?;
            let _ = state.events.send(DaemonEvent::ProjectAdded {
                revision,
                project,
                root_worktree,
            });
            Ok(())
        }
        ClientRequest::RenameProject { project_id, name } => {
            let (revision, project) = state.database.rename_project(project_id, name).await?;
            send_response_queue(
                outgoing,
                request_id,
                &DaemonResponse::ProjectUpdated {
                    revision,
                    project: project.clone(),
                },
            )
            .await?;
            let _ = state
                .events
                .send(DaemonEvent::ProjectUpdated { revision, project });
            Ok(())
        }
        ClientRequest::CreateWorktree {
            project_id,
            name,
            branch,
            base_ref,
        } => {
            let operation_lock = project_operation_lock(state, project_id).await;
            let _operation_guard = operation_lock.lock().await;
            let project = state.database.project(project_id).await?;
            let record = git::create_managed_worktree(
                &project,
                &state.managed_worktree_root,
                WorktreeId::new(),
                name,
                &branch,
                base_ref.as_deref(),
            )
            .await?;
            let external_path = record.canonical_path.clone();
            let (revision, worktree) = match state.database.add_worktree(record).await {
                Ok(result) => result,
                Err(error) => {
                    let _ = state
                        .database
                        .audit(
                            "worktree_persistence_failed",
                            "failed",
                            &serde_json::json!({ "preserved_path": external_path }).to_string(),
                        )
                        .await;
                    return Err(error);
                }
            };
            send_response_queue(
                outgoing,
                request_id,
                &DaemonResponse::WorktreeCreated {
                    revision,
                    worktree: worktree.clone(),
                },
            )
            .await?;
            let _ = state
                .events
                .send(DaemonEvent::WorktreeAdded { revision, worktree });
            Ok(())
        }
        ClientRequest::GetWorktreeStatus { worktree_id } => {
            let worktree = state.database.worktree(worktree_id).await?;
            let status = git::inspect_worktree(&worktree).await?;
            send_response_queue(
                outgoing,
                request_id,
                &DaemonResponse::WorktreeStatus(status.clone()),
            )
            .await?;
            let _ = state
                .events
                .send(DaemonEvent::GitStateChanged { worktree: status });
            Ok(())
        }
        ClientRequest::GetDiff { worktree_id } => {
            let worktree = state.database.worktree(worktree_id).await?;
            let diff = git::worktree_diff(&worktree).await?;
            send_response_queue(outgoing, request_id, &DaemonResponse::Diff(diff)).await
        }
        ClientRequest::RemoveWorktree {
            worktree_id,
            confirmation_token,
        } => {
            let worktree = state.database.worktree(worktree_id).await?;
            let project_lock = project_operation_lock(state, worktree.project_id).await;
            let _project_guard = project_lock.lock().await;
            let operation_lock = worktree_operation_lock(state, worktree_id).await;
            let _operation_guard = operation_lock.lock().await;
            let worktree = state.database.worktree(worktree_id).await?;
            if state
                .database
                .worktree_has_live_sessions(worktree_id)
                .await?
                || worktree_has_live_runtime_session(state, worktree_id).await
            {
                return Err(DaemonError::Git(
                    "worktree has a live session and cannot be removed".into(),
                ));
            }
            let project = state.database.project(worktree.project_id).await?;
            git::remove_managed_worktree(
                &project,
                &worktree,
                &state.managed_worktree_root,
                &confirmation_token,
            )
            .await?;
            let (revision, worktree) = match state.database.mark_worktree_removed(worktree_id).await
            {
                Ok(result) => result,
                Err(error) => {
                    let _ = state
                        .database
                        .audit(
                            "worktree_removal_persistence_failed",
                            "failed",
                            &serde_json::json!({ "worktree_id": worktree_id }).to_string(),
                        )
                        .await;
                    return Err(error);
                }
            };
            send_response_queue(
                outgoing,
                request_id,
                &DaemonResponse::WorktreeRemoved {
                    revision,
                    worktree: worktree.clone(),
                },
            )
            .await?;
            let _ = state
                .events
                .send(DaemonEvent::WorktreeRemoved { revision, worktree });
            Ok(())
        }
        ClientRequest::RenameWorktree { worktree_id, name } => {
            let (revision, worktree) = state.database.rename_worktree(worktree_id, name).await?;
            send_response_queue(
                outgoing,
                request_id,
                &DaemonResponse::WorktreeUpdated {
                    revision,
                    worktree: worktree.clone(),
                },
            )
            .await?;
            let _ = state
                .events
                .send(DaemonEvent::WorktreeUpdated { revision, worktree });
            Ok(())
        }
        ClientRequest::CreateSession {
            worktree_id,
            provider,
            display_name,
            model,
            effort,
            initial_prompt,
            columns,
            rows,
        } => {
            validate_terminal_size(columns, rows).map_err(DaemonError::InvalidSession)?;
            let operation_lock = worktree_operation_lock(state, worktree_id).await;
            let _operation_guard = operation_lock.lock().await;
            let worktree = verified_worktree(state, worktree_id).await?;
            let session_id = SessionId::new();
            let health = state.providers.probe(provider).await?;
            persist_provider_health(state, &health).await?;
            if !health.available {
                return Err(DaemonError::Provider(
                    health
                        .diagnostic
                        .unwrap_or_else(|| format!("provider {provider} is unavailable")),
                ));
            }
            if provider == ProviderKind::Codex && !health.authenticated {
                return Err(DaemonError::Provider(
                    "Codex is not authenticated; run `codex login` explicitly".into(),
                ));
            }
            let spec = state.providers.launch(
                provider,
                LaunchContext {
                    session_id,
                    worktree_id,
                    cwd: PathBuf::from(&worktree.canonical_path),
                    model,
                    effort,
                    initial_prompt: initial_prompt.clone(),
                },
            )?;
            let record = NewSession {
                id: session_id,
                worktree_id,
                display_name: validated_session_name(display_name, provider)?,
                provider_profile_id: Some(provider_profile_id(provider)),
                provider_kind: provider,
                command: path_text(&spec.executable)?,
                arguments_json: arguments_json(&spec.arguments)?,
                cwd: worktree.canonical_path.clone(),
                initial_prompt,
                external_session_id: None,
            };
            let (revision, running) =
                spawn_managed_session(state, record, spec, columns, rows).await?;
            send_response_queue(
                outgoing,
                request_id,
                &DaemonResponse::SessionCreated {
                    revision,
                    session: running.clone(),
                },
            )
            .await?;
            let _ = state.events.send(DaemonEvent::SessionCreated {
                revision,
                session: running,
            });
            Ok(())
        }
        ClientRequest::ResumeSession {
            session_id: source_session_id,
            columns,
            rows,
        } => {
            validate_terminal_size(columns, rows).map_err(DaemonError::InvalidSession)?;
            let source = state.database.session(source_session_id).await?;
            let external_session_id = source.external_session_id.clone().ok_or_else(|| {
                DaemonError::Provider("session has no verified provider resume identifier".into())
            })?;
            if !matches!(
                source.state,
                SessionState::FinishedSeen
                    | SessionState::FinishedUnseen
                    | SessionState::Failed
                    | SessionState::Disconnected
            ) {
                return Err(DaemonError::Provider(
                    "only inactive sessions may be resumed".into(),
                ));
            }
            if let Some(managed) = get_managed_session(state, source_session_id).await
                && !*managed.completed.borrow()
            {
                return Err(DaemonError::Provider(
                    "a live provider process already owns this session".into(),
                ));
            }
            let operation_lock = worktree_operation_lock(state, source.worktree_id).await;
            let _operation_guard = operation_lock.lock().await;
            let worktree = verified_worktree(state, source.worktree_id).await?;
            if source.cwd != worktree.canonical_path {
                return Err(DaemonError::Provider(
                    "session worktree identity no longer matches".into(),
                ));
            }
            let session_id = SessionId::new();
            let spec = state.providers.resume(
                source.provider_kind,
                ResumeContext {
                    session_id,
                    worktree_id: source.worktree_id,
                    external_session_id: external_session_id.clone(),
                    cwd: PathBuf::from(&worktree.canonical_path),
                    model: None,
                    effort: None,
                },
            )?;
            let record = NewSession {
                id: session_id,
                worktree_id: source.worktree_id,
                display_name: format!("{} (resumed)", source.display_name)
                    .chars()
                    .take(200)
                    .collect(),
                provider_profile_id: source.provider_profile_id,
                provider_kind: source.provider_kind,
                command: path_text(&spec.executable)?,
                arguments_json: arguments_json(&spec.arguments)?,
                cwd: worktree.canonical_path,
                initial_prompt: None,
                external_session_id: Some(external_session_id),
            };
            let (revision, running) =
                spawn_managed_session(state, record, spec, columns, rows).await?;
            send_response_queue(
                outgoing,
                request_id,
                &DaemonResponse::SessionResumed {
                    revision,
                    session: running.clone(),
                },
            )
            .await?;
            let _ = state.events.send(DaemonEvent::SessionCreated {
                revision,
                session: running,
            });
            Ok(())
        }
        ClientRequest::AttachSession {
            session_id,
            from_sequence,
            columns,
            rows,
        } => {
            validate_terminal_size(columns, rows).map_err(DaemonError::InvalidSession)?;
            let managed = required_session(state, session_id).await?;
            if let Some(existing) = attachments.remove(&session_id) {
                existing.abort();
                let _ = managed.handle.detach(client_id).await;
            }
            let attachment = managed
                .handle
                .attach(client_id, from_sequence, columns, rows)
                .await?;
            if attachment.completion_published {
                let mut completed = managed.completed.clone();
                while !*completed.borrow() && completed.changed().await.is_ok() {}
            }
            let seen = match state.database.mark_session_seen(session_id).await {
                Ok(seen) => seen,
                Err(error) => {
                    let _ = managed.handle.detach(client_id).await;
                    return Err(error);
                }
            };
            if let Some((revision, session)) = seen {
                *managed.record.write().await = session.clone();
                let _ = state
                    .events
                    .send(DaemonEvent::SessionStatusChanged { revision, session });
            }
            let result = send_attachment(
                outgoing,
                request_id,
                session_id,
                managed.clone(),
                attachment,
                connection_failed.clone(),
                attachments,
            )
            .await;
            if result.is_err() {
                let _ = managed.handle.detach(client_id).await;
            }
            result
        }
        ClientRequest::DetachSession { session_id } => {
            if let Some(task) = attachments.remove(&session_id) {
                task.abort();
            }
            if let Some(managed) = get_managed_session(state, session_id).await {
                managed.handle.detach(client_id).await?;
            }
            send_response_queue(outgoing, request_id, &DaemonResponse::Acknowledged).await
        }
        ClientRequest::SessionInput { session_id, bytes } => {
            required_session(state, session_id)
                .await?
                .handle
                .input_from(client_id, bytes)
                .await?;
            send_response_queue(outgoing, request_id, &DaemonResponse::Acknowledged).await
        }
        ClientRequest::ResizeSession {
            session_id,
            columns,
            rows,
        } => {
            required_session(state, session_id)
                .await?
                .handle
                .resize_from(client_id, columns, rows)
                .await?;
            send_response_queue(outgoing, request_id, &DaemonResponse::Acknowledged).await
        }
        ClientRequest::StopSession { session_id } => {
            let managed = required_session(state, session_id).await?;
            managed.handle.stop().await?;
            let mut completed = managed.completed.clone();
            while !*completed.borrow() && completed.changed().await.is_ok() {}
            send_response_queue(outgoing, request_id, &DaemonResponse::Acknowledged).await
        }
        ClientRequest::RenameSession { session_id, name } => {
            let (revision, session) = state.database.rename_session(session_id, name).await?;
            if let Some(managed) = state.sessions.read().await.get(&session_id).cloned() {
                *managed.record.write().await = session.clone();
            }
            send_response_queue(
                outgoing,
                request_id,
                &DaemonResponse::SessionUpdated {
                    revision,
                    session: session.clone(),
                },
            )
            .await?;
            let _ = state
                .events
                .send(DaemonEvent::SessionUpdated { revision, session });
            Ok(())
        }
        ClientRequest::ShutdownDaemon => {
            state
                .database
                .audit("daemon_shutdown_requested", "succeeded", "{}")
                .await?;
            send_response_queue(outgoing, request_id, &DaemonResponse::Acknowledged).await?;
            state.shutdown.send_replace(true);
            Ok(())
        }
    }
}

async fn send_attachment(
    outgoing: &ClientSink,
    request_id: Uuid,
    session_id: SessionId,
    managed: ManagedSession,
    attachment: Attachment,
    connection_failed: mpsc::Sender<()>,
    attachments: &mut HashMap<SessionId, AbortHandle>,
) -> Result<()> {
    let session = managed.record.read().await.clone();
    send_response_queue(
        outgoing,
        request_id,
        &DaemonResponse::Attached {
            session,
            role: attachment.role,
            earliest_sequence: attachment.replay.earliest_sequence,
            replay_through_sequence: attachment.replay.latest_sequence,
            output_gap: attachment.replay.output_gap,
            terminal_snapshot: attachment.replay.terminal_snapshot,
        },
    )
    .await?;
    for chunk in attachment.replay.chunks {
        send_event(
            outgoing,
            &DaemonEvent::SessionOutput {
                session_id,
                sequence: chunk.sequence,
                bytes: chunk.bytes.to_vec(),
                replay: true,
            },
        )
        .await?;
    }
    let mut live = attachment.live_output;
    let output = outgoing.clone();
    let handle = managed.handle.clone();
    let task = tokio::spawn(async move {
        let mut resync_pending = false;
        loop {
            match live.recv().await {
                Ok(chunk) => {
                    let event = DaemonEvent::SessionOutput {
                        session_id,
                        sequence: chunk.sequence,
                        bytes: chunk.bytes.to_vec(),
                        replay: false,
                    };
                    let Ok(frame) = Frame::message(MessageClass::Event, EVENT_OPCODE, &event)
                    else {
                        return;
                    };
                    match output.try_send(frame) {
                        Ok(()) => resync_pending = false,
                        Err(TryQueueError::Full) if !resync_pending => {
                            resync_pending = true;
                            if send_resync(&output, session_id, &handle).await.is_err() {
                                let _ = connection_failed.try_send(());
                                return;
                            }
                        }
                        Err(TryQueueError::Closed) => {
                            let _ = connection_failed.try_send(());
                            return;
                        }
                        Err(TryQueueError::Full) => {}
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    if send_resync(&output, session_id, &handle).await.is_err() {
                        let _ = connection_failed.try_send(());
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    });
    attachments.insert(session_id, task.abort_handle());
    Ok(())
}

async fn send_resync(
    outgoing: &ClientSink,
    session_id: SessionId,
    handle: &SessionHandle,
) -> Result<()> {
    let (snapshot_sequence, columns, rows, terminal_snapshot) = handle.terminal_snapshot().await?;
    send_event(
        outgoing,
        &DaemonEvent::ResynchronizationRequired {
            session_id,
            snapshot_sequence,
            columns,
            rows,
            terminal_snapshot,
        },
    )
    .await
}

async fn spawn_managed_session(
    state: &DaemonState,
    record: NewSession,
    launch: LaunchSpec,
    columns: u16,
    rows: u16,
) -> Result<(u64, Session)> {
    let session_id = record.id;
    let cwd = PathBuf::from(&record.cwd);
    let _ = state.database.create_session(record).await?;
    let spec = SessionSpec {
        program: launch.executable,
        arguments: launch.arguments,
        cwd,
        columns,
        rows,
        scrollback_bytes: state.scrollback_bytes,
    };
    let handle = match SessionHandle::spawn_sanitized(&spec, &launch.environment) {
        Ok(handle) => handle,
        Err(error) => {
            if let Ok((revision, session)) = state
                .database
                .finish_session(
                    session_id,
                    SessionState::Failed,
                    None,
                    Some(error.to_string()),
                )
                .await
            {
                let _ = state
                    .events
                    .send(DaemonEvent::SessionStatusChanged { revision, session });
            }
            return Err(error);
        }
    };
    let (revision, running) = match state
        .database
        .mark_session_running(session_id, handle.process_id())
        .await
    {
        Ok(result) => result,
        Err(error) => {
            let _ = handle.stop().await;
            let _ = state
                .database
                .finish_session(
                    session_id,
                    SessionState::Failed,
                    None,
                    Some(error.to_string()),
                )
                .await;
            return Err(error);
        }
    };
    let (completed_tx, completed) = watch::channel(false);
    let managed = ManagedSession {
        handle: handle.clone(),
        record: Arc::new(RwLock::new(running.clone())),
        completed,
    };
    state
        .sessions
        .write()
        .await
        .insert(session_id, managed.clone());
    state
        .status_machines
        .lock()
        .await
        .entry(session_id)
        .or_insert_with(|| SessionStatusMachine::new(running.state));
    spawn_exit_monitor(state.clone(), session_id, managed, completed_tx);
    Ok((revision, running))
}

async fn persist_provider_health(
    state: &DaemonState,
    health: &sylvops_core::provider::ProviderHealth,
) -> Result<()> {
    let profile = ProviderProfile {
        id: provider_profile_id(health.kind),
        kind: health.kind,
        display_name: match health.kind {
            ProviderKind::Shell => "Plain shell".into(),
            ProviderKind::Codex => "Codex CLI".into(),
            other => other.to_string(),
        },
        executable_path: health.executable_path.clone(),
        default_model: None,
        default_effort: None,
        enabled: matches!(health.kind, ProviderKind::Shell | ProviderKind::Codex),
        capabilities_json: serde_json::to_string(&health.capabilities)
            .map_err(|error| DaemonError::Provider(error.to_string()))?,
        last_probe_status: Some(
            if !health.available {
                "unavailable"
            } else if health.authenticated {
                "healthy"
            } else {
                "unauthenticated"
            }
            .into(),
        ),
        last_probe_at: Some(health.checked_at),
    };
    let _ = state.database.upsert_provider(profile).await?;
    Ok(())
}

#[allow(clippy::too_many_lines)]
async fn apply_hook_delivery(state: &DaemonState, delivery: HookDelivery) -> Result<()> {
    let persisted = state.database.session(delivery.session_id).await?;
    if persisted.worktree_id != delivery.worktree_id
        || persisted.provider_kind != ProviderKind::Codex
    {
        state
            .database
            .audit(
                "provider_hook_refused",
                "refused",
                &serde_json::json!({"reason": "identity_mismatch"}).to_string(),
            )
            .await?;
        return Err(DaemonError::Provider(
            "hook session/worktree association is invalid".into(),
        ));
    }
    if let (Some(existing), Some(received)) = (
        persisted.external_session_id.as_deref(),
        delivery.external_session_id.as_deref(),
    ) && existing != received
    {
        state
            .database
            .audit(
                "provider_hook_refused",
                "refused",
                &serde_json::json!({"reason": "external_session_id_changed"}).to_string(),
            )
            .await?;
        return Err(DaemonError::Provider(
            "hook external session identifier changed unexpectedly".into(),
        ));
    }
    let Some(event) = delivery.event else {
        state
            .database
            .audit(
                "provider_hook_unknown",
                "refused",
                &serde_json::json!({
                    "session_id": delivery.session_id,
                    "event_name": delivery.event_name
                })
                .to_string(),
            )
            .await?;
        return Ok(());
    };
    let disposition = {
        let mut tracking = state.hook_tracking.lock().await;
        let tracking = tracking.entry(delivery.session_id).or_default();
        if !tracking.accept_fingerprint(delivery.fingerprint) {
            Some("duplicate")
        } else if delivery
            .turn_id
            .as_ref()
            .is_some_and(|turn_id| tracking.stopped_turns.contains(turn_id))
            && !matches!(&event, NormalizedProviderEvent::TurnStopped)
        {
            Some("stale")
        } else {
            if matches!(&event, NormalizedProviderEvent::TurnStopped)
                && let Some(turn_id) = delivery.turn_id.as_ref()
            {
                tracking.close_turn(turn_id.clone());
            }
            None
        }
    };
    if let Some(disposition) = disposition {
        state
            .database
            .audit(
                "provider_hook_ignored",
                "refused",
                &serde_json::json!({
                    "session_id": delivery.session_id,
                    "event_name": delivery.event_name,
                    "reason": disposition
                })
                .to_string(),
            )
            .await?;
        return Ok(());
    }
    let next = {
        let mut machines = state.status_machines.lock().await;
        let machine = machines
            .entry(delivery.session_id)
            .or_insert_with(|| SessionStatusMachine::new(persisted.state));
        machine.apply(&event)
    };
    let (revision, session) = state
        .database
        .update_session_status(delivery.session_id, next, delivery.external_session_id)
        .await?;
    if let Some(managed) = get_managed_session(state, delivery.session_id).await {
        *managed.record.write().await = session.clone();
    }
    let _ = state.events.send(DaemonEvent::ProviderEvent {
        session_id: delivery.session_id,
        event,
    });
    let _ = state
        .events
        .send(DaemonEvent::SessionStatusChanged { revision, session });
    Ok(())
}

fn provider_profile_id(kind: ProviderKind) -> ProviderProfileId {
    let value = match kind {
        ProviderKind::Shell => "00000000-0000-0000-0000-000000000001",
        ProviderKind::Codex => "00000000-0000-0000-0000-000000000002",
        ProviderKind::Claude => "00000000-0000-0000-0000-000000000003",
        ProviderKind::Cursor => "00000000-0000-0000-0000-000000000004",
        ProviderKind::Pi => "00000000-0000-0000-0000-000000000005",
    };
    value
        .parse()
        .expect("static provider profile IDs are valid")
}

fn arguments_json(arguments: &[std::ffi::OsString]) -> Result<String> {
    let arguments: Vec<_> = arguments
        .iter()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect();
    serde_json::to_string(&arguments).map_err(|error| DaemonError::Provider(error.to_string()))
}

fn spawn_exit_monitor(
    state: DaemonState,
    session_id: SessionId,
    managed: ManagedSession,
    completed: watch::Sender<bool>,
) {
    tokio::spawn(async move {
        let mut handle = managed.handle.clone();
        match handle.wait().await {
            Ok(exit) => {
                let state_value = if exit.stop_requested {
                    SessionState::Terminated
                } else if exit.exit_code == 0 {
                    if exit.was_attached {
                        SessionState::FinishedSeen
                    } else {
                        SessionState::FinishedUnseen
                    }
                } else {
                    SessionState::Failed
                };
                let exit_code = i32::try_from(exit.exit_code).ok();
                let failure = (state_value == SessionState::Failed)
                    .then(|| format!("process exited with code {}", exit.exit_code));
                match state
                    .database
                    .finish_session(session_id, state_value, exit_code, failure)
                    .await
                {
                    Ok((revision, session)) => {
                        *managed.record.write().await = session.clone();
                        let _ = state.events.send(DaemonEvent::SessionStatusChanged {
                            revision,
                            session: session.clone(),
                        });
                        let _ = state.events.send(DaemonEvent::SessionExited {
                            revision,
                            session_id,
                            state: state_value,
                            exit_code,
                        });
                    }
                    Err(error) => {
                        tracing::error!(%error, %session_id, "failed to persist session exit");
                    }
                }
            }
            Err(error) => {
                tracing::error!(%error, %session_id, "session actor ended without status");
            }
        }
        completed.send_replace(true);
    });
}

async fn verified_worktree(
    state: &DaemonState,
    id: WorktreeId,
) -> Result<sylvops_core::domain::Worktree> {
    let worktree = state.database.worktree(id).await?;
    if worktree.status != WorktreeStatus::Active {
        return Err(DaemonError::InvalidSession(
            "sessions require an active checkout".into(),
        ));
    }
    let actual = tokio::fs::canonicalize(&worktree.canonical_path)
        .await
        .map_err(|error| {
            DaemonError::InvalidSession(format!("worktree path is unavailable: {error}"))
        })?;
    if path_text(&actual)? != worktree.canonical_path {
        return Err(DaemonError::InvalidSession(
            "worktree canonical path changed".into(),
        ));
    }
    git::verify_repository_root(&actual).await?;
    Ok(worktree)
}

async fn required_session(state: &DaemonState, id: SessionId) -> Result<ManagedSession> {
    get_managed_session(state, id)
        .await
        .ok_or_else(|| DaemonError::InvalidSession(format!("session {id} is not live")))
}

async fn get_managed_session(state: &DaemonState, id: SessionId) -> Option<ManagedSession> {
    state.sessions.read().await.get(&id).cloned()
}

async fn worktree_has_live_runtime_session(state: &DaemonState, worktree_id: WorktreeId) -> bool {
    let sessions: Vec<_> = state.sessions.read().await.values().cloned().collect();
    for session in sessions {
        if !*session.completed.borrow() && session.record.read().await.worktree_id == worktree_id {
            return true;
        }
    }
    false
}

async fn worktree_operation_lock(state: &DaemonState, id: WorktreeId) -> Arc<Mutex<()>> {
    if let Some(lock) = state.worktree_locks.read().await.get(&id).cloned() {
        return lock;
    }
    state
        .worktree_locks
        .write()
        .await
        .entry(id)
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

async fn project_operation_lock(state: &DaemonState, id: ProjectId) -> Arc<Mutex<()>> {
    if let Some(operation_lock) = state.project_locks.read().await.get(&id).cloned() {
        return operation_lock;
    }
    state
        .project_locks
        .write()
        .await
        .entry(id)
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

async fn stop_all_sessions(state: &DaemonState) {
    let sessions: Vec<_> = state.sessions.read().await.values().cloned().collect();
    for session in &sessions {
        let _ = session.handle.stop().await;
    }
    for session in sessions {
        let mut completed = session.completed;
        while !*completed.borrow() && completed.changed().await.is_ok() {}
    }
}

fn path_text(path: &Path) -> Result<String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| DaemonError::InvalidSession("path is not valid Unicode".into()))
}

fn validated_session_name(display_name: Option<String>, provider: ProviderKind) -> Result<String> {
    let display_name = display_name.unwrap_or_else(|| match provider {
        ProviderKind::Shell => "Shell".into(),
        ProviderKind::Codex => "Codex".into(),
        other => other.to_string(),
    });
    let display_name = display_name.trim();
    if display_name.is_empty() || display_name.chars().count() > 200 {
        return Err(DaemonError::InvalidSession(
            "session name must contain between 1 and 200 characters".into(),
        ));
    }
    Ok(display_name.to_owned())
}

fn failure_code(error: &DaemonError) -> &'static str {
    match error {
        DaemonError::Protocol(_) => "invalid_request",
        DaemonError::InvalidSession(_) => "session_request_refused",
        DaemonError::Attachment(_) => "attachment_refused",
        DaemonError::Git(_) => "git_validation_failed",
        DaemonError::Provider(_) => "provider_operation_failed",
        DaemonError::Database(_) => "state_operation_failed",
        DaemonError::Pty(_) | DaemonError::ProcessTree(_) => "session_runtime_failed",
        DaemonError::SessionStopped | DaemonError::RequestCancelled => "session_unavailable",
        DaemonError::Ipc(_) | DaemonError::Configuration(_) | DaemonError::Lifecycle(_) => {
            "daemon_operation_failed"
        }
    }
}

async fn complete_handshake(stream: &mut BoxStream, state: &DaemonState) -> Result<bool> {
    let frame = timeout(HANDSHAKE_TIMEOUT, read_frame(stream))
        .await
        .map_err(|_| DaemonError::Lifecycle("client handshake timed out".into()))??;
    let request = decode_request(&frame)?;
    let ClientRequest::Hello(hello) = request else {
        send_failure(
            stream,
            frame.message_id,
            "handshake_required",
            "the first request must be Hello",
            false,
        )
        .await?;
        return Ok(false);
    };
    if !authenticate(&hello, &state.authentication_token) {
        send_failure(
            stream,
            frame.message_id,
            "authentication_failed",
            "local IPC authentication failed",
            false,
        )
        .await?;
        return Ok(false);
    }
    if hello.protocol_major != PROTOCOL_MAJOR || hello.protocol_minor != PROTOCOL_MINOR {
        send_failure(
            stream,
            frame.message_id,
            "unsupported_protocol",
            "client protocol is not supported",
            false,
        )
        .await?;
        return Ok(false);
    }
    send_response(
        stream,
        frame.message_id,
        &DaemonResponse::Welcome(WelcomeResponse {
            daemon_version: env!("CARGO_PKG_VERSION").into(),
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
        }),
    )
    .await?;
    Ok(true)
}

fn decode_request(frame: &Frame) -> Result<ClientRequest> {
    if frame.class != MessageClass::Request
        || frame.opcode != CONTROL_OPCODE
        || frame.correlation_id.is_some()
    {
        return Err(DaemonError::Lifecycle(
            "unsupported control message class or opcode".into(),
        ));
    }
    frame.payload_as().map_err(Into::into)
}

fn authenticate(hello: &HelloRequest, expected: &AuthenticationToken) -> bool {
    let supplied = hello.authentication_token.as_bytes();
    let expected = expected.expose().as_bytes();
    supplied.len() == expected.len() && bool::from(supplied.ct_eq(expected))
}

async fn send_response(
    stream: &mut BoxStream,
    request_id: Uuid,
    response: &DaemonResponse,
) -> Result<()> {
    write_frame(
        stream,
        &Frame::response(CONTROL_OPCODE, request_id, response)?,
    )
    .await?;
    Ok(())
}

async fn send_failure(
    stream: &mut BoxStream,
    request_id: Uuid,
    code: &str,
    message: &str,
    retryable: bool,
) -> Result<()> {
    send_response(
        stream,
        request_id,
        &DaemonResponse::Error(ProtocolFailure {
            code: code.into(),
            message: message.into(),
            retryable,
        }),
    )
    .await
}

async fn send_response_queue(
    outgoing: &ClientSink,
    request_id: Uuid,
    response: &DaemonResponse,
) -> Result<()> {
    send_frame(
        outgoing,
        Frame::response(CONTROL_OPCODE, request_id, response)?,
    )
    .await
}

async fn send_failure_queue(
    outgoing: &ClientSink,
    request_id: Uuid,
    code: &str,
    message: &str,
    retryable: bool,
) -> Result<()> {
    send_response_queue(
        outgoing,
        request_id,
        &DaemonResponse::Error(ProtocolFailure {
            code: code.into(),
            message: message.chars().take(1024).collect(),
            retryable,
        }),
    )
    .await
}

async fn send_event(outgoing: &ClientSink, event: &DaemonEvent) -> Result<()> {
    send_frame(
        outgoing,
        Frame::message(MessageClass::Event, EVENT_OPCODE, event)?,
    )
    .await
}

async fn send_frame(outgoing: &ClientSink, frame: Frame) -> Result<()> {
    outgoing.send(frame).await
}

fn remove_token_if_owned(paths: &RuntimePaths, token: &AuthenticationToken) {
    if AuthenticationToken::read(&paths.authentication_token).is_ok_and(|current| &current == token)
    {
        let _ = std::fs::remove_file(&paths.authentication_token);
    }
}

#[derive(Debug)]
struct ClientGuard(Arc<AtomicU32>);
impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}
