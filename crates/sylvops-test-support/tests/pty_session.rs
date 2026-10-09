#![allow(unsafe_code)]

use std::{
    ffi::OsString,
    fs,
    path::Path,
    process::Command,
    time::{Duration, Instant},
};

use sylvops_core::{
    domain::ProviderKind,
    protocol::{ClientRequest, DaemonEvent, DaemonResponse},
};
use sylvops_daemon::{
    DaemonError,
    client::DaemonClient,
    daemon,
    runtime::RuntimePaths,
    session::{SessionHandle, SessionSpec},
};

const TEST_TIMEOUT: Duration = Duration::from_secs(15);
const TAGGED_ECHO_BUFFER_BYTES: usize = 4 * 1024;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_spawns_under_process_tree_control() {
    let mut session = match SessionHandle::spawn(&spec(["interactive"], 64 * 1024)) {
        Ok(session) => session,
        Err(DaemonError::ProcessTree(_)) => {
            println!("::error title=PTY spawn category::process-tree containment failed");
            panic!("process-tree containment failed");
        }
        Err(DaemonError::Pty(_)) => {
            println!("::error title=PTY spawn category::PTY creation or launch failed");
            panic!("PTY creation or launch failed");
        }
        Err(_) => {
            println!("::error title=PTY spawn category::unexpected session setup failure");
            panic!("unexpected session setup failure");
        }
    };
    wait_for_output(&session, "FAKE_AGENT_READY").await;
    session.input(input_line("exit")).await.expect("exit input");
    let exit = tokio::time::timeout(TEST_TIMEOUT, session.wait())
        .await
        .expect("contained session exit timeout")
        .expect("contained session wait");
    assert_eq!(exit.exit_code, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_starts_in_the_selected_worktree() {
    let directory = tempfile::Builder::new()
        .prefix("sylvops cwd ")
        .tempdir()
        .expect("temporary worktree");
    let expected = directory.path().canonicalize().expect("canonical worktree");
    let mut launch = spec(["cwd"], 64 * 1024);
    launch.cwd = expected.clone();
    let mut session = SessionHandle::spawn(&launch).expect("spawn cwd reporter");
    wait_for_output(&session, "CWD=").await;
    session.input(input_line("exit")).await.expect("exit input");
    let exit = tokio::time::timeout(TEST_TIMEOUT, session.wait())
        .await
        .expect("cwd reporter timeout")
        .expect("cwd reporter wait");
    assert_eq!(exit.exit_code, 0);

    let replay = session.replay_after(0).await.expect("cwd replay");
    let output: Vec<u8> = replay
        .chunks
        .iter()
        .flat_map(|chunk| chunk.bytes.iter().copied())
        .collect();
    let output = String::from_utf8_lossy(&output);
    assert!(
        output.contains(&expected.to_string_lossy().to_string()),
        "child cwd did not match selected worktree: {output:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_accepts_input_and_replays_after_detach() {
    let mut session =
        SessionHandle::spawn(&spec(["interactive"], 64 * 1024)).expect("spawn fake agent");

    wait_for_output(&session, "FAKE_AGENT_READY").await;
    let observer = session.subscribe();
    drop(observer); // Detaching an observer must not stop the process.
    session
        .input(input_line("hello"))
        .await
        .expect("send input");
    wait_for_output(&session, "ECHO:hello").await;

    session.resize(100, 40).await.expect("resize PTY");
    session.input(input_line("exit")).await.expect("exit input");
    let exit = tokio::time::timeout(TEST_TIMEOUT, session.wait())
        .await
        .expect("session exit timeout")
        .expect("session wait");
    assert_eq!(exit.exit_code, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fake_provider_preserves_two_hundred_tagged_inputs_and_reports_echo_latency() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let repository = temporary.path().join("repository");
    initialize_repository(&repository);
    let bin_directory = temporary.path().join("bin");
    fs::create_dir(&bin_directory).expect("fake provider bin directory");
    let fake_codex = install_fake_codex(&bin_directory);
    let _codex_cli_path = EnvironmentGuard::set("CODEX_CLI_PATH", fake_codex);
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
            name: "terminal-latency".into(),
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
    let mut output = client.subscribe();
    let session_id = match client
        .request(&ClientRequest::CreateSession {
            worktree_id,
            provider: ProviderKind::Codex,
            display_name: Some("IPC latency".into()),
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
    let mut observed = String::new();
    let mut latencies = Vec::with_capacity(200);
    let mut echoes = Vec::with_capacity(200);

    tokio::time::timeout(TEST_TIMEOUT, async {
        for index in 0..200 {
            let tag = format!("SYLVOPS_INPUT_{index:03}");
            let expected = format!("FAKE_CODEX_ECHO={tag}");
            let started = Instant::now();
            assert!(matches!(
                client
                    .request(&ClientRequest::SessionInput {
                        session_id,
                        bytes: input_line(&tag),
                    })
                    .await
                    .expect("tagged IPC input"),
                DaemonResponse::Acknowledged
            ));
            loop {
                let event = output.recv().await.expect("fake-provider output");
                let DaemonEvent::SessionOutput {
                    session_id: output_session_id,
                    bytes,
                    ..
                } = event
                else {
                    continue;
                };
                if output_session_id != session_id {
                    continue;
                }
                observed.push_str(&String::from_utf8_lossy(&bytes));
                if observed.len() > TAGGED_ECHO_BUFFER_BYTES {
                    let mut keep_from = observed.len() - TAGGED_ECHO_BUFFER_BYTES;
                    while !observed.is_char_boundary(keep_from) {
                        keep_from += 1;
                    }
                    observed.drain(..keep_from);
                }
                if observed.contains(&expected) {
                    latencies.push(started.elapsed());
                    echoes.push(expected);
                    observed.clear();
                    break;
                }
            }
        }
    })
    .await
    .expect("tagged echo timeout");

    assert_eq!(echoes.len(), 200);
    for (index, echo) in echoes.iter().enumerate() {
        assert_eq!(echo, &format!("FAKE_CODEX_ECHO=SYLVOPS_INPUT_{index:03}"));
    }
    latencies.sort_unstable();
    let p95 = latencies[latencies.len() * 95 / 100];
    let maximum = *latencies.last().expect("echo latencies");
    println!(
        "fake_provider_ipc_echo samples={} p95_us={} max_us={}",
        latencies.len(),
        p95.as_micros(),
        maximum.as_micros()
    );

    assert!(matches!(
        client
            .request(&ClientRequest::SessionInput {
                session_id,
                bytes: input_line("finish"),
            })
            .await
            .expect("finish input"),
        DaemonResponse::Acknowledged
    ));
    client
        .request(&ClientRequest::ShutdownDaemon)
        .await
        .expect("shutdown response");
    daemon_task
        .await
        .expect("daemon task")
        .expect("daemon exit");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopping_session_terminates_descendant_heartbeat() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let heartbeat = directory.path().join("heartbeat.txt");
    let mut session = SessionHandle::spawn(&spec(
        [
            OsString::from("spawn-child"),
            heartbeat.as_os_str().to_owned(),
        ],
        64 * 1024,
    ))
    .expect("spawn fake process tree");

    wait_for_output(&session, "CHILD_PID=").await;
    wait_for_file(&heartbeat).await;
    assert!(
        !session
            .process_tree_is_empty()
            .await
            .expect("inspect live process tree"),
        "live descendant must keep the process tree non-empty"
    );
    session.stop().await.expect("terminate process tree");
    tokio::time::timeout(TEST_TIMEOUT, session.wait())
        .await
        .expect("process tree exit timeout")
        .expect("session wait");

    tokio::time::sleep(Duration::from_millis(200)).await;
    let stopped_value = fs::read_to_string(&heartbeat).expect("read heartbeat after stop");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let later_value = fs::read_to_string(&heartbeat).expect("read heartbeat later");
    assert_eq!(stopped_value, later_value, "descendant remained alive");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_after_root_exit_reaps_the_remaining_process_group() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let heartbeat = directory.path().join("remaining-heartbeat.txt");
    let mut session = SessionHandle::spawn(&spec(
        [
            OsString::from("leave-child"),
            heartbeat.as_os_str().to_owned(),
        ],
        64 * 1024,
    ))
    .expect("spawn fake process tree");

    tokio::time::timeout(TEST_TIMEOUT, session.wait())
        .await
        .expect("root exit timeout")
        .expect("root wait");
    wait_for_file(&heartbeat).await;
    assert!(!session.process_tree_is_empty().await.unwrap());
    session
        .stop()
        .await
        .expect("terminate remaining process tree");
    assert!(session.process_tree_is_empty().await.unwrap());

    let stopped_value = fs::read_to_string(&heartbeat).expect("read heartbeat after stop");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let later_value = fs::read_to_string(&heartbeat).expect("read heartbeat later");
    assert_eq!(stopped_value, later_value, "descendant remained alive");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn large_output_is_bounded_and_reports_eviction() {
    let capacity = 16 * 1024;
    let mut session =
        SessionHandle::spawn(&spec(["burst", "262144"], capacity)).expect("spawn burst agent");
    let exit = tokio::time::timeout(TEST_TIMEOUT, session.wait())
        .await
        .expect("burst exit timeout")
        .expect("burst session wait");
    assert_eq!(exit.exit_code, 0);

    let replay = session
        .replay_after(0)
        .await
        .expect("request bounded replay");
    let retained: usize = replay.chunks.iter().map(|chunk| chunk.bytes.len()).sum();
    assert!(replay.output_gap, "eviction was not reported");
    assert!(retained <= capacity, "scrollback exceeded its byte cap");
    let attachment = session
        .attach(uuid::Uuid::now_v7(), 0, 80, 24)
        .await
        .expect("attach after eviction");
    assert!(attachment.replay.output_gap);
    assert_eq!(attachment.replay.chunks, Vec::new());
    assert!(attachment.replay.terminal_snapshot.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attachment_lease_rejects_observer_input_and_requires_reattach() {
    let mut session =
        SessionHandle::spawn(&spec(["interactive"], 64 * 1024)).expect("spawn fake agent");
    wait_for_output(&session, "FAKE_AGENT_READY").await;
    let controller = uuid::Uuid::now_v7();
    let observer = uuid::Uuid::now_v7();
    let first = session.attach(controller, 0, 80, 24).await.unwrap();
    let second = session.attach(observer, 0, 80, 24).await.unwrap();
    assert_eq!(first.role, sylvops_core::domain::AttachmentRole::Controller);
    assert_eq!(second.role, sylvops_core::domain::AttachmentRole::Observer);
    assert!(
        session
            .input_from(
                controller,
                vec![0; sylvops_core::protocol::MAX_PTY_CHUNK_SIZE + 1],
            )
            .await
            .is_err()
    );
    assert!(
        session
            .input_from(observer, b"forbidden\n".to_vec())
            .await
            .is_err()
    );
    session.detach(controller).await.unwrap();
    let promoted = session.attach(observer, 0, 80, 24).await.unwrap();
    assert_eq!(
        promoted.role,
        sylvops_core::domain::AttachmentRole::Controller
    );
    session
        .input_from(observer, input_line("exit"))
        .await
        .unwrap();
    let exit = tokio::time::timeout(TEST_TIMEOUT, session.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(exit.exit_code, 0);
}

#[cfg(windows)]
fn input_line(value: &str) -> Vec<u8> {
    format!("{value}\r").into_bytes()
}

#[cfg(unix)]
fn input_line(value: &str) -> Vec<u8> {
    format!("{value}\n").into_bytes()
}

fn spec(
    arguments: impl IntoIterator<Item = impl Into<OsString>>,
    scrollback_bytes: usize,
) -> SessionSpec {
    let program = std::env::var_os("SYLVOPS_TEST_FAKE_AGENT").map_or_else(
        || Path::new(env!("CARGO_BIN_EXE_fake-agent")).to_path_buf(),
        std::path::PathBuf::from,
    );
    SessionSpec {
        program,
        arguments: arguments.into_iter().map(Into::into).collect(),
        cwd: std::env::current_dir().expect("current directory"),
        columns: 80,
        rows: 24,
        scrollback_bytes,
    }
}

async fn wait_for_output(session: &SessionHandle, expected: &str) {
    tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            let replay = session.replay_after(0).await.expect("request replay");
            let bytes: Vec<u8> = replay
                .chunks
                .iter()
                .flat_map(|chunk| chunk.bytes.iter().copied())
                .collect();
            if String::from_utf8_lossy(&bytes).contains(expected) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("expected PTY output did not arrive");
}

async fn wait_for_file(path: &Path) {
    tokio::time::timeout(TEST_TIMEOUT, async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("heartbeat file was not created");
}

async fn connect_eventually(paths: &RuntimePaths) -> DaemonClient {
    tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            if let Ok(client) = DaemonClient::connect(paths, "terminal-latency-smoke").await {
                return client;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("daemon connection timeout")
}

fn install_fake_codex(directory: &Path) -> std::path::PathBuf {
    let target = directory.join(if cfg!(windows) { "codex.exe" } else { "codex" });
    let source = std::env::var_os("SYLVOPS_TEST_FAKE_CODEX").map_or_else(
        || std::path::PathBuf::from(env!("CARGO_BIN_EXE_fake-codex")),
        std::path::PathBuf::from,
    );
    fs::copy(source, &target).expect("copy fake Codex");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&target).expect("fake metadata").permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&target, permissions).expect("fake executable permissions");
    }
    target
}

fn initialize_repository(path: &Path) {
    fs::create_dir(path).expect("repository directory");
    run_git(path, &["init", "--initial-branch=main"]);
    run_git(path, &["config", "user.name", "SylvOps Test"]);
    run_git(path, &["config", "user.email", "sylvops@example.invalid"]);
    fs::write(path.join("README.md"), "fixture\n").expect("fixture file");
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
        // SAFETY: this integration-test process owns and restores its environment changes.
        unsafe { std::env::set_var(name, value.into()) };
        Self { name, previous }
    }
}

impl Drop for EnvironmentGuard {
    fn drop(&mut self) {
        // SAFETY: this integration-test process owns and restores its environment changes.
        unsafe {
            if let Some(previous) = self.previous.take() {
                std::env::set_var(self.name, previous);
            } else {
                std::env::remove_var(self.name);
            }
        }
    }
}
