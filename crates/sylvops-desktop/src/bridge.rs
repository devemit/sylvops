use std::{
    any::TypeId,
    hash::Hash,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use crossbeam_channel::{Receiver, Sender, bounded};
use iced::{
    Subscription,
    advanced::subscription::{EventStream, Hasher, Recipe},
    futures::stream::BoxStream,
};
use sylvops_core::{
    domain::{DaemonSnapshot, ProviderKind},
    ids::{SessionId, WorkspaceId, WorktreeId},
    protocol::{ClientRequest, DaemonEvent, DaemonResponse, MAX_PTY_CHUNK_SIZE},
    provider::ProviderHealth,
};
use sylvops_daemon::data_removal::wait_for_removal_completion;
use sylvops_daemon::{client::DaemonClient, runtime::RuntimePaths};
use tokio::sync::mpsc;

const COMMAND_CAPACITY: usize = 512;
const TERMINAL_INPUT_BATCH_BYTES: usize = 16 * 1024;
const TERMINAL_INPUT_BATCH_DELAY: Duration = Duration::from_millis(2);
const CRITICAL_EVENT_CAPACITY: usize = 256;
const TERMINAL_EVENT_CAPACITY: usize = 4_096;
const WAKE_CAPACITY: usize = 1;
const CRITICAL_EVENT_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug)]
pub(crate) enum Operation {
    RefreshSnapshot,
    ProbeProvider(ProviderKind),
    CreateWorkspace,
    OpenWorkspace(WorkspaceId),
    RegisterProject,
    CreateWorktree,
    CreateSession(WorktreeId),
    Resume(SessionId),
    RenameProject,
    RenameWorktree,
    RenameSession,
    InspectRemoval(WorktreeId),
    RemoveWorktree,
    Attach(SessionId),
    Detach(SessionId),
    Stop(SessionId),
    Resize,
    LoadDiff(WorktreeId),
    Input(SessionId),
    SaveDesktopState,
    CheckForUpdate,
    GetUpdateStatus,
    DownloadUpdate,
    InstallUpdate,
    PrepareDataRemoval,
}

#[derive(Debug)]
enum BridgeCommand {
    Request {
        operation: Operation,
        request: ClientRequest,
    },
    RemoveUserData {
        confirmation: String,
    },
    TerminalInput {
        session_id: SessionId,
        bytes: Vec<u8>,
    },
    FlushTerminalInput,
    Shutdown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TerminalInputError {
    Saturated,
    Closed,
    Oversized,
}

enum NextCommand {
    Pending(BridgeCommand),
    PendingAfterInputFailure(BridgeCommand),
    Await,
    InputFailed,
    Closed,
}

struct TerminalInputDeliveryFailed;

trait TerminalInputTransport {
    async fn send_terminal_input(
        &self,
        session_id: SessionId,
        bytes: Vec<u8>,
    ) -> Result<DaemonResponse, String>;
}

impl TerminalInputTransport for DaemonClient {
    async fn send_terminal_input(
        &self,
        session_id: SessionId,
        bytes: Vec<u8>,
    ) -> Result<DaemonResponse, String> {
        self.request(&ClientRequest::SessionInput { session_id, bytes })
            .await
            .map_err(|error| error.to_string())
    }
}

#[derive(Clone, Debug)]
pub(crate) enum BridgeEvent {
    Connected {
        snapshot: DaemonSnapshot,
        providers: Vec<ProviderHealth>,
        desktop_state: Option<sylvops_core::ui::DesktopState>,
    },
    Daemon(DaemonEvent),
    Response {
        operation: Operation,
        response: DaemonResponse,
    },
    Error {
        operation: Option<Operation>,
        message: String,
    },
    DataRemovalFinished(Result<(), String>),
    Closed,
}

pub(crate) struct Bridge {
    commands: mpsc::Sender<BridgeCommand>,
    critical_events: Receiver<BridgeEvent>,
    terminal_events: Receiver<BridgeEvent>,
    wakeups: Receiver<()>,
    wakeup: Sender<()>,
    overflowed: Arc<AtomicBool>,
    input_closed: Arc<AtomicBool>,
}

#[cfg(test)]
pub(crate) struct BridgeHarness {
    commands: mpsc::Receiver<BridgeCommand>,
    events: EventSink,
    wakeups: Receiver<()>,
}

#[derive(Clone)]
struct EventSink {
    critical: Sender<BridgeEvent>,
    terminal: Sender<BridgeEvent>,
    wakeup: Sender<()>,
    overflowed: Arc<AtomicBool>,
}

struct BridgeWake {
    receiver: Receiver<()>,
}

struct CancelOnDrop(Sender<()>);

impl Bridge {
    pub(crate) fn spawn(paths: RuntimePaths) -> Self {
        let (bridge, command_rx, events) = Self::channels();
        let input_closed = bridge.input_closed.clone();
        thread::Builder::new()
            .name("sylvops-desktop-ipc".into())
            .spawn(move || run_worker(paths, command_rx, events, &input_closed))
            .expect("desktop IPC worker thread must start");
        bridge
    }

