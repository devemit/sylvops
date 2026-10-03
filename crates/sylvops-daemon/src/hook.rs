//! Authenticated loopback hook receiver and the tiny provider hook relay.

use std::{
    collections::HashMap,
    io::{Read, Write},
    net::{SocketAddr, TcpStream as StdTcpStream},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use crate::{DaemonError, Result, runtime::AuthenticationToken};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use sylvops_core::{
    domain::ProviderKind,
    ids::{SessionId, WorktreeId},
    provider::{
        MAX_PROVIDER_LIFECYCLE_PAYLOAD_SIZE, ProviderLifecycleEndpoint, ProviderLifecyclePayload,
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, watch},
    task::JoinHandle,
    time::timeout,
};

const HEADER_LIMIT: usize = 16 * 1024;
const DEFAULT_BODY_LIMIT: usize = 64 * 1024;
const PER_SESSION_CONNECTION_LIMIT: usize = 8;
const PER_SESSION_QUEUE_LIMIT: usize = 32;

#[derive(Debug)]
struct RateWindow {
    started_at: Instant,
    accepted: u32,
}

#[derive(Debug)]
struct RateLimiter {
    limit: u32,
    window: Mutex<RateWindow>,
}

impl RateLimiter {
    fn new(limit: u32) -> Self {
        Self {
            limit: limit.clamp(1, 60_000),
            window: Mutex::new(RateWindow {
                started_at: Instant::now(),
                accepted: 0,
            }),
        }
    }

    fn allow(&self) -> bool {
        let mut window = self
            .window
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if window.started_at.elapsed() >= Duration::from_secs(60) {
            window.started_at = Instant::now();
            window.accepted = 0;
        }
        if window.accepted >= self.limit {
            return false;
        }
        window.accepted += 1;
        true
    }
}

#[derive(Debug)]
pub struct HookDelivery {
    pub provider: ProviderKind,
    pub session_id: SessionId,
    pub worktree_id: WorktreeId,
    pub fingerprint: String,
    pub payload: ProviderLifecyclePayload,
    _session_queue_permit: OwnedSemaphorePermit,
}

#[derive(Clone)]
struct SessionCredential {
    provider: ProviderKind,
    worktree_id: WorktreeId,
    token: String,
    limiter: Arc<RateLimiter>,
    connection_slots: Arc<Semaphore>,
    queue_slots: Arc<Semaphore>,
    body_limit: usize,
}

#[derive(Clone)]
pub struct HookCredentials {
    endpoint_url: String,
    requests_per_minute: u32,
    body_limit: usize,
    sessions: Arc<Mutex<HashMap<SessionId, SessionCredential>>>,
}

impl std::fmt::Debug for HookCredentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HookCredentials")
            .field("endpoint_url", &self.endpoint_url)
            .field("requests_per_minute", &self.requests_per_minute)
            .field("body_limit", &self.body_limit)
            .field(
                "session_count",
                &self
                    .sessions
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .len(),
            )
            .finish()
    }
}

impl HookCredentials {
    /// Issues a fresh memory-only credential bound to one Provider, Session, and Worktree.
    ///
    /// # Errors
    ///
    /// Returns an error if the bounded loopback endpoint cannot be represented safely.
    pub fn register(
        &self,
        provider: ProviderKind,
        session_id: SessionId,
        worktree_id: WorktreeId,
    ) -> Result<ProviderLifecycleEndpoint> {
        let token = AuthenticationToken::generate();
        let endpoint = ProviderLifecycleEndpoint::new(
            provider,
            session_id,
            worktree_id,
            self.endpoint_url.clone(),
            token.expose(),
        )
        .map_err(|error| DaemonError::Provider(error.to_string()))?;
        let credential = SessionCredential {
            provider,
            worktree_id,
            token: token.expose().to_owned(),
            limiter: Arc::new(RateLimiter::new(self.requests_per_minute)),
            connection_slots: Arc::new(Semaphore::new(PER_SESSION_CONNECTION_LIMIT)),
            queue_slots: Arc::new(Semaphore::new(PER_SESSION_QUEUE_LIMIT)),
            body_limit: self.body_limit,
        };
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(session_id, credential);
        Ok(endpoint)
    }

