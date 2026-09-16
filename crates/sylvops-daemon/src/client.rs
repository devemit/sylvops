//! Multiplexed authenticated daemon client used by the CLI and future TUI.

use std::collections::HashMap;
use std::time::Duration;

use sylvops_core::protocol::{
    ClientRequest, DaemonEvent, DaemonResponse, Frame, HelloRequest, MessageClass, PROTOCOL_MAJOR,
    PROTOCOL_MINOR, read_frame, write_frame,
};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use uuid::Uuid;

use crate::{
    DaemonError, Result,
    daemon::{CONTROL_OPCODE, EVENT_OPCODE},
    ipc::connect,
    runtime::{AuthenticationToken, RuntimePaths},
};

const CLIENT_COMMAND_CAPACITY: usize = 64;
const CLIENT_EVENT_CAPACITY: usize = 4352;
const CLIENT_WRITE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug)]
struct ClientCommand {
    request: ClientRequest,
    reply: oneshot::Sender<Result<DaemonResponse>>,
}

#[derive(Clone)]
pub struct DaemonClient {
    commands: mpsc::Sender<ClientCommand>,
    events: broadcast::Sender<DaemonEvent>,
    closed: watch::Receiver<bool>,
}

impl DaemonClient {
    /// Connects to the daemon and completes the authenticated protocol handshake.
    ///
    /// # Errors
    ///
    /// Returns an error when local transport, authentication, framing, or version negotiation fails.
    pub async fn connect(paths: &RuntimePaths, client_name: &str) -> Result<Self> {
        let token = AuthenticationToken::read(&paths.authentication_token)?;
        let mut stream = connect(&paths.endpoint).await?;
        let hello = ClientRequest::Hello(HelloRequest {
            client_name: client_name.into(),
            client_version: env!("CARGO_PKG_VERSION").into(),
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            authentication_token: token.expose().into(),
        });
        let frame = Frame::message(MessageClass::Request, CONTROL_OPCODE, &hello)?;
        let request_id = frame.message_id;
        write_frame(&mut stream, &frame).await?;
        let response = read_frame(&mut stream).await?;
        if response.class != MessageClass::Response
            || response.opcode != CONTROL_OPCODE
            || response.correlation_id != Some(request_id)
        {
            return Err(DaemonError::Lifecycle(
                "daemon handshake response was not correlated".into(),
            ));
        }
        match response.payload_as::<DaemonResponse>()? {
            DaemonResponse::Welcome(welcome)
                if welcome.protocol_major == PROTOCOL_MAJOR
                    && welcome.protocol_minor == PROTOCOL_MINOR => {}
            DaemonResponse::Error(failure) => {
                return Err(DaemonError::Lifecycle(format!(
                    "daemon rejected handshake: {}",
                    failure.message
                )));
            }
            _ => {
                return Err(DaemonError::Lifecycle(
                    "daemon returned an invalid handshake response".into(),
                ));
            }
        }

        let (commands, command_rx) = mpsc::channel(CLIENT_COMMAND_CAPACITY);
        let (events, _) = broadcast::channel(CLIENT_EVENT_CAPACITY);
        let (closed_tx, closed) = watch::channel(false);
        tokio::spawn(run_client(stream, command_rx, events.clone(), closed_tx));
        Ok(Self {
            commands,
            events,
            closed,
        })
    }

    /// Sends a correlated request while the background reader continues to receive events.
    ///
    /// # Errors
    ///
    /// Returns an error when the request cannot be encoded, queued, delivered, or decoded.
    pub async fn request(&self, request: &ClientRequest) -> Result<DaemonResponse> {
        let (reply, receive) = oneshot::channel();
        self.commands
            .send(ClientCommand {
                request: request.clone(),
                reply,
            })
            .await
            .map_err(|_| DaemonError::Lifecycle("daemon client stopped".into()))?;
        receive
            .await
            .map_err(|_| DaemonError::Lifecycle("daemon request was cancelled".into()))?
    }

    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<DaemonEvent> {
        self.events.subscribe()
    }

