use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use crossbeam_channel::{Receiver, Sender, bounded};
use sylvops_core::{
    domain::DaemonSnapshot,
    ids::{SessionId, WorkspaceId, WorktreeId},
    protocol::{ClientRequest, DaemonEvent, DaemonResponse},
    provider::ProviderHealth,
};
use sylvops_daemon::{client::DaemonClient, runtime::RuntimePaths};
use tokio::sync::mpsc;

const COMMAND_CAPACITY: usize = 128;
const EVENT_CAPACITY: usize = 4_096;
const CRITICAL_EVENT_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug)]
pub(crate) enum Operation {
    RefreshSnapshot,
    OpenWorkspace(WorkspaceId),
    CreateShell(WorktreeId),
    Attach(SessionId),
    Detach(SessionId),
    Stop(SessionId),
    LoadDiff(WorktreeId),
    Input(SessionId),
}

#[derive(Debug)]
enum BridgeCommand {
    Request {
        operation: Operation,
        request: ClientRequest,
    },
    Shutdown,
}

#[derive(Clone, Debug)]
pub(crate) enum BridgeEvent {
    Connected {
        snapshot: DaemonSnapshot,
        providers: Vec<ProviderHealth>,
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
    Closed,
}

pub(crate) struct Bridge {
    commands: mpsc::Sender<BridgeCommand>,
    events: Receiver<BridgeEvent>,
    overflowed: Arc<AtomicBool>,
}

impl Bridge {
    pub(crate) fn spawn(paths: RuntimePaths) -> Self {
        let (commands, command_rx) = mpsc::channel(COMMAND_CAPACITY);
        let (events_tx, events) = bounded(EVENT_CAPACITY);
        let overflowed = Arc::new(AtomicBool::new(false));
        let worker_overflowed = overflowed.clone();
        thread::Builder::new()
            .name("sylvops-desktop-ipc".into())
            .spawn(move || run_worker(paths, command_rx, events_tx, worker_overflowed))
            .expect("desktop IPC worker thread must start");
        Self {
            commands,
            events,
            overflowed,
        }
    }

    pub(crate) fn request(&self, operation: Operation, request: ClientRequest) -> bool {
        self.commands
            .try_send(BridgeCommand::Request { operation, request })
            .is_ok()
    }

    pub(crate) fn drain(&self, limit: usize) -> Vec<BridgeEvent> {
        self.events.try_iter().take(limit).collect()
    }

    pub(crate) fn take_overflowed(&self) -> bool {
        self.overflowed.swap(false, Ordering::AcqRel)
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        let _ = self.commands.try_send(BridgeCommand::Shutdown);
    }
}

fn run_worker(
    paths: RuntimePaths,
    commands: mpsc::Receiver<BridgeCommand>,
    events: Sender<BridgeEvent>,
    overflowed: Arc<AtomicBool>,
) {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = events.send(BridgeEvent::Error {
                operation: None,
                message: format!("cannot initialize desktop IPC runtime: {error}"),
            });
            return;
        }
    };
    runtime.block_on(run_worker_async(paths, commands, events, overflowed));
}

async fn run_worker_async(
    paths: RuntimePaths,
    mut commands: mpsc::Receiver<BridgeCommand>,
    events: Sender<BridgeEvent>,
    overflowed: Arc<AtomicBool>,
) {
    let client = match DaemonClient::connect(&paths, "sylvops-desktop").await {
        Ok(client) => client,
        Err(error) => {
            let _ = events.send(BridgeEvent::Error {
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
    if events
        .send_timeout(
            BridgeEvent::Connected {
                snapshot,
                providers,
            },
            CRITICAL_EVENT_TIMEOUT,
        )
        .is_err()
    {
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
                            let _ = request_events.send_timeout(event, CRITICAL_EVENT_TIMEOUT);
                        });
                    }
                    Some(BridgeCommand::Shutdown) | None => break,
                }
            }
            event = daemon_events.recv() => {
                match event {
                    Ok(event) => {
                        let terminal_output = matches!(event, DaemonEvent::SessionOutput { .. });
                        let delivered = if terminal_output {
                            events.try_send(BridgeEvent::Daemon(event)).is_ok()
                        } else {
                            events
                                .send_timeout(BridgeEvent::Daemon(event), CRITICAL_EVENT_TIMEOUT)
                                .is_ok()
                        };
                        if !delivered {
                            overflowed.store(true, Ordering::Release);
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        overflowed.store(true, Ordering::Release);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
    let _ = events.send_timeout(BridgeEvent::Closed, CRITICAL_EVENT_TIMEOUT);
}

fn send_startup_error(events: &Sender<BridgeEvent>, message: String) {
    let _ = events.send_timeout(
        BridgeEvent::Error {
            operation: None,
            message,
        },
        CRITICAL_EVENT_TIMEOUT,
    );
}
