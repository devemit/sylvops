use std::{
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use ed25519_dalek::{Signer, SigningKey};
use sha2::{Digest, Sha256};
use sylvops_core::{
    domain::{ProviderKind, SessionState},
    ids::SessionId,
    protocol::{
        ClientRequest, DaemonEvent, DaemonResponse, Frame, HelloRequest, MessageClass,
        PROTOCOL_MAJOR, PROTOCOL_MINOR, read_frame, write_frame,
    },
    upgrade::{NativeUpgradeOutcome, ReleaseMetadata, SignedReleaseMetadata, UpgradeStatus},
};
use sylvops_daemon::{
    DaemonError,
    client::DaemonClient,
    daemon::{self, CONTROL_OPCODE, UpgradeHandoffLauncher},
    data_removal::{DATA_REMOVAL_CONFIRMATION, DataRemovalPlan, reservation, reservation_pending},
    database::DatabaseHandle,
    ipc::{self, BoxStream},
    runtime::{AuthenticationToken, RuntimePaths},
    upgrade::current_release_target,
};
use sylvops_test_support::TemporaryRepository;

#[derive(Debug, Default)]
struct RecordingHandoff {
    calls: AtomicUsize,
    expected_terminated_session: Mutex<Option<SessionId>>,
}

impl RecordingHandoff {
    fn expect_terminated(&self, session_id: SessionId) {
        *self
            .expected_terminated_session
            .lock()
            .expect("handoff expectation lock") = Some(session_id);
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::Acquire)
    }
}

#[async_trait]
impl UpgradeHandoffLauncher for RecordingHandoff {
    async fn launch(
        &self,
        paths: &RuntimePaths,
        _release: ReleaseMetadata,
        _client_process_ids: Vec<u32>,
    ) -> sylvops_daemon::Result<()> {
        let expected = *self
            .expected_terminated_session
            .lock()
            .map_err(|_| DaemonError::Lifecycle("handoff expectation lock failed".into()))?;
        if let Some(session_id) = expected {
            let database = DatabaseHandle::open(&paths.database)?;
            let snapshot = database.snapshot().await?;
            database.shutdown().await?;
            if !snapshot.sessions.iter().any(|session| {
                session.id == session_id && session.state == SessionState::Terminated
            }) {
                return Err(DaemonError::Lifecycle(
                    "active session was not reaped before upgrade handoff".into(),
                ));
            }
        }
        tokio::fs::write(
            paths.data_directory.join("upgrades/handoff.json"),
            b"test handoff",
        )
        .await?;
        self.calls.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
}

#[derive(Debug)]
struct DaemonFixture {
    client: DaemonClient,
    task: Option<tokio::task::JoinHandle<sylvops_daemon::Result<()>>>,
}

#[derive(Debug)]
struct PendingDaemonTask(Option<tokio::task::JoinHandle<sylvops_daemon::Result<()>>>);

impl Drop for PendingDaemonTask {
    fn drop(&mut self) {
        if let Some(task) = &self.0 {
            task.abort();
        }
    }
}

impl DaemonFixture {
    async fn shutdown(mut self) {
        assert_eq!(
            self.client
                .request(&ClientRequest::ShutdownDaemon)
                .await
                .expect("shutdown response"),
            DaemonResponse::Acknowledged
        );
        self.join().await;
    }

