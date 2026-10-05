//! Authoritative daemon lifecycle, entity mutations, and persistent PTY sessions.

use std::{
    collections::{HashMap, HashSet},
    fs::OpenOptions,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use subtle::ConstantTimeEq;
use sylvops_core::{
    domain::{
        DaemonHealth, ProviderKind, ProviderProfile, Session, SessionState, WorktreeStatus,
        state_allows_resume,
    },
    ids::{ProjectId, ProviderProfileId, SessionId, WorktreeId},
    protocol::{
        ClientRequest, DaemonEvent, DaemonResponse, Frame, HelloRequest, MessageClass,
        PROTOCOL_MAJOR, PROTOCOL_MINOR, ProtocolFailure, WelcomeResponse, read_frame,
        validate_terminal_size, write_frame,
    },
    provider::{AuthenticationRequirement, LaunchContext, ProviderRuntimeSpec, ResumeContext},
    status::{
        ConversationIdentityTransition, NormalizedProviderEvent, ProviderConversationId,
        SessionStatusMachine,
    },
    upgrade::{
        ActiveUpgradeSession, InstallDisposition, MAX_UPGRADE_ACTIVE_SESSIONS,
        MAX_UPGRADE_CLIENT_PROCESSES, NativeUpgradeHandoff, ReleaseMetadata, UpgradeStatus,
    },
};
use tokio::{
    sync::{Mutex, Notify, OwnedSemaphorePermit, RwLock, Semaphore, broadcast, mpsc, watch},
    task::{AbortHandle, JoinSet},
    time::timeout,
};
use uuid::Uuid;

use crate::{
    DaemonError, Result, config_store,
    data_removal::{DATA_REMOVAL_CONFIRMATION, DataRemovalPlan},
    database::{DatabaseHandle, NewSession, VerifiedConversationIdentityUpdate},
    git,
    hook::{HookCredentials, HookDelivery, HookReceiver},
    ipc::{BoxStream, LocalListener},
    provider::ProviderRegistry,
    runtime::{AuthenticationToken, RuntimePaths},
    session::{Attachment, SessionHandle, SessionSpec},
    upgrade::{
        HttpReleaseSource, UpgradeCoordinator, current_release_target, embedded_verifying_key,
        manifest_url,
    },
};

pub const CONTROL_OPCODE: u16 = 10;
pub const EVENT_OPCODE: u16 = 11;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const CLIENT_QUEUE_CAPACITY: usize = 256;
const CLIENT_BYTE_CAPACITY: usize = 4 * 1024 * 1024;
const LIFECYCLE_QUIESCE_TIMEOUT: Duration = Duration::from_secs(30);
const CLIENT_QUIESCE_TIMEOUT: Duration = Duration::from_secs(10);
const ACTIVE_SESSION_REAP_TIMEOUT: Duration = Duration::from_secs(15);
const CLAUDE_SESSION_START_TIMEOUT: Duration = Duration::from_secs(15);
const CLAUDE_SESSION_START_GUIDANCE: &str = "Claude Code lifecycle hooks did not report SessionStart within 15 seconds. The Terminal remains usable; check Claude hook configuration or restart the Session.";
const UPGRADE_HANDOFF_FILE: &str = "handoff.json";

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
    owned_runtime_paths: Arc<Vec<PathBuf>>,
}

#[derive(Debug)]
struct LifecycleCoordinator {
    quiescing: AtomicBool,
    activities: AtomicU32,
    changed: Notify,
}

impl LifecycleCoordinator {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            quiescing: AtomicBool::new(false),
            activities: AtomicU32::new(0),
            changed: Notify::new(),
        })
    }

    fn begin(self: &Arc<Self>) -> Result<LifecycleActivity> {
        if self.quiescing.load(Ordering::Acquire) {
            return Err(DaemonError::Lifecycle(
                "application lifecycle is quiescing".into(),
            ));
        }
        self.activities.fetch_add(1, Ordering::AcqRel);
        if self.quiescing.load(Ordering::Acquire) {
            if self.activities.fetch_sub(1, Ordering::AcqRel) == 1 {
                self.changed.notify_waiters();
            }
            return Err(DaemonError::Lifecycle(
                "application lifecycle is quiescing".into(),
            ));
        }
        Ok(LifecycleActivity(self.clone()))
    }

    fn is_quiescing(&self) -> bool {
        self.quiescing.load(Ordering::Acquire)
    }

    async fn quiesce(self: &Arc<Self>) -> Result<LifecycleQuiesce> {
        self.quiescing
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| DaemonError::Lifecycle("application lifecycle is busy".into()))?;
        let deadline = tokio::time::Instant::now() + LIFECYCLE_QUIESCE_TIMEOUT;
        loop {
            let notified = self.changed.notified();
            if self.activities.load(Ordering::Acquire) == 0 {
                return Ok(LifecycleQuiesce {
                    coordinator: self.clone(),
                    committed: false,
                });
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() || tokio::time::timeout(remaining, notified).await.is_err() {
                self.quiescing.store(false, Ordering::Release);
                self.changed.notify_waiters();
                return Err(DaemonError::Lifecycle(
                    "application lifecycle did not quiesce before its deadline".into(),
                ));
            }
        }
    }
}

struct LifecycleActivity(Arc<LifecycleCoordinator>);

impl Drop for LifecycleActivity {
    fn drop(&mut self) {
        if self.0.activities.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.changed.notify_waiters();
        }
    }
}

struct LifecycleQuiesce {
    coordinator: Arc<LifecycleCoordinator>,
    committed: bool,
}

impl LifecycleQuiesce {
    fn commit(&mut self) {
        self.committed = true;
    }
}

impl Drop for LifecycleQuiesce {
    fn drop(&mut self) {
        if !self.committed {
            self.coordinator.quiescing.store(false, Ordering::Release);
            self.coordinator.changed.notify_waiters();
        }
    }
}

#[derive(Debug)]
struct ApplicationMutationCoordinator {
    in_progress: AtomicBool,
    handoff_pending: AtomicBool,
    handoff_finalizing: AtomicBool,
}

impl ApplicationMutationCoordinator {
    fn new(handoff_pending: bool) -> Arc<Self> {
        Arc::new(Self {
            in_progress: AtomicBool::new(handoff_pending),
            handoff_pending: AtomicBool::new(handoff_pending),
            handoff_finalizing: AtomicBool::new(false),
        })
    }

    fn begin(self: &Arc<Self>) -> Result<ApplicationMutation> {
        self.in_progress
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| {
                DaemonError::Lifecycle("another application mutation is already in progress".into())
            })?;
        Ok(ApplicationMutation(self.clone()))
    }

    fn handoff_pending(&self) -> bool {
        self.handoff_pending.load(Ordering::Acquire)
    }

    fn begin_handoff_finalization(self: &Arc<Self>) -> Result<HandoffFinalization> {
        if !self.handoff_pending() {
            return Err(DaemonError::Lifecycle(
                "no detached application mutation is awaiting finalization".into(),
            ));
        }
        self.handoff_finalizing
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| {
                DaemonError::Lifecycle("application handoff is already being finalized".into())
            })?;
        Ok(HandoffFinalization {
            coordinator: self.clone(),
            completed: false,
        })
    }
}