    fn channels() -> (Self, mpsc::Receiver<BridgeCommand>, EventSink) {
        let (commands, command_rx) = mpsc::channel(COMMAND_CAPACITY);
        let (critical_tx, critical_events) = bounded(CRITICAL_EVENT_CAPACITY);
        let (terminal_tx, terminal_events) = bounded(TERMINAL_EVENT_CAPACITY);
        let (wakeup, wakeups) = bounded(WAKE_CAPACITY);
        let overflowed = Arc::new(AtomicBool::new(false));
        let input_closed = Arc::new(AtomicBool::new(false));
        let events = EventSink {
            critical: critical_tx,
            terminal: terminal_tx,
            wakeup: wakeup.clone(),
            overflowed: overflowed.clone(),
        };
        (
            Self {
                commands,
                critical_events,
                terminal_events,
                wakeups,
                wakeup,
                overflowed,
                input_closed,
            },
            command_rx,
            events,
        )
    }

    #[cfg(test)]
    pub(crate) fn harness() -> (Self, BridgeHarness) {
        let (bridge, commands, events) = Self::channels();
        let wakeups = bridge.wakeups.clone();
        (
            bridge,
            BridgeHarness {
                commands,
                events,
                wakeups,
            },
        )
    }

    pub(crate) fn request(&self, operation: Operation, request: ClientRequest) -> bool {
        if matches!(&request, ClientRequest::SessionInput { .. }) {
            return false;
        }
        self.commands
            .try_send(BridgeCommand::Request { operation, request })
            .is_ok()
    }

    pub(crate) fn terminal_input(
        &self,
        session_id: SessionId,
        bytes: Vec<u8>,
    ) -> Result<(), TerminalInputError> {
        if bytes.is_empty() {
            return Ok(());
        }
        if self.input_closed.load(Ordering::Acquire) {
            return Err(TerminalInputError::Closed);
        }
        if bytes.len() > MAX_PTY_CHUNK_SIZE {
            return Err(TerminalInputError::Oversized);
        }
        self.commands
            .try_send(BridgeCommand::TerminalInput { session_id, bytes })
            .map_err(|error| terminal_input_send_error(&error))
    }

    pub(crate) fn flush_terminal_input(&self) -> Result<(), TerminalInputError> {
        if self.input_closed.load(Ordering::Acquire) {
            return Err(TerminalInputError::Closed);
        }
        self.commands
            .try_send(BridgeCommand::FlushTerminalInput)
            .map_err(|error| terminal_input_send_error(&error))
    }

    pub(crate) fn remove_user_data(&self, confirmation: String) -> bool {
        self.commands
            .try_send(BridgeCommand::RemoveUserData { confirmation })
            .is_ok()
    }

