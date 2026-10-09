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
    protocol::{ClientRequest, DaemonEvent, DaemonResponse},
    provider::ProviderHealth,
};
use sylvops_daemon::data_removal::wait_for_removal_completion;
use sylvops_daemon::{client::DaemonClient, runtime::RuntimePaths};
use tokio::sync::mpsc;

const COMMAND_CAPACITY: usize = 128;
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
    Shutdown,
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
        let (commands, command_rx) = mpsc::channel(COMMAND_CAPACITY);
        let (critical_tx, critical_events) = bounded(CRITICAL_EVENT_CAPACITY);
        let (terminal_tx, terminal_events) = bounded(TERMINAL_EVENT_CAPACITY);
        let (wakeup, wakeups) = bounded(WAKE_CAPACITY);
        let overflowed = Arc::new(AtomicBool::new(false));
        let events = EventSink {
            critical: critical_tx,
            terminal: terminal_tx,
            wakeup: wakeup.clone(),
            overflowed: overflowed.clone(),
        };
        thread::Builder::new()
            .name("sylvops-desktop-ipc".into())
            .spawn(move || run_worker(paths, command_rx, events))
            .expect("desktop IPC worker thread must start");
        Self {
            commands,
            critical_events,
            terminal_events,
            wakeups,
            wakeup,
            overflowed,
        }
    }

    pub(crate) fn request(&self, operation: Operation, request: ClientRequest) -> bool {
        self.commands
            .try_send(BridgeCommand::Request { operation, request })
            .is_ok()
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

impl Drop for Bridge {
    fn drop(&mut self) {
        let _ = self.commands.try_send(BridgeCommand::Shutdown);
    }
}

fn run_worker(paths: RuntimePaths, commands: mpsc::Receiver<BridgeCommand>, events: EventSink) {
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
            return;
        }
    };
    runtime.block_on(run_worker_async(paths, commands, events));
}

#[allow(clippy::too_many_lines)]
async fn run_worker_async(
    paths: RuntimePaths,
    mut commands: mpsc::Receiver<BridgeCommand>,
    events: EventSink,
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
    loop {
        tokio::select! {
            command = commands.recv() => {
                match command {
                    Some(BridgeCommand::Request { operation, request }) => {
                        let request_client = client.clone();
                        let request_events = events.clone();
                        tokio::spawn(async move {
                            let event = match request_client.request(&request).await {
                                Ok(response) => BridgeEvent::Response { operation, response },
                                Err(error) => BridgeEvent::Error {
                                    operation: Some(operation),
                                    message: error.to_string(),
                                },
                            };
                            request_events.send_critical(event);
                        });
                    }
                    Some(BridgeCommand::RemoveUserData { confirmation }) => {
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
                                    let result = wait_for_removal_completion(&paths)
                                        .await
                                        .map_err(|error| error.to_string());
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
                    Some(BridgeCommand::Shutdown) | None => break,
                }
            }
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
            }
        }
    }
    events.send_critical(BridgeEvent::Closed);
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
