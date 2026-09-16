use std::{ffi::OsString, fs, path::Path, time::Duration};

use sylvops_daemon::{
    DaemonError,
    session::{SessionHandle, SessionSpec},
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_spawns_under_process_tree_control() {
    let mut session = match SessionHandle::spawn(&spec(["exit", "0"], 64 * 1024)) {
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
    let exit = tokio::time::timeout(Duration::from_secs(5), session.wait())
        .await
        .expect("contained session exit timeout")
        .expect("contained session wait");
    assert_eq!(exit.exit_code, 0);
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
    let exit = tokio::time::timeout(Duration::from_secs(5), session.wait())
        .await
        .expect("session exit timeout")
        .expect("session wait");
    assert_eq!(exit.exit_code, 0);
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
    session.stop().await.expect("terminate process tree");
    tokio::time::timeout(Duration::from_secs(5), session.wait())
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
async fn large_output_is_bounded_and_reports_eviction() {
    let capacity = 16 * 1024;
    let mut session =
        SessionHandle::spawn(&spec(["burst", "262144"], capacity)).expect("spawn burst agent");
    let exit = tokio::time::timeout(Duration::from_secs(5), session.wait())
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
    assert!(attachment.replay.chunks.is_empty());
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
    let exit = tokio::time::timeout(Duration::from_secs(5), session.wait())
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
    SessionSpec {
        program: Path::new(env!("CARGO_BIN_EXE_fake-agent")).to_path_buf(),
        arguments: arguments.into_iter().map(Into::into).collect(),
        cwd: std::env::current_dir().expect("current directory"),
        columns: 80,
        rows: 24,
        scrollback_bytes,
    }
}

async fn wait_for_output(session: &SessionHandle, expected: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
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
    tokio::time::timeout(Duration::from_secs(5), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("heartbeat file was not created");
}