    /// Invalidates one Session credential immediately.
    pub fn invalidate(&self, session_id: SessionId) {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&session_id);
    }

    /// Invalidates every credential during daemon cleanup.
    pub fn invalidate_all(&self) {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    fn authenticate(
        &self,
        supplied_token: Option<&str>,
        session_id: SessionId,
        worktree_id: WorktreeId,
    ) -> Option<SessionCredential> {
        let sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let credential = sessions.get(&session_id)?;
        let supplied_token = supplied_token?;
        if credential.worktree_id != worktree_id
            || supplied_token.len() != credential.token.len()
            || !bool::from(supplied_token.as_bytes().ct_eq(credential.token.as_bytes()))
        {
            return None;
        }
        Some(credential.clone())
    }
}

#[derive(Debug)]
pub struct HookReceiver {
    relay_executable: PathBuf,
    credentials: HookCredentials,
    deliveries: Option<mpsc::Receiver<HookDelivery>>,
    task: JoinHandle<()>,
    shutdown: watch::Sender<bool>,
}

impl HookReceiver {
    /// Binds the authenticated, bounded receiver to an ephemeral IPv4-loopback port.
    ///
    /// # Errors
    ///
    /// Returns an error when the loopback listener or endpoint metadata cannot be created.
    pub async fn bind(
        relay_executable: PathBuf,
        body_limit: usize,
        requests_per_minute: u32,
    ) -> Result<Self> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|error| {
                DaemonError::Lifecycle(format!("cannot bind hook receiver: {error}"))
            })?;
        let address = listener.local_addr().map_err(|error| {
            DaemonError::Lifecycle(format!("cannot inspect hook receiver: {error}"))
        })?;
        let requests_per_minute = requests_per_minute.clamp(1, 60_000);
        let per_session_requests_per_minute = requests_per_minute.div_ceil(4);
        let body_limit = body_limit.clamp(1024, MAX_PROVIDER_LIFECYCLE_PAYLOAD_SIZE);
        let credentials = HookCredentials {
            endpoint_url: format!("http://{address}/v1/events"),
            requests_per_minute: per_session_requests_per_minute,
            body_limit,
            sessions: Arc::new(Mutex::new(HashMap::new())),
        };
        let (deliveries_tx, deliveries) = mpsc::channel(256);
        let (shutdown, mut shutdown_rx) = watch::channel(false);
        let limiter = Arc::new(RateLimiter::new(requests_per_minute));
        let connection_slots = Arc::new(Semaphore::new(64));
        let receiver_credentials = credentials.clone();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((stream, peer)) = accepted else { return; };
                        if !peer.ip().is_loopback() { continue; }
                        let Ok(permit) = connection_slots.clone().try_acquire_owned() else {
                            continue;
                        };
                        let deliveries = deliveries_tx.clone();
                        let credentials = receiver_credentials.clone();
                        let limiter = limiter.clone();
                        tokio::spawn(async move {
                            let _permit = permit;
                            let _ = timeout(
                                Duration::from_secs(2),
                                handle_connection(stream, peer, credentials, body_limit, limiter, deliveries),
                            )
                            .await;
                        });
                    }
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow() { return; }
                    }
                }
            }
        });
        Ok(Self {
            relay_executable,
            credentials,
            deliveries: Some(deliveries),
            task,
            shutdown,
        })
    }

    /// Transfers exclusive ownership of the normalized event stream.
    ///
    /// # Panics
    ///
    /// Panics when called more than once for the same receiver.
    pub fn take_deliveries(&mut self) -> mpsc::Receiver<HookDelivery> {
        self.deliveries
            .take()
            .expect("hook deliveries may only be taken once")
    }

    /// Issues a fresh memory-only credential for one Provider Session.
    ///
    /// # Errors
    ///
    /// Returns an error if the bounded loopback endpoint cannot be represented safely.
    pub fn register(
        &self,
        provider: ProviderKind,
        session_id: SessionId,
        worktree_id: WorktreeId,
    ) -> Result<ProviderLifecycleEndpoint> {
        self.credentials.register(provider, session_id, worktree_id)
    }

    /// Invalidates the credential for a completed or failed Session.
    pub fn invalidate(&self, session_id: SessionId) {
        self.credentials.invalidate(session_id);
    }

    #[must_use]
    pub fn relay_executable(&self) -> &std::path::Path {
        &self.relay_executable
    }

    #[must_use]
    pub fn credentials(&self) -> HookCredentials {
        self.credentials.clone()
    }

    pub async fn shutdown(self) {
        self.shutdown.send_replace(true);
        let _ = self.task.await;
    }
}