#[derive(Debug)]
struct ApplicationMutation(Arc<ApplicationMutationCoordinator>);

impl Drop for ApplicationMutation {
    fn drop(&mut self) {
        self.0.in_progress.store(false, Ordering::Release);
    }
}

#[derive(Debug)]
struct HandoffFinalization {
    coordinator: Arc<ApplicationMutationCoordinator>,
    completed: bool,
}

impl HandoffFinalization {
    fn complete(mut self) {
        self.coordinator
            .handoff_pending
            .store(false, Ordering::Release);
        self.coordinator.in_progress.store(false, Ordering::Release);
        self.coordinator
            .handoff_finalizing
            .store(false, Ordering::Release);
        self.completed = true;
    }
}

impl Drop for HandoffFinalization {
    fn drop(&mut self) {
        if !self.completed {
            self.coordinator
                .handoff_finalizing
                .store(false, Ordering::Release);
        }
    }
}

#[derive(Clone, Debug)]
struct DaemonState {
    database: DatabaseHandle,
    authentication_token: AuthenticationToken,
    started: Instant,
    connected_clients: Arc<AtomicU32>,
    client_processes: Arc<StdMutex<HashMap<u32, u32>>>,
    shutdown: watch::Sender<bool>,
    events: broadcast::Sender<DaemonEvent>,
    sessions: Arc<RwLock<HashMap<SessionId, ManagedSession>>>,
    project_locks: Arc<RwLock<HashMap<ProjectId, Arc<Mutex<()>>>>>,
    worktree_locks: Arc<RwLock<HashMap<WorktreeId, Arc<Mutex<()>>>>>,
    lifecycle: Arc<LifecycleCoordinator>,
    application_mutations: Arc<ApplicationMutationCoordinator>,
    scrollback_bytes: usize,
    managed_worktree_root: PathBuf,
    providers: Arc<ProviderRegistry>,
    hook_credentials: HookCredentials,
    status_machines: Arc<Mutex<HashMap<SessionId, SessionStatusMachine>>>,
    hook_tracking: Arc<Mutex<HashMap<SessionId, HookTracking>>>,
    upgrade: Arc<UpgradeCoordinator>,
    upgrade_handoff: Arc<dyn UpgradeHandoffLauncher>,
    data_removal_handoff: Arc<dyn DataRemovalHandoffLauncher>,
    runtime_paths: RuntimePaths,
}

#[derive(Debug, Default)]
struct HookTracking {
    fingerprints: HashSet<String>,
    stopped_turns: HashSet<String>,
    session_start_missing: bool,
    turn_closed: bool,
}

impl HookTracking {
    fn classify(
        &mut self,
        fingerprint: String,
        event: &NormalizedProviderEvent,
        turn_id: Option<&str>,
    ) -> Option<&'static str> {
        if !self.accept_fingerprint(fingerprint) {
            return Some("duplicate");
        }
        if self.turn_closed
            && !matches!(
                event,
                NormalizedProviderEvent::PromptSubmitted
                    | NormalizedProviderEvent::TurnStarted { .. }
                    | NormalizedProviderEvent::SessionEnded
            )
        {
            return Some("stale");
        }
        if turn_id.is_some_and(|turn_id| self.stopped_turns.contains(turn_id))
            && !matches!(event, NormalizedProviderEvent::TurnStopped { .. })
        {
            return Some("stale");
        }
        if matches!(event, NormalizedProviderEvent::TurnStopped { .. })
            && let Some(turn_id) = turn_id
        {
            self.close_turn(turn_id.to_owned());
        }
        None
    }

    fn record_applied(&mut self, event: &NormalizedProviderEvent, state: SessionState) {
        match event {
            NormalizedProviderEvent::PromptSubmitted
            | NormalizedProviderEvent::TurnStarted { .. } => self.turn_closed = false,
            NormalizedProviderEvent::TurnStopped { .. } => {
                self.turn_closed = matches!(
                    state,
                    SessionState::FinishedUnseen | SessionState::FinishedSeen
                );
            }
            NormalizedProviderEvent::SessionEnded => {
                self.turn_closed = true;
            }
            _ => {}
        }
    }

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

#[async_trait]
pub trait UpgradeHandoffLauncher: std::fmt::Debug + Send + Sync {
    /// Transfers a verified release to the detached platform installer or helper.
    async fn launch(
        &self,
        paths: &RuntimePaths,
        release: ReleaseMetadata,
        client_process_ids: Vec<u32>,
    ) -> Result<()>;
}

#[derive(Debug)]
struct NativeUpgradeHandoffLauncher;

#[async_trait]
impl UpgradeHandoffLauncher for NativeUpgradeHandoffLauncher {
    async fn launch(
        &self,
        paths: &RuntimePaths,
        release: ReleaseMetadata,
        client_process_ids: Vec<u32>,
    ) -> Result<()> {
        spawn_native_upgrade_helper(paths, release, client_process_ids).await
    }
}

#[async_trait]
pub trait DataRemovalHandoffLauncher: std::fmt::Debug + Send + Sync {
    /// Starts the detached daemon-owned helper that completes a prepared data removal.
    async fn launch(
        &self,
        paths: &RuntimePaths,
        handoff_token: &str,
        daemon_process_id: u32,
    ) -> Result<()>;
}

#[derive(Debug)]
struct NativeDataRemovalHandoffLauncher;

#[async_trait]
impl DataRemovalHandoffLauncher for NativeDataRemovalHandoffLauncher {
    async fn launch(
        &self,
        paths: &RuntimePaths,
        handoff_token: &str,
        daemon_process_id: u32,
    ) -> Result<()> {
        spawn_data_removal_helper(paths, handoff_token, daemon_process_id)
    }
}

/// Runs the authoritative daemon until an authenticated shutdown request is received.
///
/// # Errors
///
/// Returns an error when runtime setup, persistence, IPC, hook binding, or cleanup fails.
pub async fn run(paths: RuntimePaths) -> Result<()> {
    run_with_handoff_launchers(
        paths,
        Arc::new(NativeUpgradeHandoffLauncher),
        Arc::new(NativeDataRemovalHandoffLauncher),
    )
    .await
}

/// Runs the daemon with an explicit native-upgrade handoff boundary.
///
/// This is public so cross-platform IPC integration tests can replace the platform package
/// launcher without executing an installer.
///
/// # Errors
///
/// Returns an error when runtime setup, persistence, IPC, hook binding, or cleanup fails.
#[doc(hidden)]
#[allow(clippy::too_many_lines)]
pub async fn run_with_handoff_launcher(
    paths: RuntimePaths,
    upgrade_handoff: Arc<dyn UpgradeHandoffLauncher>,
) -> Result<()> {
    run_with_handoff_launchers(
        paths,
        upgrade_handoff,
        Arc::new(NativeDataRemovalHandoffLauncher),
    )
    .await
}