    pub(crate) fn drain(&self, limit: usize) -> Vec<BridgeEvent> {
        let mut events = Vec::with_capacity(limit);
        while events.len() < limit {
            let event = self
                .critical_events
                .try_recv()
                .or_else(|_| self.terminal_events.try_recv());
            let Ok(event) = event else {
                break;
            };
            events.push(event);
        }
        if !self.critical_events.is_empty() || !self.terminal_events.is_empty() {
            self.wake();
        }
        events
    }

    pub(crate) fn take_overflowed(&self) -> bool {
        self.overflowed.swap(false, Ordering::AcqRel)
    }

    pub(crate) fn subscription(&self) -> Subscription<()> {
        iced::advanced::subscription::from_recipe(BridgeWake {
            receiver: self.wakeups.clone(),
        })
    }

    fn wake(&self) {
        coalesce_wakeup(&self.wakeup);
    }
}

#[cfg(test)]
impl BridgeHarness {
    pub(crate) fn send_critical(&self, event: BridgeEvent) {
        assert!(self.events.send_critical(event));
    }

    pub(crate) fn send_terminal(&self, event: BridgeEvent) {
        self.events.send_terminal(event);
    }

    pub(crate) fn take_wakeup(&self) -> bool {
        self.wakeups.try_recv().is_ok()
    }

    pub(crate) fn drain_terminal_input(&mut self, expected_session_id: SessionId) -> Vec<u8> {
        let mut bytes = Vec::new();
        loop {
            match self.commands.try_recv() {
                Ok(BridgeCommand::TerminalInput {
                    session_id,
                    bytes: next,
                }) => {
                    assert_eq!(session_id, expected_session_id);
                    bytes.extend(next);
                }
                Ok(command) => panic!("unexpected harness command: {command:?}"),
                Err(mpsc::error::TryRecvError::Empty | mpsc::error::TryRecvError::Disconnected) => {
                    return bytes;
                }
            }
        }
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        let _ = self.commands.try_send(BridgeCommand::FlushTerminalInput);
        let _ = self.commands.try_send(BridgeCommand::Shutdown);
    }
}

fn run_worker(
    paths: RuntimePaths,
    commands: mpsc::Receiver<BridgeCommand>,
    events: EventSink,
    input_closed: &AtomicBool,
) {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            events.send_critical(BridgeEvent::Error {
                operation: None,
                message: format!("cannot initialize desktop IPC runtime: {error}"),
            });
            input_closed.store(true, Ordering::Release);
            return;
        }
    };
    runtime.block_on(run_worker_async(paths, commands, events, input_closed));
    input_closed.store(true, Ordering::Release);
}