    #[must_use]
    pub fn connection_closed(&self) -> watch::Receiver<bool> {
        self.closed.clone()
    }
}

async fn run_client(
    stream: crate::ipc::BoxStream,
    mut commands: mpsc::Receiver<ClientCommand>,
    events: broadcast::Sender<DaemonEvent>,
    closed: watch::Sender<bool>,
) {
    let _closed_guard = ClosedGuard(closed);
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
    let _reader_guard = AbortTask(reader_task.abort_handle());
    let (outgoing, mut outgoing_rx) = mpsc::channel(CLIENT_COMMAND_CAPACITY);
    let mut writer_task = tokio::spawn(async move {
        while let Some(frame) = outgoing_rx.recv().await {
            tokio::time::timeout(CLIENT_WRITE_TIMEOUT, write_frame(&mut writer, &frame))
                .await
                .map_err(|_| DaemonError::Lifecycle("daemon writer timed out".into()))??;
        }
        Ok::<(), DaemonError>(())
    });
    let _writer_guard = AbortTask(writer_task.abort_handle());
    let mut pending: HashMap<Uuid, oneshot::Sender<Result<DaemonResponse>>> = HashMap::new();
    loop {
        tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else { return; };
                match Frame::message(MessageClass::Request, CONTROL_OPCODE, &command.request) {
                    Ok(frame) => {
                        let request_id = frame.message_id;
                        match outgoing.try_send(frame) {
                            Ok(()) => { pending.insert(request_id, command.reply); }
                            Err(_) => {
                                let _ = command.reply.send(Err(DaemonError::Lifecycle(
                                    "daemon writer is unavailable or backpressured".into(),
                                )));
                            }
                        }
                    }
                    Err(error) => { let _ = command.reply.send(Err(error.into())); }
                }
            }
            received = incoming.recv() => {
                let Some(Ok(frame)) = received else {
                    fail_pending(&mut pending, "daemon connection closed");
                    return;
                };
                match (frame.class, frame.opcode) {
                    (MessageClass::Response, CONTROL_OPCODE) => {
                        let Some(correlation) = frame.correlation_id else {
                            fail_pending(&mut pending, "uncorrelated daemon response"); return;
                        };
                        if let Some(reply) = pending.remove(&correlation) {
                            let _ = reply.send(frame.payload_as::<DaemonResponse>().map_err(Into::into));
                        } else {
                            fail_pending(&mut pending, "daemon returned an unknown correlation ID");
                            return;
                        }
                    }
                    (MessageClass::Event, EVENT_OPCODE) if frame.correlation_id.is_none() => {
                        if let Ok(event) = frame.payload_as::<DaemonEvent>() {
                            let _ = events.send(event);
                        } else {
                            fail_pending(&mut pending, "daemon returned a malformed event");
                            return;
                        }
                    }
                    _ => { fail_pending(&mut pending, "unsupported daemon frame"); return; }
                }
            }
            joined = &mut writer_task => {
                let message = match joined {
                    Ok(Ok(())) => "daemon writer stopped",
                    Ok(Err(_)) | Err(_) => "daemon connection failed",
                };
                fail_pending(&mut pending, message);
                return;
            }
        }
    }
}

#[derive(Debug)]
struct AbortTask(tokio::task::AbortHandle);

impl Drop for AbortTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[derive(Debug)]
struct ClosedGuard(watch::Sender<bool>);

impl Drop for ClosedGuard {
    fn drop(&mut self) {
        self.0.send_replace(true);
    }
}

fn fail_pending(
    pending: &mut HashMap<Uuid, oneshot::Sender<Result<DaemonResponse>>>,
    message: &str,
) {
    for (_, reply) in pending.drain() {
        let _ = reply.send(Err(DaemonError::Lifecycle(message.into())));
    }
}

impl std::fmt::Debug for DaemonClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DaemonClient")
            .finish_non_exhaustive()
    }
}
