use std::time::Duration;

use sylvops_core::protocol::{
    ClientRequest, DaemonResponse, Frame, HelloRequest, MessageClass, PROTOCOL_MAJOR,
    PROTOCOL_MINOR, read_frame, write_frame,
};
use sylvops_daemon::{
    client::DaemonClient,
    daemon::{self, CONTROL_OPCODE},
    ipc,
    runtime::RuntimePaths,
};

async fn connect_eventually(paths: &RuntimePaths) -> DaemonClient {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match DaemonClient::connect(paths, "phase-one-integration-test").await {
            Ok(client) => return client,
            Err(error) if tokio::time::Instant::now() < deadline => {
                let _ = error;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(error) => panic!("daemon did not become ready: {error}"),
        }
    }
}

#[tokio::test]
async fn daemon_authenticates_concurrent_clients_and_shuts_down_cleanly() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let paths = RuntimePaths::discover(Some(temporary.path())).expect("runtime paths");
    let daemon_paths = paths.clone();
    let daemon = tokio::spawn(async move { daemon::run(daemon_paths).await });

    let first = connect_eventually(&paths).await;

    let mut unauthenticated = ipc::connect(&paths.endpoint)
        .await
        .expect("raw local connection");
    let invalid_hello = ClientRequest::Hello(HelloRequest {
        client_name: "untrusted-test-client".into(),
        client_version: "0".into(),
        protocol_major: PROTOCOL_MAJOR,
        protocol_minor: PROTOCOL_MINOR,
        authentication_token: "0".repeat(64),
    });
    let request =
        Frame::message(MessageClass::Request, CONTROL_OPCODE, &invalid_hello).expect("hello frame");
    let request_id = request.message_id;
    write_frame(&mut unauthenticated, &request)
        .await
        .expect("write unauthenticated hello");
    let rejected = read_frame(&mut unauthenticated)
        .await
        .expect("read authentication rejection");
    assert_eq!(rejected.correlation_id, Some(request_id));
    assert!(matches!(
        rejected.payload_as::<DaemonResponse>().expect("typed rejection"),
        DaemonResponse::Error(error) if error.code == "authentication_failed"
    ));

    let second = connect_eventually(&paths).await;
    let health = second
        .request(&ClientRequest::Health)
        .await
        .expect("health response");
    assert!(matches!(
        health,
        DaemonResponse::Health(health)
            if health.database_ready && health.connected_clients >= 2
    ));

    let snapshot = first
        .request(&ClientRequest::GetSnapshot)
        .await
        .expect("snapshot response");
    assert!(matches!(
        snapshot,
        DaemonResponse::Snapshot(snapshot)
            if snapshot.workspaces.is_empty()
                && snapshot.projects.is_empty()
                && snapshot.worktrees.is_empty()
                && snapshot.sessions.is_empty()
    ));

    assert!(matches!(
        second
            .request(&ClientRequest::ShutdownDaemon)
            .await
            .expect("shutdown response"),
        DaemonResponse::Acknowledged
    ));
    tokio::time::timeout(Duration::from_secs(5), daemon)
        .await
        .expect("daemon shutdown timeout")
        .expect("daemon task panicked")
        .expect("daemon shutdown failed");
    assert!(!paths.authentication_token.exists());
}