/// Runs the daemon with explicit upgrade and data-removal handoff boundaries.
///
/// # Errors
///
/// Returns an error when runtime setup, persistence, IPC, hook binding, or cleanup fails.
#[doc(hidden)]
#[allow(clippy::too_many_lines)]
pub async fn run_with_handoff_launchers(
    paths: RuntimePaths,
    upgrade_handoff: Arc<dyn UpgradeHandoffLauncher>,
    data_removal_handoff: Arc<dyn DataRemovalHandoffLauncher>,
) -> Result<()> {
    if crate::data_removal::reservation_pending(&paths)? {
        return Err(DaemonError::Lifecycle(
            "user-data removal is still in progress".into(),
        ));
    }
    paths.prepare()?;
    let config = config_store::load(&paths.config, &paths.machine_config)?;
    let managed_worktree_root = prepare_managed_worktree_root(&paths, &config).await?;
    let mut listener = LocalListener::bind(&paths.endpoint)?;
    let relay_executable = canonical_current_executable()?;
    let mut hook_receiver = HookReceiver::bind(
        relay_executable,
        config.hook_body_limit_bytes,
        config.hook_requests_per_minute,
    )
    .await?;
    let hook_credentials = hook_receiver.credentials();
    let providers = Arc::new(ProviderRegistry::new_managed(
        hook_receiver.relay_executable(),
        &paths.runtime_directory,
        hook_receiver.endpoint_url(),
        &config.enabled_providers,
    )?);
    let upgrade = prepare_upgrade(&paths).await?;
    let upgrade_handoff_pending = tokio::fs::try_exists(
        paths
            .data_directory
            .join("upgrades")
            .join(UPGRADE_HANDOFF_FILE),
    )
    .await?;
    let mut hook_deliveries = hook_receiver.take_deliveries();
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
        client_processes: Arc::new(StdMutex::new(HashMap::new())),
        shutdown,
        events,
        sessions: Arc::new(RwLock::new(HashMap::new())),
        project_locks: Arc::new(RwLock::new(HashMap::new())),
        worktree_locks: Arc::new(RwLock::new(HashMap::new())),
        lifecycle: LifecycleCoordinator::new(),
        application_mutations: ApplicationMutationCoordinator::new(upgrade_handoff_pending),
        scrollback_bytes: config.scrollback_capacity_bytes,
        managed_worktree_root,
        providers,
        hook_credentials,
        status_machines: Arc::new(Mutex::new(HashMap::new())),
        hook_tracking: Arc::new(Mutex::new(HashMap::new())),
        upgrade,
        upgrade_handoff,
        data_removal_handoff,
        runtime_paths: paths.clone(),
    };
    tracing::info!(reconciled, "SylvOps daemon is ready");

    let mut clients = JoinSet::new();
    if config.update_check_policy == sylvops_core::config::UpdateCheckPolicy::Enabled {
        let periodic_state = state.clone();
        clients.spawn(periodic_update_checks(
            periodic_state,
            Duration::from_secs(u64::from(config.update_check_interval_hours) * 60 * 60),
        ));
    }
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
    state.hook_credentials.invalidate_all();
    hook_receiver.shutdown().await;
    let database_result = database.shutdown().await;
    remove_token_if_owned(&paths, &token);
    database_result?;
    tracing::info!("SylvOps daemon stopped");
    serving_result
}

fn canonical_current_executable() -> Result<PathBuf> {
    let executable = std::env::current_exe().map_err(|error| {
        DaemonError::Lifecycle(format!("cannot resolve hook relay executable: {error}"))
    })?;
    std::fs::canonicalize(executable).map_err(|error| {
        DaemonError::Lifecycle(format!(
            "cannot canonicalize hook relay executable: {error}"
        ))
    })
}

async fn prepare_managed_worktree_root(
    paths: &RuntimePaths,
    config: &sylvops_core::config::AppConfig,
) -> Result<PathBuf> {
    let directory = config
        .managed_worktree_directory
        .clone()
        .unwrap_or_else(|| paths.data_directory.join("worktrees"));
    git::prepare_managed_root(&directory).await
}

async fn prepare_upgrade(paths: &RuntimePaths) -> Result<Arc<UpgradeCoordinator>> {
    let release_target = current_release_target()?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| DaemonError::Lifecycle("system clock is before the Unix epoch".into()))?
        .as_secs()
        .try_into()
        .map_err(|_| {
            DaemonError::Lifecycle("system clock is outside the supported range".into())
        })?;
    let update_source = HttpReleaseSource::github(&manifest_url(release_target))?;
    let installed_executable = canonical_current_executable()?;
    let upgrade = Arc::new(UpgradeCoordinator::new_for_installation(
        &paths.data_directory.join("upgrades"),
        &installed_executable,
        Box::new(update_source),
        embedded_verifying_key()?,
        sylvops_core::upgrade::ReleaseValidationContext {
            current_version: env!("CARGO_PKG_VERSION").into(),
            expected_target: release_target,
            now_unix_seconds: now,
            oldest_allowed_publication: now.saturating_sub(180 * 24 * 60 * 60),
            newest_seen_publication: None,
        },
    )?);
    if let Err(error) = upgrade.recover_staged().await {
        tracing::warn!(error = %error, "discarded invalid staged application upgrade");
    }
    Ok(upgrade)
}

async fn spawn_native_upgrade_helper(
    paths: &RuntimePaths,
    release: ReleaseMetadata,
    client_process_ids: Vec<u32>,
) -> Result<()> {
    let staging_root = paths.data_directory.join("upgrades");
    let installed_executable =
        crate::native_upgrade::active_installed_executable(release.target.installer)?;
    let helper = staging_root.join(if cfg!(windows) {
        "sylvops-upgrade-helper.exe"
    } else {
        "sylvops-upgrade-helper"
    });
    tokio::fs::copy(&installed_executable, &helper).await?;
    let permissions = tokio::fs::metadata(&installed_executable)
        .await?
        .permissions();
    tokio::fs::set_permissions(&helper, permissions).await?;
    crate::native_upgrade::verify_detached_upgrade_helper(&helper)?;
    let handoff = NativeUpgradeHandoff {
        release,
        staging_root: staging_root.clone(),
        installed_executable,
        data_directory: paths.data_directory.clone(),
        config_directory: paths.config_directory.clone(),
        runtime_directory: paths.runtime_directory.clone(),
        client_process_ids,
        relaunch_desktop: true,
    };
    let handoff_path = staging_root.join(UPGRADE_HANDOFF_FILE);
    let encoded = serde_json::to_vec(&handoff)
        .map_err(|_| DaemonError::Lifecycle("could not encode upgrade handoff".into()))?;
    crate::atomic_file::write(&handoff_path, &encoded)?;
    let result = (|| {
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&paths.daemon_log)?;
        let error_log = log.try_clone()?;
        crate::background_process::command(&helper)
            .arg("update-helper")
            .arg("--handoff")
            .arg(&handoff_path)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(error_log))
            .spawn()
            .map_err(|error| {
                DaemonError::Lifecycle(format!("could not start upgrade helper: {error}"))
            })?;
        Ok(())
    })();
    if result.is_err() {
        let _ = tokio::fs::remove_file(&handoff_path).await;
    }
    result
}

