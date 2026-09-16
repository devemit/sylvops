//! Daemon-owned PTY session actor, attachment leases, VT state, and bounded scrollback.

use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    ffi::OsString,
    io::{Read, Write},
    path::PathBuf,
    sync::Arc,
};

use tokio::sync::{broadcast, mpsc, oneshot, watch};
use uuid::Uuid;

use crate::{DaemonError, Result, process_tree, pty};

const ACTOR_QUEUE_CAPACITY: usize = 128;
const LIVE_OUTPUT_CAPACITY: usize = 256;
const READ_CHUNK_SIZE: usize = 8192;
const MAX_SCROLLBACK_CHUNKS: usize = 4096;
const PTY_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Exercises the production PTY backend with a bounded, non-shell child process.
///
/// The probe creates a pseudoterminal, captures output, waits for the child to be reaped, and
/// drops all process-tree handles. It is intentionally independent of repositories and provider
/// credentials so `sylvops doctor` can run before first-use setup.
///
/// # Errors
///
/// Returns an error when the platform diagnostic executable cannot be launched, the process does
/// not exit before the timeout, it exits unsuccessfully, or no output reaches the PTY reader.
pub async fn run_pty_probe() -> Result<()> {
    #[cfg(unix)]
    let (program, arguments) = (
        PathBuf::from("/bin/echo"),
        vec![OsString::from("sylvops-pty-ok")],
    );

    #[cfg(windows)]
    let (program, arguments) = {
        let windows = std::env::var_os("SystemRoot")
            .map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
        (windows.join("System32").join("hostname.exe"), Vec::new())
    };

    let cwd = std::env::current_dir()
        .and_then(std::fs::canonicalize)
        .map_err(|error| DaemonError::Pty(format!("cannot resolve probe directory: {error}")))?;
    let spec = SessionSpec {
        program,
        arguments,
        cwd,
        columns: 80,
        rows: 24,
        scrollback_bytes: 64 * 1024,
    };
    let mut handle = SessionHandle::spawn_sanitized(&spec, &BTreeMap::new())?;
    let exit = if let Ok(result) = tokio::time::timeout(PTY_PROBE_TIMEOUT, handle.wait()).await {
        result?
    } else {
        handle.stop().await?;
        return Err(DaemonError::Pty(
            "PTY probe did not exit within five seconds".into(),
        ));
    };
    if exit.exit_code != 0 {
        return Err(DaemonError::Pty(format!(
            "PTY probe exited with status {}",
            exit.exit_code
        )));
    }

    let replay = handle.replay_after(0).await?;
    if replay.chunks.iter().all(|chunk| chunk.bytes.is_empty()) {
        return Err(DaemonError::Pty(
            "PTY probe exited without observable terminal output".into(),
        ));
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct SessionSpec {
    pub program: PathBuf,
    pub arguments: Vec<OsString>,
    pub cwd: PathBuf,
    pub columns: u16,
    pub rows: u16,
    pub scrollback_bytes: usize,
}

impl SessionSpec {
    /// Validates values needed before a PTY is created.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty program, relative working directory, zero dimensions, or
    /// zero scrollback capacity.
    pub fn validate(&self) -> Result<()> {
        if self.program.as_os_str().is_empty() {
            return Err(DaemonError::InvalidSession("program is empty".into()));
        }
        if !self.cwd.is_absolute() {
            return Err(DaemonError::InvalidSession(
                "working directory must be absolute".into(),
            ));
        }
        if self.columns == 0 || self.rows == 0 {
            return Err(DaemonError::InvalidSession(
                "terminal dimensions must be non-zero".into(),
            ));
        }
        if self.scrollback_bytes == 0 {
            return Err(DaemonError::InvalidSession(
                "scrollback capacity must be non-zero".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutputChunk {
    pub sequence: u64,
    pub bytes: Arc<[u8]>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Replay {
    pub earliest_sequence: Option<u64>,
    pub latest_sequence: u64,
    pub output_gap: bool,
    pub chunks: Vec<OutputChunk>,
    pub terminal_snapshot: Option<Vec<u8>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionExit {
    pub exit_code: u32,
    pub stop_requested: bool,
    pub was_attached: bool,
}

#[derive(Debug)]
pub struct Attachment {
    pub role: sylvops_core::domain::AttachmentRole,
    pub replay: Replay,
    pub live_output: broadcast::Receiver<OutputChunk>,
    pub completion_published: bool,
}

#[derive(Debug)]
pub struct Scrollback {
    capacity_bytes: usize,
    retained_bytes: usize,
    latest_sequence: u64,
    lost_through_sequence: u64,
    chunks: VecDeque<OutputChunk>,
}

impl Scrollback {
    /// Creates a byte-bounded output ring.
    ///
    /// # Panics
    ///
    /// Panics when `capacity_bytes` is zero. Session specifications reject that value before
    /// constructing a buffer.
    #[must_use]
    pub fn new(capacity_bytes: usize) -> Self {
        assert!(capacity_bytes > 0, "scrollback capacity must be non-zero");
        Self {
            capacity_bytes,
            retained_bytes: 0,
            latest_sequence: 0,
            lost_through_sequence: 0,
            chunks: VecDeque::new(),
        }
    }

    pub fn push(&mut self, bytes: impl Into<Arc<[u8]>>) -> OutputChunk {
        self.latest_sequence = self.latest_sequence.saturating_add(1);
        let live_chunk = OutputChunk {
            sequence: self.latest_sequence,
            bytes: bytes.into(),
        };
        let retained_bytes = if live_chunk.bytes.len() > self.capacity_bytes {
            self.lost_through_sequence = live_chunk.sequence;
            Arc::from(&live_chunk.bytes[live_chunk.bytes.len() - self.capacity_bytes..])
        } else {
            live_chunk.bytes.clone()
        };
        self.retained_bytes += retained_bytes.len();
        self.chunks.push_back(OutputChunk {
            sequence: live_chunk.sequence,
            bytes: retained_bytes,
        });

        while self.retained_bytes > self.capacity_bytes || self.chunks.len() > MAX_SCROLLBACK_CHUNKS
        {
            if let Some(evicted) = self.chunks.pop_front() {
                self.retained_bytes -= evicted.bytes.len();
                self.lost_through_sequence = evicted.sequence;
            }
        }
        live_chunk
    }

    #[must_use]
    pub fn replay_after(&self, from_sequence: u64) -> Replay {
        let earliest_sequence = self.chunks.front().map(|chunk| chunk.sequence);
        let output_gap = from_sequence > self.latest_sequence
            || from_sequence < self.lost_through_sequence
            || earliest_sequence.is_some_and(|earliest| from_sequence.saturating_add(1) < earliest);
        let chunks = self
            .chunks
            .iter()
            .filter(|chunk| chunk.sequence > from_sequence)
            .cloned()
            .collect();
        Replay {
            earliest_sequence,
            latest_sequence: self.latest_sequence,
            output_gap,
            chunks,
            terminal_snapshot: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SessionHandle {
    commands: mpsc::Sender<Command>,
    live_output: broadcast::Sender<OutputChunk>,
    exit: watch::Receiver<Option<SessionExit>>,
    process_id: u32,
}

impl SessionHandle {
    /// Creates a PTY, launches the child with structured arguments, and starts its owner task.
    ///
    /// # Errors
    ///
    /// Returns an error when validation, PTY creation, process launch, process-tree ownership, or
    /// PTY handle cloning fails. A child that cannot be placed under process-tree control is killed
    /// and reaped before returning.
    pub fn spawn(spec: &SessionSpec) -> Result<Self> {
        Self::spawn_inner(spec, None)
    }

    /// Spawns a session after clearing the inherited environment and applying an explicit map.
    ///
    /// # Errors
    ///
    /// Returns the same validation and PTY ownership errors as [`Self::spawn`].
    pub fn spawn_sanitized(
        spec: &SessionSpec,
        environment: &BTreeMap<OsString, OsString>,
    ) -> Result<Self> {
        Self::spawn_inner(spec, Some(environment))
    }

    fn spawn_inner(
        spec: &SessionSpec,
        environment: Option<&BTreeMap<OsString, OsString>>,
    ) -> Result<Self> {
        spec.validate()?;
        let spawned = pty::spawn(spec, environment)?;
        let writer = spawn_writer(spawned.writer);

        let (commands, command_rx) = mpsc::channel(ACTOR_QUEUE_CAPACITY);
        let (actor_events, actor_event_rx) = mpsc::channel(ACTOR_QUEUE_CAPACITY);
        let (live_output, _) = broadcast::channel(LIVE_OUTPUT_CAPACITY);
        let (exit_tx, exit) = watch::channel(None);

        spawn_reader(spawned.reader, actor_events.clone());
        spawn_waiter(spawned.child, actor_events.clone());
        tokio::spawn(
            Actor {
                master: spawned.master,
                writer: Some(writer),
                process_tree: spawned.process_tree,
                scrollback: Scrollback::new(spec.scrollback_bytes),
                terminal: vt100::Parser::new(spec.rows, spec.columns, 0),
                columns: spec.columns,
                rows: spec.rows,
                controller: None,
                observers: HashSet::new(),
                stop_requested: false,
                stop_replies: Vec::new(),
                commands: command_rx,
                events: actor_event_rx,
                events_tx: actor_events,
                live_output: live_output.clone(),
                exit: exit_tx,
            }
            .run(),
        );

        Ok(Self {
            commands,
            live_output,
            exit,
            process_id: spawned.process_id,
        })
    }

    /// Attaches a client and atomically captures replay plus a live subscription.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid dimensions or a stopped session actor.
    pub async fn attach(
        &self,
        client_id: Uuid,
        from_sequence: u64,
        columns: u16,
        rows: u16,
    ) -> Result<Attachment> {
        validate_dimensions(columns, rows)?;
        let (reply, receive) = oneshot::channel();
        self.commands
            .send(Command::Attach {
                client_id,
                from_sequence,
                columns,
                rows,
                reply,
            })
            .await
            .map_err(|_| DaemonError::SessionStopped)?;
        receive.await.map_err(|_| DaemonError::RequestCancelled)?
    }

    /// Releases any controller or observer lease held by a client.
    ///
    /// # Errors
    ///
    /// Returns an error if the session actor has stopped.
    pub async fn detach(&self, client_id: Uuid) -> Result<()> {
        let (reply, receive) = oneshot::channel();
        self.commands
            .send(Command::Detach { client_id, reply })
            .await
            .map_err(|_| DaemonError::SessionStopped)?;
        receive.await.map_err(|_| DaemonError::RequestCancelled)
    }

    /// Sends bounded input on behalf of the current controller.
    ///
    /// # Errors
    ///
    /// Returns an error for oversized input, a non-controller client, or a PTY failure.
    pub async fn input_from(&self, client_id: Uuid, bytes: Vec<u8>) -> Result<()> {
        sylvops_core::protocol::validate_pty_chunk_length(bytes.len())
            .map_err(DaemonError::InvalidSession)?;
        let (reply, receive) = oneshot::channel();
        self.commands
            .send(Command::InputFrom {
                client_id,
                bytes,
                reply,
            })
            .await
            .map_err(|_| DaemonError::SessionStopped)?;
        receive.await.map_err(|_| DaemonError::RequestCancelled)?
    }

    /// Resizes the PTY on behalf of the current controller.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid dimensions, a non-controller client, or a PTY failure.
    pub async fn resize_from(&self, client_id: Uuid, columns: u16, rows: u16) -> Result<()> {
        validate_dimensions(columns, rows)?;
        let (reply, receive) = oneshot::channel();
        self.commands
            .send(Command::ResizeFrom {
                client_id,
                columns,
                rows,
                reply,
            })
            .await
            .map_err(|_| DaemonError::SessionStopped)?;
        receive.await.map_err(|_| DaemonError::RequestCancelled)?
    }

    #[must_use]
    pub const fn process_id(&self) -> u32 {
        self.process_id
    }

    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<OutputChunk> {
        self.live_output.subscribe()
    }

    /// Returns retained chunks newer than `sequence` and reports any eviction gap.
    ///
    /// # Errors
    ///
    /// Returns an error if the owning session actor has stopped or cancels the request.
    pub async fn replay_after(&self, sequence: u64) -> Result<Replay> {
        let (reply, receive) = oneshot::channel();
        self.commands
            .send(Command::Replay {
                from_sequence: sequence,
                reply,
            })
            .await
            .map_err(|_| DaemonError::SessionStopped)?;
        receive.await.map_err(|_| DaemonError::RequestCancelled)
    }

    /// Returns the parser-derived terminal state and its output sequence boundary.
    ///
    /// # Errors
    ///
    /// Returns an error if the session actor has stopped.
    pub async fn terminal_snapshot(&self) -> Result<(u64, u16, u16, Vec<u8>)> {
        let (reply, receive) = oneshot::channel();
        self.commands
            .send(Command::Snapshot { reply })
            .await
            .map_err(|_| DaemonError::SessionStopped)?;
        receive.await.map_err(|_| DaemonError::RequestCancelled)
    }

    /// Writes raw bytes to the PTY input stream.
    ///
    /// # Errors
    ///
    /// Returns an oversized-input, actor, cancellation, or PTY write error.
    pub async fn input(&self, bytes: Vec<u8>) -> Result<()> {
        sylvops_core::protocol::validate_pty_chunk_length(bytes.len())
            .map_err(DaemonError::InvalidSession)?;
        let (reply, receive) = oneshot::channel();
        self.commands
            .send(Command::Input { bytes, reply })
            .await
            .map_err(|_| DaemonError::SessionStopped)?;
        receive.await.map_err(|_| DaemonError::RequestCancelled)?
    }

    /// Resizes the PTY to non-zero character dimensions.
    ///
    /// # Errors
    ///
    /// Returns an error for zero dimensions, actor shutdown, cancellation, or PTY resize failure.
    pub async fn resize(&self, columns: u16, rows: u16) -> Result<()> {
        if columns == 0 || rows == 0 {
            return Err(DaemonError::InvalidSession(
                "terminal dimensions must be non-zero".into(),
            ));
        }
        let (reply, receive) = oneshot::channel();
        self.commands
            .send(Command::Resize {
                columns,
                rows,
                reply,
            })
            .await
            .map_err(|_| DaemonError::SessionStopped)?;
        receive.await.map_err(|_| DaemonError::RequestCancelled)?
    }

    /// Hard-stops the entire platform process tree.
    ///
    /// # Errors
    ///
    /// Returns an actor, cancellation, or operating-system process-tree error.
    pub async fn stop(&self) -> Result<()> {
        let (reply, receive) = oneshot::channel();
        self.commands
            .send(Command::Stop { reply })
            .await
            .map_err(|_| DaemonError::SessionStopped)?;
        receive.await.map_err(|_| DaemonError::RequestCancelled)?
    }

    /// Waits until the child has been reaped and returns its exit code.
    ///
    /// # Errors
    ///
    /// Returns an error if the owner task disappears without publishing an exit status.
    pub async fn wait(&mut self) -> Result<SessionExit> {
        loop {
            if let Some(exit) = self.exit.borrow().clone() {
                return Ok(exit);
            }
            self.exit
                .changed()
                .await
                .map_err(|_| DaemonError::SessionStopped)?;
        }
    }
}

#[derive(Debug)]
enum Command {
    Attach {
        client_id: Uuid,
        from_sequence: u64,
        columns: u16,
        rows: u16,
        reply: oneshot::Sender<Result<Attachment>>,
    },
    Detach {
        client_id: Uuid,
        reply: oneshot::Sender<()>,
    },
    InputFrom {
        client_id: Uuid,
        bytes: Vec<u8>,
        reply: oneshot::Sender<Result<()>>,
    },
    ResizeFrom {
        client_id: Uuid,
        columns: u16,
        rows: u16,
        reply: oneshot::Sender<Result<()>>,
    },
    Input {
        bytes: Vec<u8>,
        reply: oneshot::Sender<Result<()>>,
    },
    Resize {
        columns: u16,
        rows: u16,
        reply: oneshot::Sender<Result<()>>,
    },
    Replay {
        from_sequence: u64,
        reply: oneshot::Sender<Replay>,
    },
    Snapshot {
        reply: oneshot::Sender<(u64, u16, u16, Vec<u8>)>,
    },
    Stop {
        reply: oneshot::Sender<Result<()>>,
    },
}

#[derive(Debug)]
enum ActorEvent {
    Output(Vec<u8>),
    ReaderClosed,
    ReaderFailed(String),
    Exited(u32),
    ForceStop,
}

#[derive(Debug)]
enum WriterCommand {
    Write {
        bytes: Vec<u8>,
        reply: oneshot::Sender<Result<()>>,
    },
    Close,
}

struct Actor {
    master: Box<dyn pty::PtyMaster>,
    writer: Option<mpsc::Sender<WriterCommand>>,
    process_tree: process_tree::ProcessTree,
    scrollback: Scrollback,
    terminal: vt100::Parser,
    columns: u16,
    rows: u16,
    controller: Option<Uuid>,
    observers: HashSet<Uuid>,
    stop_requested: bool,
    stop_replies: Vec<oneshot::Sender<Result<()>>>,
    commands: mpsc::Receiver<Command>,
    events: mpsc::Receiver<ActorEvent>,
    events_tx: mpsc::Sender<ActorEvent>,
    live_output: broadcast::Sender<OutputChunk>,
    exit: watch::Sender<Option<SessionExit>>,
}

impl Actor {
    #[allow(clippy::too_many_lines)]
    async fn run(mut self) {
        let mut reader_closed = false;
        let mut pending_exit = None;
        let mut exit_published = false;
        loop {
            tokio::select! {
            command = self.commands.recv() => {
                let Some(command) = command else {
                    let _ = self.process_tree.terminate();
                    return;
                };
                match command {
                    Command::Attach { client_id, from_sequence, columns, rows, reply } => {
                        let role = if self.controller.is_none() || self.controller == Some(client_id) {
                            self.controller = Some(client_id);
                            sylvops_core::domain::AttachmentRole::Controller
                        } else {
                            self.observers.insert(client_id);
                            sylvops_core::domain::AttachmentRole::Observer
                        };
                        if let Some((_, was_attached)) = pending_exit.as_mut() {
                            *was_attached = true;
                        }
                        let resize_result = if role == sylvops_core::domain::AttachmentRole::Controller {
                            self.resize_terminal(columns, rows)
                        } else {
                            Ok(())
                        };
                        let result = match resize_result {
                            Ok(()) => {
                                let mut replay = self.scrollback.replay_after(from_sequence);
                                if replay.output_gap {
                                    replay.chunks.clear();
                                    replay.terminal_snapshot = Some(terminal_snapshot(&self.terminal));
                                }
                                Ok(Attachment {
                                    role,
                                    replay,
                                    live_output: self.live_output.subscribe(),
                                    completion_published: exit_published,
                                })
                            }
                            Err(error) => {
                                if self.controller == Some(client_id) {
                                    self.controller = None;
                                }
                                self.observers.remove(&client_id);
                                Err(error)
                            }
                        };
                        let _ = reply.send(result);
                    }
                    Command::Detach { client_id, reply } => {
                        if self.controller == Some(client_id) {
                            self.controller = None;
                        }
                        self.observers.remove(&client_id);
                        let _ = reply.send(());
                    }
                    Command::InputFrom { client_id, bytes, reply } => {
                        if self.controller == Some(client_id) {
                            self.queue_input(bytes, reply);
                        } else {
                            let _ = reply.send(Err(DaemonError::Attachment(
                                "only the attached controller may send input".into(),
                            )));
                        }
                    }
                    Command::ResizeFrom { client_id, columns, rows, reply } => {
                        let result = if self.controller == Some(client_id) {
                            self.resize_terminal(columns, rows)
                        } else {
                            Err(DaemonError::Attachment("only the attached controller may resize".into()))
                        };
                        let _ = reply.send(result);
                    }
                    Command::Input { bytes, reply } => {
                        self.queue_input(bytes, reply);
                    }
                    Command::Resize { columns, rows, reply } => {
                        let result = self.resize_terminal(columns, rows);
                        let _ = reply.send(result);
                    }
                    Command::Replay { from_sequence, reply } => {
                        let _ = reply.send(self.scrollback.replay_after(from_sequence));
                    }
                    Command::Snapshot { reply } => {
                        let _ = reply.send((
                            self.scrollback.latest_sequence,
                            self.columns,
                            self.rows,
                            terminal_snapshot(&self.terminal),
                        ));
                    }
                    Command::Stop { reply } => {
                        if exit_published {
                            let _ = reply.send(Ok(()));
                            continue;
                        }
                        if self.stop_requested {
                            self.stop_replies.push(reply);
                        } else {
                            self.stop_requested = true;
                            if let Some(writer) = self.writer.take() {
                                let _ = writer.try_send(WriterCommand::Close);
                            }
                            self.stop_replies.push(reply);
                            let events = self.events_tx.clone();
                            tokio::spawn(async move {
                                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                                let _ = events.send(ActorEvent::ForceStop).await;
                            });
                        }
                    }
                }
            }
            event = self.events.recv() => {
                let Some(event) = event else { return; };
                match event {
                    ActorEvent::Output(bytes) => {
                        self.terminal.process(&bytes);
                        let chunk = self.scrollback.push(Arc::<[u8]>::from(bytes));
                        let _ = self.live_output.send(chunk);
                    }
                    ActorEvent::ReaderClosed => {
                        reader_closed = true;
                    }
                    ActorEvent::ReaderFailed(error) => {
                        tracing::warn!(error, "PTY reader stopped with an error");
                        reader_closed = true;
                    }
                    ActorEvent::Exited(exit_code) => {
                        if let Some(writer) = self.writer.take() {
                            let _ = writer.try_send(WriterCommand::Close);
                        }
                        pending_exit = Some((
                            exit_code,
                            self.controller.is_some() || !self.observers.is_empty(),
                        ));
                        for reply in self.stop_replies.drain(..) {
                            let _ = reply.send(Ok(()));
                        }
                    }
                    ActorEvent::ForceStop => {
                        if pending_exit.is_none()
                            && let Err(error) = self.process_tree.terminate()
                        {
                            for reply in self.stop_replies.drain(..) {
                                let _ = reply.send(Err(DaemonError::ProcessTree(
                                    error.to_string(),
                                )));
                            }
                        }
                    }
                }
            }
            }
            if !exit_published
                && (reader_closed || self.stop_requested)
                && let Some((exit_code, was_attached)) = pending_exit
            {
                self.exit.send_replace(Some(SessionExit {
                    exit_code,
                    stop_requested: self.stop_requested,
                    was_attached,
                }));
                exit_published = true;
                // Windows ConPTY readers do not consistently report EOF after a Job Object
                // terminates the child. Once an explicit stop has reaped that child, returning
                // drops the PTY master and releases the reader instead of waiting indefinitely.
                if self.stop_requested {
                    return;
                }
            }
        }
    }
}

impl Actor {
    fn queue_input(&self, bytes: Vec<u8>, reply: oneshot::Sender<Result<()>>) {
        let Some(writer) = &self.writer else {
            let _ = reply.send(Err(DaemonError::Pty("PTY input is closed".into())));
            return;
        };
        if let Err(error) = writer.try_send(WriterCommand::Write { bytes, reply }) {
            let WriterCommand::Write { reply, .. } = error.into_inner() else {
                unreachable!("only write commands are queued here");
            };
            let _ = reply.send(Err(DaemonError::Pty(
                "PTY writer is unavailable or backpressured".into(),
            )));
        }
    }

    fn resize_terminal(&mut self, columns: u16, rows: u16) -> Result<()> {
        validate_dimensions(columns, rows)?;
        self.master.resize(columns, rows)?;
        self.terminal.screen_mut().set_size(rows, columns);
        self.columns = columns;
        self.rows = rows;
        Ok(())
    }
}

fn spawn_writer(mut writer: Box<dyn Write + Send>) -> mpsc::Sender<WriterCommand> {
    let (commands, mut receiver) = mpsc::channel(ACTOR_QUEUE_CAPACITY);
    tokio::task::spawn_blocking(move || {
        while let Some(command) = receiver.blocking_recv() {
            match command {
                WriterCommand::Write { bytes, reply } => {
                    let result = writer
                        .write_all(&bytes)
                        .and_then(|()| writer.flush())
                        .map_err(|error| DaemonError::Pty(error.to_string()));
                    let _ = reply.send(result);
                }
                WriterCommand::Close => return,
            }
        }
    });
    commands
}

fn validate_dimensions(columns: u16, rows: u16) -> Result<()> {
    sylvops_core::protocol::validate_terminal_size(columns, rows)
        .map_err(DaemonError::InvalidSession)
}

fn terminal_snapshot(parser: &vt100::Parser) -> Vec<u8> {
    let formatted = parser.screen().contents_formatted();
    if formatted.len() <= 512 * 1024 {
        formatted
    } else {
        parser.screen().contents().into_bytes()
    }
}

fn spawn_reader(mut reader: Box<dyn Read + Send>, events: mpsc::Sender<ActorEvent>) {
    tokio::task::spawn_blocking(move || {
        let mut buffer = vec![0_u8; READ_CHUNK_SIZE];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => {
                    let _ = events.blocking_send(ActorEvent::ReaderClosed);
                    return;
                }
                Ok(read) => {
                    if events
                        .blocking_send(ActorEvent::Output(buffer[..read].to_vec()))
                        .is_err()
                    {
                        return;
                    }
                }
                Err(error) => {
                    let _ = events.blocking_send(ActorEvent::ReaderFailed(error.to_string()));
                    return;
                }
            }
        }
    });
}

fn spawn_waiter(mut child: Box<dyn pty::PtyChild>, events: mpsc::Sender<ActorEvent>) {
    tokio::task::spawn_blocking(move || {
        let result = child.wait();
        let exit_code = match result {
            Ok(exit_code) => exit_code,
            Err(error) => {
                tracing::warn!(error = %error, "waiting for PTY child failed");
                1
            }
        };
        let _ = events.blocking_send(ActorEvent::Exited(exit_code));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn diagnostic_probe_spawns_outputs_and_reaps() {
        run_pty_probe().await.unwrap();
    }

    #[test]
    fn scrollback_is_bounded_and_reports_a_gap() {
        let mut buffer = Scrollback::new(6);
        buffer.push(Arc::<[u8]>::from(&b"abc"[..]));
        buffer.push(Arc::<[u8]>::from(&b"def"[..]));
        buffer.push(Arc::<[u8]>::from(&b"ghi"[..]));

        let replay = buffer.replay_after(0);
        assert!(replay.output_gap);
        assert_eq!(replay.earliest_sequence, Some(2));
        assert_eq!(replay.chunks.len(), 2);
        assert_eq!(replay.latest_sequence, 3);
    }

    #[test]
    fn duplicate_boundary_is_not_replayed() {
        let mut buffer = Scrollback::new(32);
        buffer.push(Arc::<[u8]>::from(&b"one"[..]));
        buffer.push(Arc::<[u8]>::from(&b"two"[..]));

        let replay = buffer.replay_after(1);
        assert_eq!(replay.chunks.len(), 1);
        assert_eq!(replay.chunks[0].sequence, 2);
    }

    #[test]
    fn future_sequence_forces_a_resynchronization_boundary() {
        let mut buffer = Scrollback::new(32);
        buffer.push(Arc::<[u8]>::from(&b"one"[..]));

        assert!(buffer.replay_after(u64::MAX).output_gap);
    }

    #[test]
    fn oversized_single_chunk_requires_parser_resynchronization() {
        let mut buffer = Scrollback::new(4);
        let live = buffer.push(Arc::<[u8]>::from(&b"abcdefgh"[..]));

        assert_eq!(&*live.bytes, b"abcdefgh");
        let replay = buffer.replay_after(0);
        assert!(replay.output_gap);
        assert_eq!(&*replay.chunks[0].bytes, b"efgh");
    }

    #[test]
    fn scrollback_is_also_bounded_by_chunk_count() {
        let mut buffer = Scrollback::new(MAX_SCROLLBACK_CHUNKS * 2);
        for _ in 0..=MAX_SCROLLBACK_CHUNKS {
            buffer.push(Arc::<[u8]>::from(&b"x"[..]));
        }

        let replay = buffer.replay_after(0);
        assert!(replay.output_gap);
        assert_eq!(replay.chunks.len(), MAX_SCROLLBACK_CHUNKS);
    }

    #[test]
    fn parser_snapshot_handles_split_utf8_and_filters_osc() {
        let mut parser = vt100::Parser::new(24, 80, 0);
        let glyph = "λ".as_bytes();
        parser.process(&glyph[..1]);
        parser.process(&glyph[1..]);
        parser.process(b"\x1b]52;c;UNTRUSTED_CLIPBOARD\x07safe");

        let snapshot = terminal_snapshot(&parser);
        let snapshot = String::from_utf8_lossy(&snapshot);
        assert!(snapshot.contains('λ'));
        assert!(snapshot.contains("safe"));
        assert!(!snapshot.contains("UNTRUSTED_CLIPBOARD"));
        assert!(!snapshot.contains("]52"));
    }

    #[test]
    fn parser_snapshot_tracks_alternate_screen_transitions() {
        let mut parser = vt100::Parser::new(24, 80, 0);
        parser.process(b"main\x1b[?1049halt\x1b[?1049l");

        let snapshot = String::from_utf8_lossy(&terminal_snapshot(&parser)).into_owned();
        assert!(snapshot.contains("main"));
        assert!(!snapshot.contains("alt"));
    }
}