#[allow(clippy::too_many_lines)]
async fn run_worker_async(
    paths: RuntimePaths,
    mut commands: mpsc::Receiver<BridgeCommand>,
    events: EventSink,
    input_closed: &AtomicBool,
) {
    let client = match DaemonClient::connect(&paths, "sylvops-desktop").await {
        Ok(client) => client,
        Err(error) => {
            events.send_critical(BridgeEvent::Error {
                operation: None,
                message: format!("cannot connect to the SylvOps daemon: {error}"),
            });
            return;
        }
    };
    let snapshot = match client.request(&ClientRequest::GetSnapshot).await {
        Ok(DaemonResponse::Snapshot(snapshot)) => snapshot,
        Ok(response) => {
            send_startup_error(
                &events,
                format!("unexpected snapshot response: {response:?}"),
            );
            return;
        }
        Err(error) => {
            send_startup_error(&events, format!("cannot load daemon snapshot: {error}"));
            return;
        }
    };
    let providers = match client.request(&ClientRequest::ListProviders).await {
        Ok(DaemonResponse::Providers(providers)) => providers,
        Ok(response) => {
            send_startup_error(
                &events,
                format!("unexpected provider response: {response:?}"),
            );
            return;
        }
        Err(error) => {
            send_startup_error(&events, format!("cannot load providers: {error}"));
            return;
        }
    };
    let desktop_state = match client.request(&ClientRequest::GetDesktopState).await {
        Ok(DaemonResponse::DesktopState(state)) => state,
        Ok(response) => {
            send_startup_error(
                &events,
                format!("unexpected desktop-state response: {response:?}"),
            );
            return;
        }
        Err(error) => {
            send_startup_error(&events, format!("cannot load desktop preferences: {error}"));
            return;
        }
    };
    if !events.send_critical(BridgeEvent::Connected {
        snapshot,
        providers,
        desktop_state,
    }) {
        return;
    }

    let mut daemon_events = client.subscribe();
    let mut deferred_command = None;
    let mut terminal_input_open = true;
    loop {
        let command = if let Some(command) = deferred_command.take() {
            Some(command)
        } else {
            tokio::select! {
                command = commands.recv() => command,
                event = daemon_events.recv() => {
                    match event {
                        Ok(event) => {
                            if matches!(event, DaemonEvent::SessionOutput { .. }) {
                                events.send_terminal(BridgeEvent::Daemon(event));
                            } else {
                                events.send_critical(BridgeEvent::Daemon(event));
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            events.mark_overflowed();
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                    continue;
                }
            }
        };
        let Some(command) = command else {
            break;
        };
        match command {
            BridgeCommand::Request { operation, request } => {
                let request_client = client.clone();
                let request_events = events.clone();
                tokio::spawn(async move {
                    let event = match request_client.request(&request).await {
                        Ok(response) => BridgeEvent::Response {
                            operation,
                            response,
                        },
                        Err(error) => BridgeEvent::Error {
                            operation: Some(operation),
                            message: error.to_string(),
                        },
                    };
                    request_events.send_critical(event);
                });
            }
            BridgeCommand::RemoveUserData { confirmation } => {
                let response = client
                    .request(&ClientRequest::PrepareDataRemoval {
                        confirmation: confirmation.clone(),
                    })
                    .await;
                match response {
                    Ok(response) => {
                        let prepared = matches!(response, DaemonResponse::DataRemovalPrepared);
                        events.send_critical(BridgeEvent::Response {
                            operation: Operation::PrepareDataRemoval,
                            response,
                        });
                        if prepared {
                            drop(daemon_events);
                            drop(client);
                            let result = wait_for_removal_completion(&paths).await;
                            let result = result.map_err(|error| error.to_string());
                            events.send_critical(BridgeEvent::DataRemovalFinished(result));
                            return;
                        }
                    }
                    Err(error) => {
                        events.send_critical(BridgeEvent::Error {
                            operation: Some(Operation::PrepareDataRemoval),
                            message: error.to_string(),
                        });
                    }
                }
            }
            BridgeCommand::TerminalInput { session_id, bytes } => {
                if terminal_input_open {
                    match send_terminal_input_batch(
                        &client,
                        &events,
                        &mut commands,
                        session_id,
                        bytes,
                    )
                    .await
                    {
                        NextCommand::Pending(command) => {
                            deferred_command = Some(command);
                        }
                        NextCommand::PendingAfterInputFailure(command) => {
                            deferred_command = Some(command);
                            terminal_input_open = false;
                        }
                        NextCommand::Await => {}
                        NextCommand::InputFailed => terminal_input_open = false,
                        NextCommand::Closed => break,
                    }
                    if !terminal_input_open {
                        input_closed.store(true, Ordering::Release);
                    }
                }
            }
            BridgeCommand::FlushTerminalInput => {}
            BridgeCommand::Shutdown => break,
        }
    }
    events.send_critical(BridgeEvent::Closed);
}

async fn send_terminal_input_batch(
    client: &impl TerminalInputTransport,
    events: &EventSink,
    commands: &mut mpsc::Receiver<BridgeCommand>,
    session_id: SessionId,
    bytes: Vec<u8>,
) -> NextCommand {
    let deadline = tokio::time::Instant::now() + TERMINAL_INPUT_BATCH_DELAY;
    let mut batch = Vec::with_capacity(TERMINAL_INPUT_BATCH_BYTES);
    let mut input_bytes = bytes;
    let mut offset = 0;

    loop {
        let available = TERMINAL_INPUT_BATCH_BYTES.saturating_sub(batch.len());
        let take = available.min(input_bytes.len().saturating_sub(offset));
        batch.extend_from_slice(&input_bytes[offset..offset + take]);
        offset += take;

        if batch.len() == TERMINAL_INPUT_BATCH_BYTES
            && flush_terminal_input_batch(client, events, session_id, &mut batch)
                .await
                .is_err()
        {
            return NextCommand::InputFailed;
        }
        if offset < input_bytes.len() {
            continue;
        }

        match tokio::time::timeout_at(deadline, commands.recv()).await {
            Ok(Some(BridgeCommand::TerminalInput {
                session_id: next_session_id,
                bytes: next_bytes,
            })) if next_session_id == session_id => {
                input_bytes = next_bytes;
                offset = 0;
            }
            Ok(Some(command)) => {
                return if flush_terminal_input_batch(client, events, session_id, &mut batch)
                    .await
                    .is_ok()
                {
                    NextCommand::Pending(command)
                } else {
                    NextCommand::PendingAfterInputFailure(command)
                };
            }
            Ok(None) => {
                let _ = flush_terminal_input_batch(client, events, session_id, &mut batch).await;
                return NextCommand::Closed;
            }
            Err(_) => {
                return if flush_terminal_input_batch(client, events, session_id, &mut batch)
                    .await
                    .is_ok()
                {
                    NextCommand::Await
                } else {
                    NextCommand::InputFailed
                };
            }
        }
    }
}

async fn flush_terminal_input_batch(
    client: &impl TerminalInputTransport,
    events: &EventSink,
    session_id: SessionId,
    batch: &mut Vec<u8>,
) -> Result<(), TerminalInputDeliveryFailed> {
    if batch.is_empty() {
        return Ok(());
    }
    let bytes = std::mem::take(batch);
    match client.send_terminal_input(session_id, bytes).await {
        Ok(DaemonResponse::Acknowledged) => Ok(()),
        Ok(_) => {
            events.send_critical(BridgeEvent::Error {
                operation: Some(Operation::Input(session_id)),
                message: "the daemon returned an unexpected input acknowledgement; terminal input was stopped before any later queued bytes could be sent".into(),
            });
            Err(TerminalInputDeliveryFailed)
        }
        Err(error) => {
            events.send_critical(BridgeEvent::Error {
                operation: Some(Operation::Input(session_id)),
                message: bounded_input_error(&error),
            });
            Err(TerminalInputDeliveryFailed)
        }
    }
}

fn terminal_input_send_error<T>(error: &mpsc::error::TrySendError<T>) -> TerminalInputError {
    match error {
        mpsc::error::TrySendError::Full(_) => TerminalInputError::Saturated,
        mpsc::error::TrySendError::Closed(_) => TerminalInputError::Closed,
    }
}

fn bounded_input_error(message: &str) -> String {
    let detail: String = message
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(512)
        .collect();
    format!(
        "terminal input transport failed; terminal input was stopped before any later queued bytes could be sent: {detail}"
    )
}

fn send_startup_error(events: &EventSink, message: String) {
    events.send_critical(BridgeEvent::Error {
        operation: None,
        message,
    });
}

impl EventSink {
    fn send_critical(&self, event: BridgeEvent) -> bool {
        let delivered = self
            .critical
            .send_timeout(event, CRITICAL_EVENT_TIMEOUT)
            .is_ok();
        if !delivered {
            self.overflowed.store(true, Ordering::Release);
        }
        self.wake();
        delivered
    }

    fn send_terminal(&self, event: BridgeEvent) {
        if self.terminal.try_send(event).is_err() {
            self.overflowed.store(true, Ordering::Release);
        }
        self.wake();
    }

    fn mark_overflowed(&self) {
        self.overflowed.store(true, Ordering::Release);
        self.wake();
    }

    fn wake(&self) {
        coalesce_wakeup(&self.wakeup);
    }
}

impl Recipe for BridgeWake {
    type Output = ();

    fn hash(&self, state: &mut Hasher) {
        TypeId::of::<Self>().hash(state);
    }

    fn stream(self: Box<Self>, _input: EventStream) -> BoxStream<'static, Self::Output> {
        Box::pin(iced::stream::channel(1, async move |mut output| {
            let receiver = self.receiver;
            let (cancel, cancelled) = bounded(1);
            let _cancel_on_drop = CancelOnDrop(cancel);
            let _ = tokio::task::spawn_blocking(move || {
                loop {
                    crossbeam_channel::select! {
                        recv(cancelled) -> _ => break,
                        recv(receiver) -> wake => {
                            if wake.is_err() {
                                break;
                            }
                            if let Err(error) = output.try_send(())
                                && error.is_disconnected()
                            {
                                break;
                            }
                        }
                    }
                }
            })
            .await;
        }))
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        let _ = self.0.try_send(());
    }
}

fn coalesce_wakeup(wakeup: &Sender<()>) {
    let _ = wakeup.try_send(());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingInputTransport {
        batches: Mutex<Vec<Vec<u8>>>,
    }

    impl TerminalInputTransport for RecordingInputTransport {
        fn send_terminal_input(
            &self,
            _session_id: SessionId,
            bytes: Vec<u8>,
        ) -> impl Future<Output = Result<DaemonResponse, String>> {
            self.batches.lock().expect("batch lock").push(bytes);
            std::future::ready(Ok(DaemonResponse::Acknowledged))
        }
    }

    #[tokio::test]
    async fn two_hundred_keys_use_the_ordered_batch_and_consume_successful_acknowledgement() {
        let (bridge, harness) = Bridge::harness();
        let (commands, mut command_rx) = mpsc::channel(COMMAND_CAPACITY);
        let session_id = SessionId::new();
        let expected: Vec<u8> = (0..200)
            .map(|index| b'0' + u8::try_from(index % 10).expect("decimal digit"))
            .collect();
        for byte in expected.iter().skip(1) {
            commands
                .try_send(BridgeCommand::TerminalInput {
                    session_id,
                    bytes: vec![*byte],
                })
                .expect("bounded test input");
        }
        commands
            .try_send(BridgeCommand::FlushTerminalInput)
            .expect("flush boundary");
        let transport = RecordingInputTransport::default();

        let next = send_terminal_input_batch(
            &transport,
            &harness.events,
            &mut command_rx,
            session_id,
            vec![expected[0]],
        )
        .await;

        assert!(matches!(
            next,
            NextCommand::Pending(BridgeCommand::FlushTerminalInput)
        ));
        assert_eq!(
            transport.batches.into_inner().expect("batch lock"),
            vec![expected]
        );
        assert!(bridge.drain(1).is_empty());
        assert!(!harness.take_wakeup());
    }

    #[test]
    fn heavy_terminal_output_stays_bounded_and_cannot_starve_critical_events() {
        let (bridge, harness) = Bridge::harness();
        let session_id = SessionId::new();
        for sequence in 1..=TERMINAL_EVENT_CAPACITY as u64 {
            harness.send_terminal(BridgeEvent::Daemon(DaemonEvent::SessionOutput {
                session_id,
                sequence,
                bytes: vec![b'x'],
                replay: false,
            }));
        }
        harness.send_terminal(BridgeEvent::Daemon(DaemonEvent::SessionOutput {
            session_id,
            sequence: TERMINAL_EVENT_CAPACITY as u64 + 1,
            bytes: vec![b'y'],
            replay: false,
        }));
        harness.send_critical(BridgeEvent::Error {
            operation: None,
            message: "critical".into(),
        });

        assert!(harness.take_wakeup());
        assert!(
            !harness.take_wakeup(),
            "wakeups must coalesce while pending"
        );
        assert!(matches!(
            bridge.drain(1).as_slice(),
            [BridgeEvent::Error { message, .. }] if message == "critical"
        ));
        assert_eq!(bridge.drain(512).len(), 512);
        assert!(bridge.take_overflowed());
        assert!(
            harness.take_wakeup(),
            "a partial drain must schedule the bounded remainder"
        );
    }
}