#[allow(clippy::too_many_lines)]
async fn handle_connection(
    mut stream: TcpStream,
    _peer: SocketAddr,
    credentials: HookCredentials,
    body_limit: usize,
    limiter: Arc<RateLimiter>,
    deliveries: mpsc::Sender<HookDelivery>,
) -> Result<()> {
    let mut bytes = Vec::with_capacity(4096);
    let header_end = loop {
        if bytes.len() >= HEADER_LIMIT {
            write_status(&mut stream, 431, "Request Header Fields Too Large").await?;
            return Ok(());
        }
        let mut buffer = [0_u8; 2048];
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            return Ok(());
        }
        bytes.extend_from_slice(&buffer[..read]);
        if let Some(position) = find_bytes(&bytes, b"\r\n\r\n") {
            break position + 4;
        }
    };
    let headers = std::str::from_utf8(&bytes[..header_end])
        .map_err(|_| DaemonError::Lifecycle("hook request headers are not UTF-8".into()))?;
    let mut lines = headers.split("\r\n");
    if lines.next() != Some("POST /v1/events HTTP/1.1") {
        write_status(&mut stream, 404, "Not Found").await?;
        return Ok(());
    }
    let mut content_length = None;
    let mut content_type = None;
    let mut authorization = None;
    let mut session_id = None;
    let mut worktree_id = None;
    for line in lines.filter(|line| !line.is_empty()) {
        let Some((name, value)) = line.split_once(':') else {
            write_status(&mut stream, 400, "Bad Request").await?;
            return Ok(());
        };
        match name.trim().to_ascii_lowercase().as_str() {
            "content-length" => content_length = value.trim().parse::<usize>().ok(),
            "content-type" => content_type = Some(value.trim()),
            "authorization" => authorization = Some(value.trim()),
            "x-sylvops-session-id" => session_id = value.trim().parse().ok(),
            "x-sylvops-worktree-id" => worktree_id = value.trim().parse().ok(),
            _ => {}
        }
    }
    let (Some(session_id), Some(worktree_id)) = (session_id, worktree_id) else {
        write_status(&mut stream, 400, "Bad Request").await?;
        return Ok(());
    };
    let supplied = authorization.and_then(|value| value.strip_prefix("Bearer "));
    let Some(credential) = credentials.authenticate(supplied, session_id, worktree_id) else {
        write_status(&mut stream, 401, "Unauthorized").await?;
        return Ok(());
    };
    let Ok(_session_connection) = credential.connection_slots.clone().try_acquire_owned() else {
        write_status(&mut stream, 503, "Service Unavailable").await?;
        return Ok(());
    };
    if !limiter.allow() || !credential.limiter.allow() {
        write_status(&mut stream, 429, "Too Many Requests").await?;
        return Ok(());
    }
    if content_type.is_none_or(|value| !value.eq_ignore_ascii_case("application/json")) {
        write_status(&mut stream, 415, "Unsupported Media Type").await?;
        return Ok(());
    }
    let Some(length) =
        content_length.filter(|length| *length <= body_limit && *length <= credential.body_limit)
    else {
        write_status(&mut stream, 413, "Content Too Large").await?;
        return Ok(());
    };
    while bytes.len().saturating_sub(header_end) < length {
        let remaining = length - bytes.len().saturating_sub(header_end);
        let mut buffer = vec![0_u8; remaining.min(8192)];
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            write_status(&mut stream, 400, "Bad Request").await?;
            return Ok(());
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
    let payload_bytes = bytes[header_end..header_end + length].to_vec();
    let payload = ProviderLifecyclePayload::new(payload_bytes.clone())
        .map_err(|error| DaemonError::Provider(error.to_string()))?;
    let fingerprint = format!("{:x}", Sha256::digest(&payload_bytes));
    let Ok(queue_permit) = credential.queue_slots.clone().try_acquire_owned() else {
        write_status(&mut stream, 503, "Service Unavailable").await?;
        return Ok(());
    };
    if deliveries
        .try_send(HookDelivery {
            provider: credential.provider,
            session_id,
            worktree_id,
            fingerprint,
            payload,
            _session_queue_permit: queue_permit,
        })
        .is_err()
    {
        write_status(&mut stream, 503, "Service Unavailable").await?;
        return Ok(());
    }
    write_status(&mut stream, 204, "No Content").await
}

