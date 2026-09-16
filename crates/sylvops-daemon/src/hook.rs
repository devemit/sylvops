//! Authenticated loopback hook receiver and the tiny provider hook relay.

use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream as StdTcpStream},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use crate::{DaemonError, Result, runtime::AuthenticationToken};
use serde_json::Value;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use sylvops_core::{
    ids::{SessionId, WorktreeId},
    provider::HookEndpoint,
    status::NormalizedProviderEvent,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Semaphore, mpsc, watch},
    task::JoinHandle,
    time::timeout,
};

const HEADER_LIMIT: usize = 16 * 1024;
const DEFAULT_BODY_LIMIT: usize = 64 * 1024;

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

#[derive(Clone, Debug)]
pub struct HookDelivery {
    pub session_id: SessionId,
    pub worktree_id: WorktreeId,
    pub external_session_id: Option<String>,
    pub event_name: String,
    pub turn_id: Option<String>,
    pub fingerprint: String,
    pub event: Option<NormalizedProviderEvent>,
}

#[derive(Debug)]
pub struct HookReceiver {
    pub endpoint: HookEndpoint,
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
        let token = AuthenticationToken::generate();
        let profile_name = format!("sylvops-{}", &token.expose()[..12]);
        let endpoint = HookEndpoint {
            url: format!("http://{address}/v1/events"),
            bearer_token: token.expose().to_owned(),
            relay_executable,
            profile_name,
        };
        let (deliveries_tx, deliveries) = mpsc::channel(256);
        let (shutdown, mut shutdown_rx) = watch::channel(false);
        let expected_token = token.expose().to_owned();
        let body_limit = body_limit.clamp(1024, 1024 * 1024);
        let limiter = Arc::new(RateLimiter::new(requests_per_minute));
        let connection_slots = Arc::new(Semaphore::new(64));
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
                        let token = expected_token.clone();
                        let limiter = limiter.clone();
                        tokio::spawn(async move {
                            let _permit = permit;
                            let _ = timeout(
                                Duration::from_secs(2),
                                handle_connection(stream, peer, &token, body_limit, limiter, deliveries),
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
            endpoint,
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

    pub async fn shutdown(self) {
        self.shutdown.send_replace(true);
        let _ = self.task.await;
    }
}

#[allow(clippy::too_many_lines)]
async fn handle_connection(
    mut stream: TcpStream,
    _peer: SocketAddr,
    expected_token: &str,
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
    let supplied = authorization.and_then(|value| value.strip_prefix("Bearer "));
    if supplied.is_none_or(|value| {
        value.len() != expected_token.len()
            || !bool::from(value.as_bytes().ct_eq(expected_token.as_bytes()))
    }) {
        write_status(&mut stream, 401, "Unauthorized").await?;
        return Ok(());
    }
    if !limiter.allow() {
        write_status(&mut stream, 429, "Too Many Requests").await?;
        return Ok(());
    }
    if content_type.is_none_or(|value| !value.eq_ignore_ascii_case("application/json")) {
        write_status(&mut stream, 415, "Unsupported Media Type").await?;
        return Ok(());
    }
    let Some(length) = content_length.filter(|length| *length <= body_limit) else {
        write_status(&mut stream, 413, "Content Too Large").await?;
        return Ok(());
    };
    let (Some(session_id), Some(worktree_id)) = (session_id, worktree_id) else {
        write_status(&mut stream, 400, "Bad Request").await?;
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
    let Ok(payload) = serde_json::from_slice::<Value>(&bytes[header_end..header_end + length])
    else {
        write_status(&mut stream, 400, "Bad Request").await?;
        return Ok(());
    };
    let event_name = payload
        .get("hook_event_name")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .chars()
        .take(80)
        .collect::<String>();
    let event = normalize_event(&payload);
    let external_session_id = payload
        .get("session_id")
        .and_then(Value::as_str)
        .filter(|value| valid_external_id(value))
        .map(str::to_owned);
    let turn_id = payload
        .get("turn_id")
        .and_then(Value::as_str)
        .filter(|value| valid_external_id(value))
        .map(str::to_owned);
    let fingerprint = format!(
        "{:x}",
        Sha256::digest(&bytes[header_end..header_end + length])
    );
    if deliveries
        .try_send(HookDelivery {
            session_id,
            worktree_id,
            external_session_id,
            event_name,
            turn_id,
            fingerprint,
            event,
        })
        .is_err()
    {
        write_status(&mut stream, 503, "Service Unavailable").await?;
        return Ok(());
    }
    write_status(&mut stream, 204, "No Content").await
}

fn normalize_event(payload: &Value) -> Option<NormalizedProviderEvent> {
    match payload.get("hook_event_name")?.as_str()? {
        "SessionStart" => Some(NormalizedProviderEvent::TurnStarted),
        "SessionEnd" => Some(NormalizedProviderEvent::SessionEnded),
        "UserPromptSubmit" => Some(NormalizedProviderEvent::PromptSubmitted),
        "PermissionRequest" => Some(NormalizedProviderEvent::PermissionRequested),
        "SubagentStart" => Some(NormalizedProviderEvent::SubagentStarted {
            agent_id: bounded_field(payload, "agent_id"),
        }),
        "SubagentStop" => Some(NormalizedProviderEvent::SubagentStopped {
            agent_id: bounded_field(payload, "agent_id"),
        }),
        "Stop" | "Interrupt" => Some(NormalizedProviderEvent::TurnStopped),
        _ => None,
    }
}

fn bounded_field(payload: &Value, name: &str) -> String {
    payload
        .get(name)
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .chars()
        .take(200)
        .collect()
}

fn valid_external_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 200
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
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
    serde_json::from_slice::<Value>(&body)
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

    async fn send_test_request(
        endpoint: &HookEndpoint,
        token: &str,
        session_id: SessionId,
        worktree_id: WorktreeId,
        body: &[u8],
    ) -> String {
        let (address, path) = parse_loopback_endpoint(&endpoint.url).expect("test endpoint");
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
        stream
            .read_to_end(&mut response)
            .await
            .expect("hook response");
        String::from_utf8_lossy(&response).into_owned()
    }

    #[test]
    fn unknown_events_do_not_normalize() {
        assert!(normalize_event(&serde_json::json!({"hook_event_name": "FutureEvent"})).is_none());
    }

    #[test]
    fn permission_event_is_observational() {
        assert_eq!(
            normalize_event(&serde_json::json!({"hook_event_name": "PermissionRequest"})),
            Some(NormalizedProviderEvent::PermissionRequested)
        );
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
        let body = br#"{"hook_event_name":"FutureEvent","session_id":"fake-session"}"#;
        let unauthorized = send_test_request(
            &receiver.endpoint,
            "wrong-token",
            session_id,
            worktree_id,
            body,
        )
        .await;
        assert!(unauthorized.starts_with("HTTP/1.1 401"));
        let accepted = send_test_request(
            &receiver.endpoint,
            &receiver.endpoint.bearer_token,
            session_id,
            worktree_id,
            body,
        )
        .await;
        assert!(accepted.starts_with("HTTP/1.1 204"));
        let delivery = deliveries.recv().await.expect("unknown delivery");
        assert_eq!(delivery.event_name, "FutureEvent");
        assert!(delivery.event.is_none());
        let limited = send_test_request(
            &receiver.endpoint,
            &receiver.endpoint.bearer_token,
            session_id,
            worktree_id,
            body,
        )
        .await;
        assert!(limited.starts_with("HTTP/1.1 429"));
        receiver.shutdown().await;
    }
}