    async fn join(&mut self) {
        self.task
            .take()
            .expect("daemon task")
            .await
            .expect("daemon task")
            .expect("daemon shutdown");
    }
}

impl Drop for DaemonFixture {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn confirmed_override_does_not_stop_sessions_without_a_staged_upgrade() {
    tokio::time::timeout(
        Duration::from_secs(20),
        exercise_missing_staged_upgrade_override(),
    )
    .await
    .expect("upgrade coordination integration timeout");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn changed_active_session_set_requires_a_new_named_confirmation() {
    tokio::time::timeout(Duration::from_secs(20), exercise_changed_session_set())
        .await
        .expect("upgrade coordination integration timeout");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn active_sessions_block_install_and_are_named_for_confirmation() {
    tokio::time::timeout(Duration::from_secs(20), exercise_declined_override())
        .await
        .expect("upgrade coordination integration timeout");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn confirmed_override_stops_and_reaps_sessions_before_handoff() {
    tokio::time::timeout(Duration::from_secs(20), exercise_confirmed_override())
        .await
        .expect("upgrade coordination integration timeout");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_install_broadcasts_bounded_progress_to_multiple_clients() {
    tokio::time::timeout(Duration::from_secs(20), exercise_idle_install_progress())
        .await
        .expect("upgrade coordination integration timeout");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_client_bounds_install_and_concurrent_mutation_is_refused() {
    tokio::time::timeout(
        Duration::from_secs(20),
        exercise_stale_client_and_concurrency(),
    )
    .await
    .expect("upgrade coordination integration timeout");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn detached_handoff_reservation_survives_daemon_restart_until_finalized() {
    tokio::time::timeout(
        Duration::from_secs(20),
        exercise_durable_handoff_reservation(),
    )
    .await
    .expect("upgrade coordination integration timeout");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn data_removal_reservation_blocks_daemon_restart_until_removal_finishes() {
    tokio::time::timeout(Duration::from_secs(20), exercise_data_removal_reservation())
        .await
        .expect("data removal coordination integration timeout");
}

async fn exercise_missing_staged_upgrade_override() {
    let repository = TemporaryRepository::initialize()
        .await
        .expect("temporary repository");
    let paths = prepared_paths(repository.root());
    let launcher = Arc::new(RecordingHandoff::default());
    let fixture = start_daemon(&paths, launcher.clone()).await;
    let client = &fixture.client;
    let session_id = create_active_session(client, repository.path(), "Release work").await;

    let response = client
        .request(&ClientRequest::InstallUpdate {
            confirmed_active_sessions: vec![session_id],
            requesting_process_id: std::process::id(),
        })
        .await
        .expect("install response");
    assert!(matches!(
        response,
        DaemonResponse::Error(ref failure) if failure.code == "upgrade_operation_failed"
    ));

    let snapshot = match client
        .request(&ClientRequest::GetSnapshot)
        .await
        .expect("snapshot response")
    {
        DaemonResponse::Snapshot(snapshot) => snapshot,
        other => panic!("unexpected snapshot response: {other:?}"),
    };
    assert!(snapshot.sessions.iter().any(|session| {
        session.id == session_id
            && matches!(
                session.state,
                SessionState::Starting | SessionState::Running | SessionState::NeedsFeedback
            )
    }));

    assert_eq!(launcher.calls(), 0);
    fixture.shutdown().await;
}

async fn exercise_changed_session_set() {
    let repository = TemporaryRepository::initialize()
        .await
        .expect("temporary repository");
    let paths = prepared_paths(repository.root());
    stage_upgrade(&paths);
    let launcher = Arc::new(RecordingHandoff::default());
    let fixture = start_daemon(&paths, launcher.clone()).await;
    let client = &fixture.client;
    let first_session =
        create_active_session(client, repository.path(), "First release task").await;

    let first_block = client
        .request(&ClientRequest::InstallUpdate {
            confirmed_active_sessions: Vec::new(),
            requesting_process_id: std::process::id(),
        })
        .await
        .expect("first install response");
    let confirmed_active_sessions = match first_block {
        DaemonResponse::UpdateInstall(sylvops_core::upgrade::InstallDisposition::Blocked {
            active_sessions,
        }) => active_sessions
            .into_iter()
            .map(|session| session.id)
            .collect(),
        other => panic!("unexpected first install response: {other:?}"),
    };
    let worktree_id = snapshot(client)
        .await
        .sessions
        .into_iter()
        .find(|session| session.id == first_session)
        .expect("first active session")
        .worktree_id;
    let second_session =
        create_session_on_worktree(client, worktree_id, "Second release task").await;

    let changed = client
        .request(&ClientRequest::InstallUpdate {
            confirmed_active_sessions,
            requesting_process_id: std::process::id(),
        })
        .await
        .expect("changed install response");
    assert!(matches!(
        changed,
        DaemonResponse::UpdateInstall(
            sylvops_core::upgrade::InstallDisposition::Blocked { active_sessions }
        ) if active_sessions.len() == 2
            && active_sessions.iter().any(|session| session.id == first_session)
            && active_sessions.iter().any(|session| session.id == second_session)
    ));
    for session_id in [first_session, second_session] {
        assert_session_state(client, session_id, |state| {
            matches!(
                state,
                SessionState::Starting | SessionState::Running | SessionState::NeedsFeedback
            )
        })
        .await;
    }
    assert_eq!(launcher.calls(), 0);
    fixture.shutdown().await;
}

async fn exercise_declined_override() {
    let repository = TemporaryRepository::initialize()
        .await
        .expect("temporary repository");
    let paths = prepared_paths(repository.root());
    stage_upgrade(&paths);
    let launcher = Arc::new(RecordingHandoff::default());
    let fixture = start_daemon(&paths, launcher.clone()).await;
    let client = &fixture.client;
    let session_id = create_active_session(client, repository.path(), "Release work").await;

    let response = client
        .request(&ClientRequest::InstallUpdate {
            confirmed_active_sessions: Vec::new(),
            requesting_process_id: std::process::id(),
        })
        .await
        .expect("install response");
    assert!(matches!(
        response,
        DaemonResponse::UpdateInstall(
            sylvops_core::upgrade::InstallDisposition::Blocked { active_sessions }
        ) if active_sessions.len() == 1
            && active_sessions[0].id == session_id
            && active_sessions[0].name == "Release work"
    ));
    assert_session_state(client, session_id, |state| {
        matches!(
            state,
            SessionState::Starting | SessionState::Running | SessionState::NeedsFeedback
        )
    })
    .await;
    assert_eq!(launcher.calls(), 0);
    fixture.shutdown().await;
}

async fn exercise_confirmed_override() {
    let repository = TemporaryRepository::initialize()
        .await
        .expect("temporary repository");
    let paths = prepared_paths(repository.root());
    stage_upgrade(&paths);
    let launcher = Arc::new(RecordingHandoff::default());
    let mut fixture = start_daemon(&paths, launcher.clone()).await;
    let session_id =
        create_active_session(&fixture.client, repository.path(), "Release work").await;
    launcher.expect_terminated(session_id);

    let response = fixture
        .client
        .request(&ClientRequest::InstallUpdate {
            confirmed_active_sessions: vec![session_id],
            requesting_process_id: std::process::id(),
        })
        .await
        .expect("install response");
    assert!(matches!(
        response,
        DaemonResponse::UpdateInstall(
            sylvops_core::upgrade::InstallDisposition::Prepared { version }
        ) if version == "99.0.0"
    ));
    fixture.join().await;
    assert_eq!(launcher.calls(), 1);
}

async fn exercise_idle_install_progress() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let paths = prepared_paths(temporary.path());
    stage_upgrade(&paths);
    let launcher = Arc::new(RecordingHandoff::default());
    let mut fixture = start_daemon(&paths, launcher.clone()).await;
    let first_observer = connect_eventually(&paths).await;
    let second_observer = connect_eventually(&paths).await;
    let mut first_events = first_observer.subscribe();
    let mut second_events = second_observer.subscribe();

    let response = fixture
        .client
        .request(&ClientRequest::InstallUpdate {
            confirmed_active_sessions: Vec::new(),
            requesting_process_id: std::process::id(),
        })
        .await
        .expect("install response");
    assert!(matches!(
        response,
        DaemonResponse::UpdateInstall(
            sylvops_core::upgrade::InstallDisposition::Prepared { version }
        ) if version == "99.0.0"
    ));
    for events in [&mut first_events, &mut second_events] {
        assert!(matches!(
            next_upgrade_status(events).await,
            UpgradeStatus::Installing { .. }
        ));
    }

    drop(first_observer);
    drop(second_observer);
    fixture.join().await;
    assert_eq!(launcher.calls(), 1);
}

async fn exercise_stale_client_and_concurrency() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let paths = prepared_paths(temporary.path());
    stage_upgrade(&paths);
    let launcher = Arc::new(RecordingHandoff::default());
    let fixture = start_daemon(&paths, launcher.clone()).await;
    let observer = connect_eventually(&paths).await;
    let contender = connect_eventually(&paths).await;
    let mut events = observer.subscribe();
    let stale_client = connect_raw_client(&paths, u32::MAX - 1).await;

    let requester = fixture.client.clone();
    let installing = tokio::spawn(async move {
        requester
            .request(&ClientRequest::InstallUpdate {
                confirmed_active_sessions: Vec::new(),
                requesting_process_id: std::process::id(),
            })
            .await
    });
    assert!(matches!(
        next_upgrade_status(&mut events).await,
        UpgradeStatus::Installing { .. }
    ));

    let concurrent = contender
        .request(&ClientRequest::PrepareDataRemoval {
            confirmation: DATA_REMOVAL_CONFIRMATION.into(),
        })
        .await
        .expect("concurrent response");
    assert!(matches!(
        concurrent,
        DaemonResponse::Error(ref failure)
            if failure.code == "daemon_operation_failed"
                && failure.message.contains("another application mutation")
    ));

    let install_response = installing
        .await
        .expect("install task")
        .expect("install response");
    assert!(matches!(
        install_response,
        DaemonResponse::Error(ref failure)
            if failure.code == "daemon_operation_failed"
                && failure.message.contains("did not quiesce")
    ));
    assert!(matches!(
        next_upgrade_status(&mut events).await,
        UpgradeStatus::Staged { .. }
    ));

    drop(stale_client);
    drop(observer);
    assert_eq!(launcher.calls(), 0);
    drop(contender);
    fixture.shutdown().await;
}

async fn exercise_durable_handoff_reservation() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let paths = prepared_paths(temporary.path());
    stage_upgrade(&paths);
    let handoff_path = paths.data_directory.join("upgrades/handoff.json");
    std::fs::write(&handoff_path, b"pending handoff").expect("handoff reservation");
    let launcher = Arc::new(RecordingHandoff::default());
    let fixture = start_daemon(&paths, launcher).await;

    let blocked = fixture
        .client
        .request(&ClientRequest::InstallUpdate {
            confirmed_active_sessions: Vec::new(),
            requesting_process_id: std::process::id(),
        })
        .await
        .expect("blocked mutation response");
    assert!(matches!(
        blocked,
        DaemonResponse::Error(ref failure)
            if failure.message.contains("another application mutation")
    ));

    assert_eq!(
        fixture
            .client
            .request(&ClientRequest::FinalizeUpdate {
                version: "99.0.0".into(),
                outcome: NativeUpgradeOutcome::RolledBack,
            })
            .await
            .expect("finalize response"),
        DaemonResponse::Acknowledged
    );
    assert!(!handoff_path.exists());

    let released = fixture
        .client
        .request(&ClientRequest::InstallUpdate {
            confirmed_active_sessions: Vec::new(),
            requesting_process_id: std::process::id(),
        })
        .await
        .expect("released mutation response");
    assert!(matches!(
        released,
        DaemonResponse::Error(ref failure) if failure.code == "upgrade_operation_failed"
    ));
    fixture.shutdown().await;
}

async fn exercise_data_removal_reservation() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let paths = prepared_paths(temporary.path());
    let launcher = Arc::new(RecordingHandoff::default());
    let mut fixture = start_daemon(&paths, launcher.clone()).await;

    assert_eq!(
        fixture
            .client
            .request(&ClientRequest::PrepareDataRemoval {
                confirmation: DATA_REMOVAL_CONFIRMATION.into(),
            })
            .await
            .expect("data removal response"),
        DaemonResponse::DataRemovalPrepared
    );
    fixture.join().await;
    assert!(reservation_pending(&paths).expect("reservation state"));

    let restart = daemon::run_with_handoff_launcher(paths.clone(), launcher).await;
    assert!(matches!(
        restart,
        Err(DaemonError::Lifecycle(ref message)) if message.contains("removal is still in progress")
    ));

    let protected_paths = reservation(&paths)
        .expect("reservation contents")
        .expect("durable reservation");
    let plan = DataRemovalPlan::prepare(&paths, DATA_REMOVAL_CONFIRMATION, &protected_paths)
        .expect("recovered data removal plan");
    plan.execute().await.expect("execute data removal");
    assert!(!paths.data_directory.exists());
    assert!(!paths.config_directory.exists());
    assert!(!paths.runtime_directory.exists());
}

async fn create_active_session(
    client: &DaemonClient,
    repository: &Path,
    name: &str,
) -> sylvops_core::ids::SessionId {
    let workspace = match client
        .request(&ClientRequest::AddWorkspace {
            name: "upgrade coordination".into(),
        })
        .await
        .expect("workspace response")
    {
        DaemonResponse::WorkspaceAdded { workspace, .. } => workspace,
        other => panic!("unexpected workspace response: {other:?}"),
    };
    let worktree = match client
        .request(&ClientRequest::AddProject {
            workspace_id: workspace.id,
            repository_path: repository.to_string_lossy().into_owned(),
        })
        .await
        .expect("project response")
    {
        DaemonResponse::ProjectAdded { root_worktree, .. } => root_worktree,
        other => panic!("unexpected project response: {other:?}"),
    };
    create_session_on_worktree(client, worktree.id, name).await
}

async fn create_session_on_worktree(
    client: &DaemonClient,
    worktree_id: sylvops_core::ids::WorktreeId,
    name: &str,
) -> sylvops_core::ids::SessionId {
    match client
        .request(&ClientRequest::CreateSession {
            worktree_id,
            provider: ProviderKind::Shell,
            display_name: Some(name.into()),
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
        other => panic!("unexpected session response: {other:?}"),
    }
}

async fn assert_session_state(
    client: &DaemonClient,
    session_id: sylvops_core::ids::SessionId,
    predicate: impl Fn(SessionState) -> bool,
) {
    let snapshot = snapshot(client).await;
    assert!(
        snapshot
            .sessions
            .iter()
            .any(|session| { session.id == session_id && predicate(session.state) })
    );
}

async fn snapshot(client: &DaemonClient) -> sylvops_core::domain::DaemonSnapshot {
    match client
        .request(&ClientRequest::GetSnapshot)
        .await
        .expect("snapshot response")
    {
        DaemonResponse::Snapshot(snapshot) => snapshot,
        other => panic!("unexpected snapshot response: {other:?}"),
    }
}

fn prepared_paths(root: &Path) -> RuntimePaths {
    let paths = RuntimePaths::discover(Some(&root.join("state"))).expect("runtime paths");
    paths.prepare().expect("prepare runtime paths");
    paths
}

fn stage_upgrade(paths: &RuntimePaths) {
    let staging = paths.data_directory.join("upgrades");
    std::fs::create_dir(&staging).expect("upgrade staging directory");
    let payload = b"verified upgrade payload";
    std::fs::write(staging.join("payload.staged"), payload).expect("staged payload");
    let release = staged_release();
    let signing_key = SigningKey::from_bytes(&[
        0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec, 0x2c,
        0xc4, 0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03, 0x1c, 0xae,
        0x7f, 0x60,
    ]);
    let signature = signing_key
        .sign(&release.signed_bytes().expect("signed release bytes"))
        .to_bytes();
    std::fs::write(
        staging.join("release.json"),
        serde_json::to_vec(&SignedReleaseMetadata::new(release, signature))
            .expect("signed release metadata"),
    )
    .expect("release metadata");
}

fn staged_release() -> ReleaseMetadata {
    let payload = b"verified upgrade payload";
    let published_at_unix_seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time")
        .as_secs()
        .try_into()
        .expect("publication timestamp");
    ReleaseMetadata {
        schema_version: 1,
        minimum_source_version: env!("CARGO_PKG_VERSION").into(),
        target_version: "99.0.0".into(),
        target: current_release_target().expect("current release target"),
        installer_url: "https://github.com/devemit/sylvops/releases/download/v99.0.0/sylvops-test"
            .into(),
        byte_length: payload.len().try_into().expect("payload length"),
        sha256: format!("{:x}", Sha256::digest(payload)),
        release_notes_url: "https://github.com/devemit/sylvops/releases/tag/v99.0.0".into(),
        release_notes: "Upgrade coordination integration test.".into(),
        published_at_unix_seconds,
    }
}

async fn start_daemon(paths: &RuntimePaths, launcher: Arc<RecordingHandoff>) -> DaemonFixture {
    let daemon_paths = paths.clone();
    let mut task = PendingDaemonTask(Some(tokio::spawn(async move {
        daemon::run_with_handoff_launcher(daemon_paths, launcher).await
    })));
    let client = connect_eventually(paths).await;
    DaemonFixture {
        client,
        task: task.0.take(),
    }
}

async fn next_upgrade_status(
    events: &mut tokio::sync::broadcast::Receiver<DaemonEvent>,
) -> UpgradeStatus {
    tokio::time::timeout(Duration::from_secs(12), async {
        loop {
            if let DaemonEvent::UpgradeProgress { status } =
                events.recv().await.expect("daemon event")
            {
                return status;
            }
        }
    })
    .await
    .expect("upgrade progress event")
}

async fn connect_raw_client(paths: &RuntimePaths, process_id: u32) -> BoxStream {
    let token =
        AuthenticationToken::read(&paths.authentication_token).expect("authentication token");
    let mut stream = ipc::connect(&paths.endpoint)
        .await
        .expect("raw local connection");
    let hello = ClientRequest::Hello(HelloRequest {
        client_name: "stale-upgrade-client".into(),
        client_process_id: process_id,
        client_version: env!("CARGO_PKG_VERSION").into(),
        protocol_major: PROTOCOL_MAJOR,
        protocol_minor: PROTOCOL_MINOR,
        authentication_token: token.expose().into(),
    });
    let request =
        Frame::message(MessageClass::Request, CONTROL_OPCODE, &hello).expect("hello frame");
    let request_id = request.message_id;
    write_frame(&mut stream, &request)
        .await
        .expect("write hello");
    let response = read_frame(&mut stream).await.expect("read hello");
    assert_eq!(response.correlation_id, Some(request_id));
    assert!(matches!(
        response
            .payload_as::<DaemonResponse>()
            .expect("typed hello response"),
        DaemonResponse::Welcome(_)
    ));
    stream
}

async fn connect_eventually(paths: &RuntimePaths) -> DaemonClient {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match DaemonClient::connect(paths, "upgrade-coordination-integration").await {
            Ok(client) => return client,
            Err(error) if tokio::time::Instant::now() < deadline => {
                let _ = error;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(error) => panic!("daemon did not become ready: {error}"),
        }
    }
}