async fn write_status(stream: &mut TcpStream, status: u16, reason: &str) -> Result<()> {
    stream
        .write_all(
            format!("HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await?;
    Ok(())
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Relays one provider hook from stdin to the authenticated daemon loopback endpoint.
///
/// # Errors
///
/// Returns an error for missing managed environment, invalid/oversized input, transport failure,
/// or a non-success response from the daemon.
pub fn emit_from_environment() -> Result<()> {
    let endpoint = std::env::var("SYLVOPS_HOOK_ENDPOINT")
        .map_err(|_| DaemonError::Lifecycle("hook endpoint is unavailable".into()))?;
    let token = std::env::var("SYLVOPS_HOOK_TOKEN")
        .map_err(|_| DaemonError::Lifecycle("hook token is unavailable".into()))?;
    let session_id = std::env::var("SYLVOPS_SESSION_ID")
        .map_err(|_| DaemonError::Lifecycle("hook session ID is unavailable".into()))?;
    let worktree_id = std::env::var("SYLVOPS_WORKTREE_ID")
        .map_err(|_| DaemonError::Lifecycle("hook worktree ID is unavailable".into()))?;
    let mut body = Vec::new();
    std::io::stdin()
        .take((DEFAULT_BODY_LIMIT + 1) as u64)
        .read_to_end(&mut body)?;
    if body.len() > DEFAULT_BODY_LIMIT {
        return Err(DaemonError::Lifecycle(
            "hook payload exceeded 64 KiB".into(),
        ));
    }
    serde_json::from_slice::<serde_json::Value>(&body)
        .map_err(|_| DaemonError::Lifecycle("hook payload is not valid JSON".into()))?;
    let (address, path) = parse_loopback_endpoint(&endpoint)?;
    let mut stream = StdTcpStream::connect_timeout(&address, Duration::from_secs(2))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    write!(
        stream,
        "POST {path} HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nX-SylvOps-Session-Id: {session_id}\r\nX-SylvOps-Worktree-Id: {worktree_id}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(&body)?;
    stream.flush()?;
    let mut response = [0_u8; 64];
    let read = stream.read(&mut response)?;
    let line = std::str::from_utf8(&response[..read]).unwrap_or_default();
    if !(line.starts_with("HTTP/1.1 204") || line.starts_with("HTTP/1.1 202")) {
        return Err(DaemonError::Lifecycle(
            "hook receiver refused the event".into(),
        ));
    }
    Ok(())
}

fn parse_loopback_endpoint(endpoint: &str) -> Result<(SocketAddr, &str)> {
    let remainder = endpoint
        .strip_prefix("http://")
        .ok_or_else(|| DaemonError::Lifecycle("hook endpoint must use HTTP".into()))?;
    let (authority, path) = remainder
        .split_once('/')
        .ok_or_else(|| DaemonError::Lifecycle("hook endpoint path is missing".into()))?;
    let address: SocketAddr = authority
        .parse()
        .map_err(|_| DaemonError::Lifecycle("hook endpoint address is invalid".into()))?;
    if !address.ip().is_loopback() {
        return Err(DaemonError::Lifecycle(
            "hook endpoint is not loopback".into(),
        ));
    }
    Ok((address, &endpoint[endpoint.len() - path.len() - 1..]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sylvops_core::domain::ProviderKind;

    async fn send_test_request(
        endpoint: &ProviderLifecycleEndpoint,
        token: &str,
        session_id: SessionId,
        worktree_id: WorktreeId,
        body: &[u8],
    ) -> String {
        let (address, path) = parse_loopback_endpoint(endpoint.url()).expect("test endpoint");
        let mut stream = TcpStream::connect(address).await.expect("hook connection");
        stream
            .write_all(
                format!(
                    "POST {path} HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nX-SylvOps-Session-Id: {session_id}\r\nX-SylvOps-Worktree-Id: {worktree_id}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .expect("hook headers");
        stream.write_all(body).await.expect("hook body");
        let mut response = Vec::new();
        // A zero-length HTTP response is complete at the header terminator. Waiting for TCP EOF
        // is not portable because some systems reset a rejected request with an unread body.
        loop {
            let mut chunk = [0_u8; 256];
            match stream.read(&mut chunk).await {
                Ok(0) => break,
                Ok(read) => {
                    response.extend_from_slice(&chunk[..read]);
                    if find_bytes(&response, b"\r\n\r\n").is_some() {
                        break;
                    }
                    assert!(
                        response.len() < HEADER_LIMIT,
                        "hook response header too large"
                    );
                }
                Err(error)
                    if error.kind() == std::io::ErrorKind::ConnectionReset
                        && find_bytes(&response, b"\r\n\r\n").is_some() =>
                {
                    break;
                }
                Err(error) => panic!("hook response: {error}"),
            }
        }
        assert!(
            find_bytes(&response, b"\r\n\r\n").is_some(),
            "hook response header was incomplete"
        );
        String::from_utf8_lossy(&response).into_owned()
    }

    #[test]
    fn rate_limit_is_bounded() {
        let limiter = RateLimiter::new(2);
        assert!(limiter.allow());
        assert!(limiter.allow());
        assert!(!limiter.allow());
    }

    #[tokio::test]
    async fn receiver_authenticates_and_delivers_unknown_events_for_audit() {
        let mut receiver =
            HookReceiver::bind(std::env::current_exe().expect("test executable"), 1_024, 1)
                .await
                .expect("hook receiver");
        let mut deliveries = receiver.take_deliveries();
        let session_id = SessionId::new();
        let worktree_id = WorktreeId::new();
        let endpoint = receiver
            .register(ProviderKind::Codex, session_id, worktree_id)
            .expect("session credential");
        let body = br#"{"hook_event_name":"FutureEvent","session_id":"fake-session"}"#;
        let unauthorized =
            send_test_request(&endpoint, "wrong-token", session_id, worktree_id, body).await;
        assert!(unauthorized.starts_with("HTTP/1.1 401"));
        let accepted = send_test_request(
            &endpoint,
            endpoint.bearer_token(),
            session_id,
            worktree_id,
            body,
        )
        .await;
        assert!(accepted.starts_with("HTTP/1.1 204"));
        let delivery = deliveries.recv().await.expect("unknown delivery");
        assert_eq!(delivery.payload.as_bytes(), body);
        let limited = send_test_request(
            &endpoint,
            endpoint.bearer_token(),
            session_id,
            worktree_id,
            body,
        )
        .await;
        assert!(limited.starts_with("HTTP/1.1 429"));
        receiver.shutdown().await;
    }

    #[tokio::test]
    async fn receiver_isolates_and_invalidates_session_credentials() {
        let mut receiver =
            HookReceiver::bind(std::env::current_exe().expect("test executable"), 1_024, 16)
                .await
                .expect("hook receiver");
        let mut deliveries = receiver.take_deliveries();
        let first_session = SessionId::new();
        let second_session = SessionId::new();
        let first_worktree = WorktreeId::new();
        let second_worktree = WorktreeId::new();
        let shell_session = SessionId::new();
        let shell_worktree = WorktreeId::new();
        let first = receiver
            .register(ProviderKind::Codex, first_session, first_worktree)
            .expect("first credential");
        let second = receiver
            .register(ProviderKind::Codex, second_session, second_worktree)
            .expect("second credential");
        let shell = receiver
            .register(ProviderKind::Shell, shell_session, shell_worktree)
            .expect("other-provider credential");
        assert_ne!(first.bearer_token(), second.bearer_token());

        let body = br#"{"hook_event_name":"FutureEvent"}"#;
        let cross_session = send_test_request(
            &first,
            first.bearer_token(),
            second_session,
            second_worktree,
            body,
        )
        .await;
        assert!(cross_session.starts_with("HTTP/1.1 401"));

        let accepted = send_test_request(
            &first,
            first.bearer_token(),
            first_session,
            first_worktree,
            body,
        )
        .await;
        assert!(accepted.starts_with("HTTP/1.1 204"));
        let delivery = deliveries.recv().await.expect("first delivery");
        assert_eq!(delivery.provider, ProviderKind::Codex);
        assert_eq!(delivery.session_id, first_session);
        assert_eq!(delivery.worktree_id, first_worktree);

        let cross_provider = send_test_request(
            &first,
            first.bearer_token(),
            shell_session,
            shell_worktree,
            body,
        )
        .await;
        assert!(cross_provider.starts_with("HTTP/1.1 401"));
        let shell_accepted = send_test_request(
            &shell,
            shell.bearer_token(),
            shell_session,
            shell_worktree,
            body,
        )
        .await;
        assert!(shell_accepted.starts_with("HTTP/1.1 204"));
        let shell_delivery = deliveries.recv().await.expect("other-provider delivery");
        assert_eq!(shell_delivery.provider, ProviderKind::Shell);

        receiver.invalidate(first_session);
        let invalidated = send_test_request(
            &first,
            first.bearer_token(),
            first_session,
            first_worktree,
            body,
        )
        .await;
        assert!(invalidated.starts_with("HTTP/1.1 401"));
        receiver.shutdown().await;
    }

    #[tokio::test]
    async fn receiver_bounds_each_session_queue_independently() {
        let receiver = HookReceiver::bind(
            std::env::current_exe().expect("test executable"),
            1_024,
            10_000,
        )
        .await
        .expect("hook receiver");
        let first_session = SessionId::new();
        let second_session = SessionId::new();
        let first_worktree = WorktreeId::new();
        let second_worktree = WorktreeId::new();
        let first = receiver
            .register(ProviderKind::Codex, first_session, first_worktree)
            .expect("first credential");
        let second = receiver
            .register(ProviderKind::Codex, second_session, second_worktree)
            .expect("second credential");
        let body = br#"{"hook_event_name":"FutureEvent"}"#;

        for _ in 0..32 {
            let accepted = send_test_request(
                &first,
                first.bearer_token(),
                first_session,
                first_worktree,
                body,
            )
            .await;
            assert!(accepted.starts_with("HTTP/1.1 204"));
        }
        let saturated = send_test_request(
            &first,
            first.bearer_token(),
            first_session,
            first_worktree,
            body,
        )
        .await;
        assert!(saturated.starts_with("HTTP/1.1 503"));

        let independent = send_test_request(
            &second,
            second.bearer_token(),
            second_session,
            second_worktree,
            body,
        )
        .await;
        assert!(independent.starts_with("HTTP/1.1 204"));
        receiver.shutdown().await;
    }

    #[tokio::test]
    async fn receiver_rate_limits_one_session_without_starving_another() {
        let receiver =
            HookReceiver::bind(std::env::current_exe().expect("test executable"), 1_024, 8)
                .await
                .expect("hook receiver");
        let first_session = SessionId::new();
        let second_session = SessionId::new();
        let first_worktree = WorktreeId::new();
        let second_worktree = WorktreeId::new();
        let first = receiver
            .register(ProviderKind::Codex, first_session, first_worktree)
            .expect("first credential");
        let second = receiver
            .register(ProviderKind::Codex, second_session, second_worktree)
            .expect("second credential");
        let body = br#"{"hook_event_name":"FutureEvent"}"#;

        for _ in 0..2 {
            let accepted = send_test_request(
                &first,
                first.bearer_token(),
                first_session,
                first_worktree,
                body,
            )
            .await;
            assert!(accepted.starts_with("HTTP/1.1 204"));
        }
        let limited = send_test_request(
            &first,
            first.bearer_token(),
            first_session,
            first_worktree,
            body,
        )
        .await;
        assert!(limited.starts_with("HTTP/1.1 429"));

        let independent = send_test_request(
            &second,
            second.bearer_token(),
            second_session,
            second_worktree,
            body,
        )
        .await;
        assert!(independent.starts_with("HTTP/1.1 204"));
        receiver.shutdown().await;
    }
}