fn spawn_data_removal_helper(
    paths: &RuntimePaths,
    handoff_token: &str,
    daemon_process_id: u32,
) -> Result<()> {
    let executable = canonical_current_executable()?;
    for directory in [
        &paths.data_directory,
        &paths.config_directory,
        &paths.runtime_directory,
    ] {
        let canonical = std::fs::canonicalize(directory).map_err(|_| {
            DaemonError::Lifecycle("data-removal helper path validation failed".into())
        })?;
        if executable.starts_with(canonical) {
            return Err(DaemonError::Lifecycle(
                "data-removal helper cannot run from a removal target".into(),
            ));
        }
    }
    let mut command = crate::background_process::command(executable);
    command
        .arg("data-removal-helper")
        .arg("--data-directory")
        .arg(&paths.data_directory)
        .arg("--config-directory")
        .arg(&paths.config_directory)
        .arg("--runtime-directory")
        .arg(&paths.runtime_directory)
        .arg("--daemon-process-id")
        .arg(daemon_process_id.to_string())
        .arg("--handoff-token")
        .arg(handoff_token)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    configure_detached_data_removal_helper(&mut command);
    command.spawn().map_err(|error| {
        DaemonError::Lifecycle(format!("could not start data-removal helper: {error}"))
    })?;
    Ok(())
}

#[cfg(unix)]
fn configure_detached_data_removal_helper(command: &mut tokio::process::Command) {
    use std::os::unix::process::CommandExt;

    command.as_std_mut().process_group(0);
}

#[cfg(windows)]
fn configure_detached_data_removal_helper(command: &mut tokio::process::Command) {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Threading::{
        CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, DETACHED_PROCESS,
    };

    command
        .as_std_mut()
        .creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW | DETACHED_PROCESS);
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
    let Some(client_process_id) = complete_handshake(&mut stream, &state).await? else {
        return Ok(());
    };
    let client_id = Uuid::now_v7();
    state.connected_clients.fetch_add(1, Ordering::Relaxed);
    let _client_guard = ClientGuard {
        count: state.connected_clients.clone(),
        processes: state.client_processes.clone(),
        process_id: client_process_id,
    };
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
                if let Err(error) = handle_request(request, request_id, client_id, client_process_id, &state,
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

async fn periodic_update_checks(state: DaemonState, interval: Duration) {
    let mut shutdown = state.shutdown.subscribe();
    let mut ticker = tokio::time::interval(interval.max(Duration::from_secs(60 * 60)));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ticker.tick().await;
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                match state.database.desktop_state().await {
                    Ok(Some(desktop_state)) if !desktop_state.periodic_update_checks => continue,
                    Ok(_) => {}
                    Err(error) => {
                        tracing::debug!(%error, "periodic application upgrade preference could not be read");
                        continue;
                    }
                }
                let Ok(_mutation) = state.application_mutations.begin() else {
                    continue;
                };
                match state.upgrade.check().await {
                    Ok(Some(release)) => {
                        let _ = state.events.send(DaemonEvent::UpgradeProgress {
                            status: UpgradeStatus::Available { release },
                        });
                    }
                    Ok(None) => {}
                    Err(error) => {
                        tracing::debug!(error = %error, "periodic application upgrade check did not complete");
                    }
                }
            }
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
        }
    }
}

