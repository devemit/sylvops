use sylvops_core::protocol::{PhaseZeroRequest, PhaseZeroResponse};
use sylvops_daemon::ipc::{
    LocalEndpoint, LocalListener, connect, request, serve_phase_zero_connection,
};

#[tokio::test]
async fn local_transport_serves_multiple_requests() {
    #[cfg(unix)]
    let parent_permissions = unix_parent_permissions();
    let endpoint = unique_endpoint();
    let mut listener = LocalListener::bind(&endpoint).expect("bind local endpoint");
    #[cfg(unix)]
    assert_eq!(unix_parent_permissions(), parent_permissions);
    let server = tokio::spawn(async move {
        let stream = listener.accept().await.expect("accept local client");
        serve_phase_zero_connection(stream).await
    });

    let mut stream = connect_with_retry(&endpoint).await;
    let welcome = request(
        &mut stream,
        &PhaseZeroRequest::Hello {
            client_name: "transport-test".into(),
        },
    )
    .await
    .expect("hello response");
    assert!(matches!(welcome, PhaseZeroResponse::Welcome { .. }));

    let health = request(&mut stream, &PhaseZeroRequest::Health)
        .await
        .expect("health response");
    assert!(matches!(health, PhaseZeroResponse::Healthy { .. }));
    drop(stream);
    server.await.expect("server task").expect("server result");
}

async fn connect_with_retry(endpoint: &LocalEndpoint) -> sylvops_daemon::ipc::BoxStream {
    let mut last_error = None;
    for _ in 0..20 {
        match connect(endpoint).await {
            Ok(stream) => return stream,
            Err(error) => {
                last_error = Some(error);
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        }
    }
    panic!("failed to connect: {last_error:?}");
}

#[cfg(unix)]
fn unique_endpoint() -> LocalEndpoint {
    LocalEndpoint::unix(std::env::temp_dir().join(format!("sylvops-{}.sock", uuid::Uuid::now_v7())))
}

#[cfg(unix)]
fn unix_parent_permissions() -> u32 {
    use std::os::unix::fs::PermissionsExt;

    std::fs::metadata(std::env::temp_dir())
        .expect("temporary directory metadata")
        .permissions()
        .mode()
}

#[cfg(windows)]
fn unique_endpoint() -> LocalEndpoint {
    LocalEndpoint::windows_pipe(format!(r"\\.\pipe\sylvops-{}", uuid::Uuid::now_v7()))
}