async fn wait_for_other_client_processes(
    state: &DaemonState,
    requesting_process_id: u32,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + CLIENT_QUIESCE_TIMEOUT;
    loop {
        let has_other_process = state
            .client_processes
            .lock()
            .map_err(|_| DaemonError::Lifecycle("client process registry is unavailable".into()))?
            .keys()
            .any(|process_id| *process_id != requesting_process_id);
        if !has_other_process {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(DaemonError::Lifecycle(
                "connected applications did not quiesce for the upgrade".into(),
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn handle_request(
    request: ClientRequest,
    request_id: Uuid,
    client_id: Uuid,
    client_process_id: u32,
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
        ClientRequest::GetDesktopState => {
            send_response_queue(
                outgoing,
                request_id,
                &DaemonResponse::DesktopState(state.database.desktop_state().await?),
            )
            .await
        }
        ClientRequest::SaveDesktopState {
            state: desktop_state,
        } => {
            state.database.save_desktop_state(desktop_state).await?;
            send_response_queue(outgoing, request_id, &DaemonResponse::DesktopStateSaved).await
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
            let _lifecycle_activity = state.lifecycle.begin()?;
            if *state.shutdown.borrow() {
                return Err(DaemonError::Lifecycle(
                    "daemon is quiescing for application replacement".into(),
                ));
            }
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
            if health.capabilities.authentication == AuthenticationRequirement::ExistingLogin
                && !health.authenticated
            {
                return Err(DaemonError::Provider(health.diagnostic.unwrap_or_else(
                    || format!("{provider} is not authenticated; sign in explicitly"),
                )));
            }
            let display_name = validated_session_name(display_name, provider)?;
            let lifecycle_endpoint = if state.providers.supports_lifecycle_events(provider)? {
                Some(
                    state
                        .hook_credentials
                        .register(provider, session_id, worktree_id)?,
                )
            } else {
                None
            };
            let spec = match state.providers.launch(
                provider,
                LaunchContext {
                    session_id,
                    worktree_id,
                    cwd: PathBuf::from(&worktree.canonical_path),
                    model,
                    effort,
                    initial_prompt: initial_prompt.clone(),
                    lifecycle_endpoint,
                },
            ) {
                Ok(spec) => spec,
                Err(error) => {
                    state.hook_credentials.invalidate(session_id);
                    return Err(error);
                }
            };
            let command = match path_text(&spec.launch.executable) {
                Ok(command) => command,
                Err(error) => {
                    state.hook_credentials.invalidate(session_id);
                    state
                        .providers
                        .cleanup_runtime_paths(provider, &spec.owned_paths);
                    return Err(error);
                }
            };
            let arguments_json = persisted_launch_arguments(state, session_id, provider, &spec)?;
            let record = NewSession {
                id: session_id,
                worktree_id,
                display_name,
                provider_profile_id: Some(provider_profile_id(provider)),
                provider_kind: provider,
                command,
                arguments_json,
                cwd: worktree.canonical_path.clone(),
                external_session_id: None,
                resumed_from_session_id: None,
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
            let _lifecycle_activity = state.lifecycle.begin()?;
            if *state.shutdown.borrow() {
                return Err(DaemonError::Lifecycle(
                    "daemon is quiescing for application replacement".into(),
                ));
            }
            validate_terminal_size(columns, rows).map_err(DaemonError::InvalidSession)?;
            let source = state.database.session(source_session_id).await?;
            let external_session_id = source.external_session_id.clone().ok_or_else(|| {
                DaemonError::Provider("session has no verified provider resume identifier".into())
            })?;
            if !state_allows_resume(source.state) {
                return Err(DaemonError::Provider(format!(
                    "session state {} is not eligible for resume; only finished, failed, or disconnected sessions may be resumed",
                    source.state
                )));
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
            let lifecycle_endpoint = if state
                .providers
                .supports_lifecycle_events(source.provider_kind)?
            {
                Some(state.hook_credentials.register(
                    source.provider_kind,
                    session_id,
                    source.worktree_id,
                )?)
            } else {
                None
            };
            let spec = match state.providers.resume(
                source.provider_kind,
                ResumeContext {
                    session_id,
                    worktree_id: source.worktree_id,
                    external_session_id: external_session_id.clone(),
                    cwd: PathBuf::from(&worktree.canonical_path),
                    model: None,
                    effort: None,
                    lifecycle_endpoint,
                },
            ) {
                Ok(spec) => spec,
                Err(error) => {
                    state.hook_credentials.invalidate(session_id);
                    return Err(error);
                }
            };
            let command = match path_text(&spec.launch.executable) {
                Ok(command) => command,
                Err(error) => {
                    state.hook_credentials.invalidate(session_id);
                    state
                        .providers
                        .cleanup_runtime_paths(source.provider_kind, &spec.owned_paths);
                    return Err(error);
                }
            };
            let arguments_json =
                persisted_launch_arguments(state, session_id, source.provider_kind, &spec)?;
            let record = NewSession {
                id: session_id,
                worktree_id: source.worktree_id,
                display_name: format!("{} (resumed)", source.display_name)
                    .chars()
                    .take(200)
                    .collect(),
                provider_profile_id: source.provider_profile_id,
                provider_kind: source.provider_kind,
                command,
                arguments_json,
                cwd: worktree.canonical_path,
                external_session_id: Some(external_session_id),
                resumed_from_session_id: Some(source_session_id),
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
        ClientRequest::CheckForUpdate => {
            let _mutation = state.application_mutations.begin()?;
            let _lifecycle_activity = state.lifecycle.begin()?;
            if let Some(release) = state.upgrade.check().await? {
                let status = UpgradeStatus::Available {
                    release: release.clone(),
                };
                let _ = state.events.send(DaemonEvent::UpgradeProgress { status });
                send_response_queue(
                    outgoing,
                    request_id,
                    &DaemonResponse::UpdateAvailable(release),
                )
                .await
            } else {
                let version = env!("CARGO_PKG_VERSION").to_owned();
                let status = UpgradeStatus::UpToDate {
                    version: version.clone(),
                };
                let _ = state.events.send(DaemonEvent::UpgradeProgress { status });
                send_response_queue(
                    outgoing,
                    request_id,
                    &DaemonResponse::UpdateNotAvailable { version },
                )
                .await
            }
        }
        ClientRequest::DownloadUpdate => {
            let _mutation = state.application_mutations.begin()?;
            let _lifecycle_activity = state.lifecycle.begin()?;
            let staged = state.upgrade.download().await?;
            let status = UpgradeStatus::Staged {
                release: staged.release.clone(),
            };
            let _ = state.events.send(DaemonEvent::UpgradeProgress { status });
            send_response_queue(
                outgoing,
                request_id,
                &DaemonResponse::UpdateStaged(staged.release),
            )
            .await
        }
        ClientRequest::CancelUpdateDownload => {
            state.upgrade.cancel_download();
            send_response_queue(outgoing, request_id, &DaemonResponse::Acknowledged).await
        }
        ClientRequest::GetUpdateStatus => {
            send_response_queue(
                outgoing,
                request_id,
                &DaemonResponse::UpdateStatus(state.upgrade.status().await),
            )
            .await
        }
        ClientRequest::FinalizeUpdate { version, outcome } => {
            let handoff_pending = state.application_mutations.handoff_pending();
            let handoff_finalization = if handoff_pending {
                Some(state.application_mutations.begin_handoff_finalization()?)
            } else {
                None
            };
            let _mutation = if handoff_pending {
                None
            } else {
                Some(state.application_mutations.begin()?)
            };
            let _lifecycle_activity = state.lifecycle.begin()?;
            state.upgrade.record_outcome(&version, outcome).await?;
            let status = state.upgrade.status().await;
            let _ = state.events.send(DaemonEvent::UpgradeProgress {
                status: status.clone(),
            });
            state
                .database
                .audit(
                    "application_upgrade",
                    "succeeded",
                    &serde_json::json!({
                        "version": version,
                        "result": match outcome {
                            sylvops_core::upgrade::NativeUpgradeOutcome::Installed => "installed",
                            sylvops_core::upgrade::NativeUpgradeOutcome::RolledBack => "rolled_back",
                        }
                    })
                    .to_string(),
                )
                .await?;
            if handoff_pending {
                tokio::fs::remove_file(
                    state
                        .runtime_paths
                        .data_directory
                        .join("upgrades")
                        .join(UPGRADE_HANDOFF_FILE),
                )
                .await?;
                handoff_finalization
                    .expect("pending handoff has a finalization lease")
                    .complete();
            }
            send_response_queue(outgoing, request_id, &DaemonResponse::Acknowledged).await
        }
        ClientRequest::InstallUpdate {
            confirmed_active_sessions,
            requesting_process_id,
        } => {
            if requesting_process_id != client_process_id {
                return Err(DaemonError::Lifecycle(
                    "upgrade request process ID does not match its authenticated client".into(),
                ));
            }
            if confirmed_active_sessions.len() > MAX_UPGRADE_ACTIVE_SESSIONS {
                return Err(DaemonError::Lifecycle(
                    "active-session upgrade confirmation is oversized".into(),
                ));
            }
            let _mutation = state.application_mutations.begin()?;
            let mut lifecycle_quiesce = state.lifecycle.quiesce().await?;
            let managed_sessions: Vec<_> = state.sessions.read().await.values().cloned().collect();
            let mut active = Vec::new();
            let mut active_handles = Vec::new();
            for managed in managed_sessions {
                let record = managed.record.read().await.clone();
                if matches!(
                    record.state,
                    SessionState::Starting | SessionState::Running | SessionState::NeedsFeedback
                ) {
                    if active.len() >= MAX_UPGRADE_ACTIVE_SESSIONS {
                        return Err(DaemonError::Lifecycle(
                            "too many active sessions to coordinate an upgrade".into(),
                        ));
                    }
                    active.push(ActiveUpgradeSession {
                        id: record.id,
                        name: record.display_name,
                    });
                    active_handles.push(managed);
                }
            }
            active.sort_by_key(|session| session.id.to_string());
            let active_ids = active
                .iter()
                .map(|session| session.id)
                .collect::<HashSet<_>>();
            let confirmed_ids = confirmed_active_sessions
                .iter()
                .copied()
                .collect::<HashSet<_>>();
            let override_confirmed = !active.is_empty()
                && confirmed_ids.len() == confirmed_active_sessions.len()
                && confirmed_ids == active_ids;
            let disposition = state
                .upgrade
                .prepare_install(&active, override_confirmed)
                .await?;
            if matches!(disposition, InstallDisposition::Prepared { .. }) && override_confirmed {
                let stop_result = timeout(ACTIVE_SESSION_REAP_TIMEOUT, async {
                    for managed in &active_handles {
                        managed.handle.stop().await?;
                    }
                    for managed in active_handles {
                        let mut completed = managed.completed.clone();
                        while !*completed.borrow() && completed.changed().await.is_ok() {}
                    }
                    Ok::<(), DaemonError>(())
                })
                .await
                .map_err(|_| {
                    DaemonError::Lifecycle(
                        "active sessions did not stop before the upgrade deadline".into(),
                    )
                })
                .and_then(std::convert::identity);
                if let Err(error) = stop_result {
                    state.upgrade.cancel_prepared_install().await;
                    return Err(error);
                }
            }
            let native_handoff = if let InstallDisposition::Prepared { .. } = &disposition {
                let UpgradeStatus::Installing { release } = state.upgrade.status().await else {
                    return Err(DaemonError::Lifecycle(
                        "upgrade handoff lost its verified release".into(),
                    ));
                };
                let client_process_ids = state
                    .client_processes
                    .lock()
                    .map_err(|_| {
                        DaemonError::Lifecycle("client process registry is unavailable".into())
                    })?
                    .keys()
                    .copied()
                    .collect::<Vec<_>>();
                if client_process_ids.len() > MAX_UPGRADE_CLIENT_PROCESSES {
                    state.upgrade.cancel_prepared_install().await;
                    return Err(DaemonError::Lifecycle(
                        "too many connected applications to coordinate an upgrade".into(),
                    ));
                }
                Some((release, client_process_ids))
            } else {
                None
            };
            let _ = state.events.send(DaemonEvent::UpgradeProgress {
                status: state.upgrade.status().await,
            });
            let prepared = matches!(disposition, InstallDisposition::Prepared { .. });
            if let Some((release, client_process_ids)) = native_handoff {
                let handoff_result = async {
                    wait_for_other_client_processes(state, client_process_id).await?;
                    state
                        .upgrade_handoff
                        .launch(&state.runtime_paths, release, client_process_ids)
                        .await
                }
                .await;
                if let Err(error) = handoff_result {
                    state.upgrade.cancel_prepared_install().await;
                    let _ = state.events.send(DaemonEvent::UpgradeProgress {
                        status: state.upgrade.status().await,
                    });
                    return Err(error);
                }
            }
            send_response_queue(
                outgoing,
                request_id,
                &DaemonResponse::UpdateInstall(disposition),
            )
            .await?;
            if prepared {
                state.shutdown.send_replace(true);
                lifecycle_quiesce.commit();
            }
            Ok(())
        }
        ClientRequest::PrepareDataRemoval { confirmation } => {
            let _mutation = state.application_mutations.begin()?;
            let mut lifecycle_quiesce = state.lifecycle.quiesce().await?;
            if confirmation != DATA_REMOVAL_CONFIRMATION {
                state
                    .database
                    .audit(
                        "user_data_removal_prepared",
                        "failed",
                        r#"{"reason":"confirmation_required"}"#,
                    )
                    .await?;
                return Err(crate::data_removal::DataRemovalError::ConfirmationRequired.into());
            }
            let managed_sessions: Vec<_> = state.sessions.read().await.values().cloned().collect();
            for managed in managed_sessions {
                if matches!(
                    managed.record.read().await.state,
                    SessionState::Starting | SessionState::Running | SessionState::NeedsFeedback
                ) {
                    state
                        .database
                        .audit(
                            "user_data_removal_prepared",
                            "failed",
                            r#"{"reason":"active_sessions"}"#,
                        )
                        .await?;
                    return Err(DaemonError::Lifecycle(
                        "user data cannot be removed while sessions are active".into(),
                    ));
                }
            }
            let snapshot = state.database.snapshot().await?;
            let protected_paths = snapshot
                .projects
                .iter()
                .map(|project| PathBuf::from(&project.canonical_repository_path))
                .chain(
                    snapshot
                        .worktrees
                        .iter()
                        .map(|worktree| PathBuf::from(&worktree.canonical_path)),
                )
                .collect::<Vec<_>>();
            if let Err(error) =
                DataRemovalPlan::prepare(&state.runtime_paths, &confirmation, &protected_paths)
            {
                state
                    .database
                    .audit(
                        "user_data_removal_prepared",
                        "failed",
                        r#"{"reason":"unsafe_targets"}"#,
                    )
                    .await?;
                return Err(error.into());
            }
            let handoff_token = match crate::data_removal::reserve_for_daemon(
                &state.runtime_paths,
                &protected_paths,
            ) {
                Ok(handoff_token) => handoff_token,
                Err(error) => {
                    state
                        .database
                        .audit(
                            "user_data_removal_prepared",
                            "failed",
                            r#"{"reason":"reservation_failed"}"#,
                        )
                        .await?;
                    return Err(error.into());
                }
            };
            if let Err(error) = state
                .data_removal_handoff
                .launch(&state.runtime_paths, &handoff_token, std::process::id())
                .await
            {
                crate::data_removal::clear_reservation(&state.runtime_paths);
                let _ = state
                    .database
                    .audit(
                        "user_data_removal_prepared",
                        "failed",
                        r#"{"reason":"helper_launch_failed"}"#,
                    )
                    .await;
                return Err(error);
            }
            if let Err(error) = state
                .database
                .audit(
                    "user_data_removal_prepared",
                    "succeeded",
                    &serde_json::json!({ "protected_paths": protected_paths.len() }).to_string(),
                )
                .await
            {
                crate::data_removal::clear_reservation(&state.runtime_paths);
                return Err(error);
            }
            if let Err(error) =
                send_response_queue(outgoing, request_id, &DaemonResponse::DataRemovalPrepared)
                    .await
            {
                crate::data_removal::clear_reservation(&state.runtime_paths);
                let _ = state
                    .database
                    .audit(
                        "user_data_removal_prepared",
                        "failed",
                        r#"{"reason":"response_delivery_failed"}"#,
                    )
                    .await;
                return Err(error);
            }
            state.shutdown.send_replace(true);
            lifecycle_quiesce.commit();
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
    runtime: ProviderRuntimeSpec,
    columns: u16,
    rows: u16,
) -> Result<(u64, Session)> {
    let session_id = record.id;
    let provider_kind = record.provider_kind;
    let cwd = PathBuf::from(&record.cwd);
    let owned_runtime_paths = Arc::new(runtime.owned_paths);
    let launch = runtime.launch;
    if let Err(error) = state.database.create_session(record).await {
        state.hook_credentials.invalidate(session_id);
        state
            .providers
            .cleanup_runtime_paths(provider_kind, &owned_runtime_paths);
        return Err(error);
    }
    let spec = SessionSpec {
        program: launch.executable,
        arguments: launch.arguments.into_all(),
        cwd,
        columns,
        rows,
        scrollback_bytes: state.scrollback_bytes,
    };
    let handle = match SessionHandle::spawn_sanitized(&spec, &launch.environment) {
        Ok(handle) => handle,
        Err(error) => {
            state.hook_credentials.invalidate(session_id);
            state
                .providers
                .cleanup_runtime_paths(provider_kind, &owned_runtime_paths);
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
            state.hook_credentials.invalidate(session_id);
            state
                .providers
                .cleanup_runtime_paths(provider_kind, &owned_runtime_paths);
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
        owned_runtime_paths,
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
    spawn_exit_monitor(state.clone(), session_id, managed.clone(), completed_tx);
    if provider_kind == ProviderKind::Claude {
        spawn_claude_session_start_monitor(state.clone(), session_id, managed);
    }
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
            ProviderKind::Claude => "Claude Code".into(),
            other => other.to_string(),
        },
        executable_path: health.executable_path.clone(),
        default_model: None,
        default_effort: None,
        enabled: matches!(
            health.kind,
            ProviderKind::Shell | ProviderKind::Codex | ProviderKind::Claude
        ),
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

fn provider_external_id_update(
    existing: Option<&str>,
    received: Option<&ProviderConversationId>,
    transition: Option<ConversationIdentityTransition>,
) -> Result<Option<ProviderConversationId>> {
    let Some(received) = received else {
        return Ok(None);
    };
    match existing {
        None => Ok(Some(received.clone())),
        Some(existing) if existing == received.as_str() => Ok(None),
        Some(_)
            if transition.is_some_and(|transition| {
                matches!(
                    transition,
                    ConversationIdentityTransition::Cleared
                        | ConversationIdentityTransition::Resumed
                )
            }) =>
        {
            Ok(Some(received.clone()))
        }
        Some(_) => Err(DaemonError::Provider(
            "hook external session identifier changed unexpectedly".into(),
        )),
    }
}

#[allow(clippy::too_many_lines)]
async fn apply_hook_delivery(state: &DaemonState, delivery: HookDelivery) -> Result<()> {
    let persisted = state.database.session(delivery.session_id).await?;
    if persisted.worktree_id != delivery.worktree_id || persisted.provider_kind != delivery.provider
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
    if !state.hook_credentials.is_registered(delivery.session_id)
        || persisted.process_id.is_none()
        || matches!(
            persisted.state,
            SessionState::Failed | SessionState::Terminated | SessionState::Disconnected
        )
    {
        state
            .database
            .audit(
                "provider_hook_ignored",
                "refused",
                &serde_json::json!({
                    "session_id": delivery.session_id,
                    "reason": "late_terminal"
                })
                .to_string(),
            )
            .await?;
        return Ok(());
    }
    let normalized = match state
        .providers
        .normalize_lifecycle_event(delivery.provider, &delivery.payload)
    {
        Ok(normalized) => normalized,
        Err(error) => {
            state
                .database
                .audit(
                    "provider_hook_refused",
                    "refused",
                    &serde_json::json!({
                        "session_id": delivery.session_id,
                        "reason": "invalid_payload"
                    })
                    .to_string(),
                )
                .await?;
            return Err(error);
        }
    };
    let identity_transition = normalized.event.as_ref().and_then(|event| match event {
        NormalizedProviderEvent::TurnStarted {
            conversation: Some(conversation),
        } => Some(conversation.transition),
        _ => None,
    });
    let Some(event) = normalized.event else {
        state
            .database
            .audit(
                "provider_hook_unknown",
                "refused",
                &serde_json::json!({"session_id": delivery.session_id}).to_string(),
            )
            .await?;
        return Err(DaemonError::Provider(
            "provider lifecycle event type is unknown".into(),
        ));
    };
    let is_claude_session_start = delivery.provider == ProviderKind::Claude
        && matches!(
            &event,
            NormalizedProviderEvent::TurnStarted {
                conversation: Some(_)
            }
        );
    let (disposition, session_start_missing) = {
        // The daemon run loop awaits each delivery to completion, so classification, persistence,
        // and recording are serialized even though the lock is not held across database awaits.
        let mut tracking = state.hook_tracking.lock().await;
        let tracking = tracking.entry(delivery.session_id).or_default();
        let disposition =
            tracking.classify(delivery.fingerprint, &event, normalized.turn_id.as_deref());
        (disposition, tracking.session_start_missing)
    };
    if let Some(disposition) = disposition {
        state
            .database
            .audit(
                "provider_hook_ignored",
                "refused",
                &serde_json::json!({
                    "session_id": delivery.session_id,
                    "reason": disposition
                })
                .to_string(),
            )
            .await?;
        return Ok(());
    }
    let recovers_claude_lifecycle = delivery.provider == ProviderKind::Claude
        && session_start_missing
        && normalized.conversation_id.is_some();
    let received_identity = if delivery.provider == ProviderKind::Claude
        && persisted.external_session_id.is_none()
        && identity_transition.is_none()
    {
        None
    } else {
        normalized.conversation_id.as_ref()
    };
    let external_session_id = match provider_external_id_update(
        persisted.external_session_id.as_deref(),
        received_identity,
        identity_transition,
    ) {
        Ok(update) => update,
        Err(error) => {
            state
                .database
                .audit(
                    "provider_hook_refused",
                    "refused",
                    &serde_json::json!({"reason": "external_session_id_changed"}).to_string(),
                )
                .await?;
            return Err(error);
        }
    };
    let conversation_identity = external_session_id.map(|external_session_id| {
        VerifiedConversationIdentityUpdate::new(external_session_id, identity_transition)
    });
    let next = {
        let mut machines = state.status_machines.lock().await;
        let machine = machines
            .entry(delivery.session_id)
            .or_insert_with(|| SessionStatusMachine::new(persisted.state));
        machine.apply(&event)
    };
    let status_update = state
        .database
        .update_session_status(delivery.session_id, next, conversation_identity)
        .await;
    let (revision, session) = match status_update {
        Ok(updated) => updated,
        Err(error) => {
            let latest = state.database.session(delivery.session_id).await?;
            if latest.process_id.is_none()
                || matches!(
                    latest.state,
                    SessionState::Failed | SessionState::Terminated | SessionState::Disconnected
                )
            {
                state
                    .database
                    .audit(
                        "provider_hook_ignored",
                        "refused",
                        &serde_json::json!({
                            "session_id": delivery.session_id,
                            "reason": "late_terminal"
                        })
                        .to_string(),
                    )
                    .await?;
                return Ok(());
            }
            return Err(error);
        }
    };
    state
        .hook_tracking
        .lock()
        .await
        .entry(delivery.session_id)
        .or_default()
        .record_applied(&event, next);
    if let Some(managed) = get_managed_session(state, delivery.session_id).await {
        *managed.record.write().await = session.clone();
    }
    if is_claude_session_start || recovers_claude_lifecycle {
        state
            .hook_tracking
            .lock()
            .await
            .entry(delivery.session_id)
            .or_default()
            .session_start_missing = false;
    }
    if recovers_claude_lifecycle {
        state
            .database
            .audit(
                "provider_lifecycle_recovered",
                "succeeded",
                &serde_json::json!({"session_id": delivery.session_id}).to_string(),
            )
            .await?;
    }
    if matches!(&event, NormalizedProviderEvent::SessionEnded) {
        state.hook_credentials.invalidate(delivery.session_id);
    }
    let _ = state.events.send(DaemonEvent::ProviderEvent {
        provider: persisted.provider_kind,
        session_id: delivery.session_id,
        worktree_id: persisted.worktree_id,
        event,
    });
    let _ = state
        .events
        .send(DaemonEvent::SessionStatusChanged { revision, session });
    Ok(())
}

async fn revoke_session_hook_access(state: &DaemonState, session_id: SessionId) {
    state.hook_credentials.invalidate(session_id);
    state.hook_tracking.lock().await.remove(&session_id);
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

fn persisted_launch_arguments(
    state: &DaemonState,
    session_id: SessionId,
    provider_kind: ProviderKind,
    runtime: &ProviderRuntimeSpec,
) -> Result<String> {
    arguments_json(runtime.launch.arguments.persisted_values()).inspect_err(|_| {
        state.hook_credentials.invalidate(session_id);
        state
            .providers
            .cleanup_runtime_paths(provider_kind, &runtime.owned_paths);
    })
}

fn spawn_exit_monitor(
    state: DaemonState,
    session_id: SessionId,
    managed: ManagedSession,
    completed: watch::Sender<bool>,
) {
    tokio::spawn(async move {
        let mut handle = managed.handle.clone();
        let provider_kind = managed.record.read().await.provider_kind;
        let exit = handle.wait().await;
        revoke_session_hook_access(&state, session_id).await;
        state
            .providers
            .cleanup_runtime_paths(provider_kind, &managed.owned_runtime_paths);
        match exit {
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

fn spawn_claude_session_start_monitor(
    state: DaemonState,
    session_id: SessionId,
    managed: ManagedSession,
) {
    tokio::spawn(async move {
        tokio::time::sleep(CLAUDE_SESSION_START_TIMEOUT).await;
        if *managed.completed.borrow() {
            return;
        }
        state
            .hook_tracking
            .lock()
            .await
            .entry(session_id)
            .or_default()
            .session_start_missing = true;
        match state
            .database
            .mark_session_lifecycle_missing(session_id, CLAUDE_SESSION_START_GUIDANCE.into())
            .await
        {
            Ok(Some((revision, session))) => {
                *managed.record.write().await = session.clone();
                let _ = state
                    .events
                    .send(DaemonEvent::SessionStatusChanged { revision, session });
            }
            Ok(None) => {
                if let Some(tracking) = state.hook_tracking.lock().await.get_mut(&session_id) {
                    tracking.session_start_missing = false;
                }
            }
            Err(error) => {
                if let Some(tracking) = state.hook_tracking.lock().await.get_mut(&session_id) {
                    tracking.session_start_missing = false;
                }
                tracing::warn!(%error, %session_id, "cannot persist missing Claude lifecycle warning");
            }
        }
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
        DaemonError::Upgrade(_) => "upgrade_operation_failed",
        DaemonError::DataRemoval(_) => "data_removal_refused",
        DaemonError::Pty(_) | DaemonError::ProcessTree(_) => "session_runtime_failed",
        DaemonError::SessionStopped | DaemonError::RequestCancelled => "session_unavailable",
        DaemonError::Ipc(_) | DaemonError::Configuration(_) | DaemonError::Lifecycle(_) => {
            "daemon_operation_failed"
        }
    }
}

async fn complete_handshake(stream: &mut BoxStream, state: &DaemonState) -> Result<Option<u32>> {
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
        return Ok(None);
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
        return Ok(None);
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
        return Ok(None);
    }
    if hello.client_process_id == 0 {
        send_failure(
            stream,
            frame.message_id,
            "invalid_request",
            "client process ID is invalid",
            false,
        )
        .await?;
        return Ok(None);
    }
    if !register_client_process(state, hello.client_process_id)? {
        send_failure(
            stream,
            frame.message_id,
            "daemon_quiescing",
            "daemon is not accepting new clients during lifecycle replacement",
            true,
        )
        .await?;
        return Ok(None);
    }
    if let Err(error) = send_response(
        stream,
        frame.message_id,
        &DaemonResponse::Welcome(WelcomeResponse {
            daemon_version: env!("CARGO_PKG_VERSION").into(),
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
        }),
    )
    .await
    {
        unregister_client_process(&state.client_processes, hello.client_process_id);
        return Err(error);
    }
    Ok(Some(hello.client_process_id))
}

fn register_client_process(state: &DaemonState, process_id: u32) -> Result<bool> {
    let mut processes = state
        .client_processes
        .lock()
        .map_err(|_| DaemonError::Lifecycle("client process registry is unavailable".into()))?;
    if state.lifecycle.is_quiescing() {
        return Ok(false);
    }
    *processes.entry(process_id).or_default() += 1;
    Ok(true)
}

fn unregister_client_process(processes: &StdMutex<HashMap<u32, u32>>, process_id: u32) {
    if let Ok(mut processes) = processes.lock()
        && let Some(count) = processes.get_mut(&process_id)
    {
        *count = count.saturating_sub(1);
        if *count == 0 {
            processes.remove(&process_id);
        }
    }
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
struct ClientGuard {
    count: Arc<AtomicU32>,
    processes: Arc<StdMutex<HashMap<u32, u32>>>,
    process_id: u32,
}
impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.count.fetch_sub(1, Ordering::Relaxed);
        unregister_client_process(&self.processes, self.process_id);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ApplicationMutationCoordinator, HookTracking, LifecycleCoordinator,
        provider_external_id_update,
    };
    use sylvops_core::{
        domain::SessionState,
        status::{ConversationIdentityTransition, NormalizedProviderEvent, ProviderConversationId},
    };

    #[tokio::test]
    async fn lifecycle_quiescing_drains_existing_activity_and_refuses_new_work() {
        let lifecycle = LifecycleCoordinator::new();
        let activity = lifecycle.begin().unwrap();
        let waiting = {
            let lifecycle = lifecycle.clone();
            tokio::spawn(async move { lifecycle.quiesce().await.unwrap() })
        };
        while !lifecycle
            .quiescing
            .load(std::sync::atomic::Ordering::Acquire)
        {
            tokio::task::yield_now().await;
        }
        assert!(lifecycle.begin().is_err());

        drop(activity);
        let quiesce = waiting.await.unwrap();
        assert!(lifecycle.begin().is_err());
        drop(quiesce);
        assert!(lifecycle.begin().is_ok());
    }

    #[test]
    fn detached_handoff_allows_only_one_finalizer() {
        let coordinator = ApplicationMutationCoordinator::new(true);
        let finalization = coordinator.begin_handoff_finalization().unwrap();
        assert!(coordinator.begin_handoff_finalization().is_err());
        assert!(coordinator.begin().is_err());

        drop(finalization);
        let finalization = coordinator.begin_handoff_finalization().unwrap();
        finalization.complete();
        assert!(!coordinator.handoff_pending());
        assert!(coordinator.begin().is_ok());
    }

    #[test]
    fn hook_tracking_rejects_stale_turn_events_without_turn_ids() {
        let mut tracking = HookTracking::default();
        let stopped = NormalizedProviderEvent::TurnStopped {
            remaining_work: sylvops_core::status::RemainingWork::default(),
        };
        assert_eq!(tracking.classify("stop".into(), &stopped, None), None);
        tracking.record_applied(&stopped, SessionState::FinishedUnseen);

        assert_eq!(
            tracking.classify(
                "permission".into(),
                &NormalizedProviderEvent::PermissionRequested,
                None,
            ),
            Some("stale")
        );
        assert_eq!(
            tracking.classify(
                "prompt".into(),
                &NormalizedProviderEvent::PromptSubmitted,
                None,
            ),
            None
        );
    }

    #[test]
    fn verified_external_id_is_captured_from_any_event_but_cannot_change_for_codex() {
        let received = ProviderConversationId::new("codex-session-1").unwrap();
        assert_eq!(
            provider_external_id_update(None, Some(&received), None).unwrap(),
            Some(received.clone())
        );
        assert_eq!(
            provider_external_id_update(Some("codex-session-1"), Some(&received), None).unwrap(),
            None
        );
        let changed = ProviderConversationId::new("codex-session-2").unwrap();
        assert!(
            provider_external_id_update(
                Some("codex-session-1"),
                Some(&changed),
                Some(ConversationIdentityTransition::Established),
            )
            .is_err()
        );
    }
}
