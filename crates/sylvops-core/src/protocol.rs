//! Length-prefixed, versioned `MessagePack` protocol primitives.

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use uuid::Uuid;

use crate::{
    CoreError, Result,
    domain::{
        AttachmentRole, DaemonHealth, DaemonSnapshot, GitDiff, GitWorktreeState, Project,
        ProviderKind, Session, SessionState, Workspace, Worktree,
    },
    ids::{ProjectId, SessionId, WorkspaceId, WorktreeId},
    provider::ProviderHealth,
    status::NormalizedProviderEvent,
};

pub const MAGIC: u32 = u32::from_be_bytes(*b"CSTL");
pub const PROTOCOL_MAJOR: u16 = 1;
pub const PROTOCOL_MINOR: u16 = 7;
pub const MAX_FRAME_SIZE: usize = 1024 * 1024;
pub const MAX_PTY_CHUNK_SIZE: usize = 64 * 1024;
pub const MIN_TERMINAL_COLUMNS: u16 = 1;
pub const MAX_TERMINAL_COLUMNS: u16 = 500;
pub const MIN_TERMINAL_ROWS: u16 = 1;
pub const MAX_TERMINAL_ROWS: u16 = 200;
const HEADER_SIZE: usize = 48;
const LENGTH_PREFIX_SIZE: usize = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum MessageClass {
    Request = 1,
    Response = 2,
    Event = 3,
}

impl TryFrom<u8> for MessageClass {
    type Error = CoreError;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::Request),
            2 => Ok(Self::Response),
            3 => Ok(Self::Event),
            other => Err(CoreError::UnknownMessageClass(other)),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProtocolVersion {
    pub major: u16,
    pub minor: u16,
}

impl ProtocolVersion {
    pub const CURRENT: Self = Self {
        major: PROTOCOL_MAJOR,
        minor: PROTOCOL_MINOR,
    };

    /// Validates compatibility with the current implementation.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::UnsupportedProtocol`] unless both major and minor exactly match.
    pub fn validate(self) -> Result<()> {
        if self.major == PROTOCOL_MAJOR && self.minor == PROTOCOL_MINOR {
            Ok(())
        } else {
            Err(CoreError::UnsupportedProtocol {
                major: self.major,
                minor: self.minor,
            })
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Frame {
    pub version: ProtocolVersion,
    pub class: MessageClass,
    pub opcode: u16,
    pub flags: u8,
    pub message_id: Uuid,
    pub correlation_id: Option<Uuid>,
    pub payload: Vec<u8>,
}

impl Frame {
    /// Serializes a typed payload into a new frame.
    ///
    /// # Errors
    ///
    /// Returns an error when `message` cannot be serialized.
    pub fn message<T: Serialize>(class: MessageClass, opcode: u16, message: &T) -> Result<Self> {
        Ok(Self {
            version: ProtocolVersion::CURRENT,
            class,
            opcode,
            flags: 0,
            message_id: Uuid::now_v7(),
            correlation_id: None,
            payload: rmp_serde::to_vec_named(message)?,
        })
    }

    /// Serializes a response correlated to `request_id`.
    ///
    /// # Errors
    ///
    /// Returns an error when `message` cannot be serialized.
    pub fn response<T: Serialize>(opcode: u16, request_id: Uuid, message: &T) -> Result<Self> {
        let mut frame = Self::message(MessageClass::Response, opcode, message)?;
        frame.correlation_id = Some(request_id);
        Ok(frame)
    }

    /// Deserializes the typed payload.
    ///
    /// # Errors
    ///
    /// Returns an error when the payload is not a valid encoded `T`.
    pub fn payload_as<T: DeserializeOwned>(&self) -> Result<T> {
        Ok(rmp_serde::from_slice(&self.payload)?)
    }

    /// Encodes this frame, including its length prefix and fixed header.
    ///
    /// # Errors
    ///
    /// Returns an error for a nil message ID or a frame larger than [`MAX_FRAME_SIZE`].
    pub fn encode(&self) -> Result<Vec<u8>> {
        if self.message_id.is_nil() {
            return Err(CoreError::NilMessageId);
        }
        if self.flags != 0 {
            return Err(CoreError::UnsupportedFrameFlags(self.flags));
        }

        let frame_length =
            HEADER_SIZE
                .checked_add(self.payload.len())
                .ok_or(CoreError::FrameTooLarge {
                    actual: usize::MAX,
                    maximum: MAX_FRAME_SIZE,
                })?;
        if frame_length > MAX_FRAME_SIZE {
            return Err(CoreError::FrameTooLarge {
                actual: frame_length,
                maximum: MAX_FRAME_SIZE,
            });
        }

        let frame_length_u32 =
            u32::try_from(frame_length).map_err(|_| CoreError::FrameTooLarge {
                actual: frame_length,
                maximum: MAX_FRAME_SIZE,
            })?;
        let payload_length =
            u32::try_from(self.payload.len()).map_err(|_| CoreError::FrameTooLarge {
                actual: self.payload.len(),
                maximum: MAX_FRAME_SIZE - HEADER_SIZE,
            })?;

        let mut bytes = Vec::with_capacity(LENGTH_PREFIX_SIZE + frame_length);
        bytes.extend_from_slice(&frame_length_u32.to_be_bytes());
        bytes.extend_from_slice(&MAGIC.to_be_bytes());
        bytes.extend_from_slice(&self.version.major.to_be_bytes());
        bytes.extend_from_slice(&self.version.minor.to_be_bytes());
        bytes.push(self.class as u8);
        bytes.extend_from_slice(&self.opcode.to_be_bytes());
        bytes.push(self.flags);
        bytes.extend_from_slice(self.message_id.as_bytes());
        bytes.extend_from_slice(self.correlation_id.unwrap_or_else(Uuid::nil).as_bytes());
        bytes.extend_from_slice(&payload_length.to_be_bytes());
        bytes.extend_from_slice(&self.payload);
        Ok(bytes)
    }

    /// Validates and decodes one complete frame.
    ///
    /// # Errors
    ///
    /// Returns an error for truncation, inconsistent lengths, invalid identifiers, unsupported
    /// versions, unknown message classes, or oversized frames.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < LENGTH_PREFIX_SIZE {
            return Err(CoreError::TruncatedFrame);
        }

        let prefix: [u8; 4] = bytes
            .get(..4)
            .ok_or(CoreError::TruncatedFrame)?
            .try_into()
            .map_err(|_| CoreError::TruncatedFrame)?;
        let declared =
            usize::try_from(u32::from_be_bytes(prefix)).map_err(|_| CoreError::FrameTooLarge {
                actual: usize::MAX,
                maximum: MAX_FRAME_SIZE,
            })?;
        if declared > MAX_FRAME_SIZE {
            return Err(CoreError::FrameTooLarge {
                actual: declared,
                maximum: MAX_FRAME_SIZE,
            });
        }
        if bytes.len() < LENGTH_PREFIX_SIZE + HEADER_SIZE {
            return Err(CoreError::TruncatedFrame);
        }
        if bytes.len() != LENGTH_PREFIX_SIZE + declared {
            return Err(CoreError::InvalidFrameLength);
        }

        Self::decode_body(&bytes[LENGTH_PREFIX_SIZE..])
    }

    fn decode_body(body: &[u8]) -> Result<Self> {
        if body.len() < HEADER_SIZE {
            return Err(CoreError::TruncatedFrame);
        }
        let mut position = 0;
        let magic = take_u32(body, &mut position)?;
        if magic != MAGIC {
            return Err(CoreError::InvalidMagic);
        }

        let version = ProtocolVersion {
            major: take_u16(body, &mut position)?,
            minor: take_u16(body, &mut position)?,
        };
        version.validate()?;
        let class = MessageClass::try_from(take_u8(body, &mut position)?)?;
        let opcode = take_u16(body, &mut position)?;
        let flags = take_u8(body, &mut position)?;
        if flags != 0 {
            return Err(CoreError::UnsupportedFrameFlags(flags));
        }
        let message_id = take_uuid(body, &mut position)?;
        if message_id.is_nil() {
            return Err(CoreError::NilMessageId);
        }
        let correlation = take_uuid(body, &mut position)?;
        let payload_length = usize::try_from(take_u32(body, &mut position)?).map_err(|_| {
            CoreError::FrameTooLarge {
                actual: usize::MAX,
                maximum: MAX_FRAME_SIZE,
            }
        })?;
        if HEADER_SIZE + payload_length != body.len() {
            return Err(CoreError::InvalidFrameLength);
        }
        let payload = body[HEADER_SIZE..].to_vec();

        Ok(Self {
            version,
            class,
            opcode,
            flags,
            message_id,
            correlation_id: (!correlation.is_nil()).then_some(correlation),
            payload,
        })
    }
}

/// Reads and validates one frame from an asynchronous byte stream.
///
/// # Errors
///
/// Returns an I/O or frame-validation error. Oversized lengths are rejected before allocation.
pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Frame> {
    let declared_u32 = reader.read_u32().await?;
    let declared = usize::try_from(declared_u32).map_err(|_| CoreError::FrameTooLarge {
        actual: usize::MAX,
        maximum: MAX_FRAME_SIZE,
    })?;
    if declared > MAX_FRAME_SIZE {
        return Err(CoreError::FrameTooLarge {
            actual: declared,
            maximum: MAX_FRAME_SIZE,
        });
    }
    if declared < HEADER_SIZE {
        return Err(CoreError::TruncatedFrame);
    }
    let mut bytes = vec![0_u8; LENGTH_PREFIX_SIZE + declared];
    bytes[..4].copy_from_slice(&declared_u32.to_be_bytes());
    reader.read_exact(&mut bytes[4..]).await?;
    Frame::decode(&bytes)
}

/// Encodes and writes one complete frame to an asynchronous byte stream.
///
/// # Errors
///
/// Returns a frame-validation or I/O error.
pub async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, frame: &Frame) -> Result<()> {
    writer.write_all(&frame.encode()?).await?;
    writer.flush().await?;
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PhaseZeroRequest {
    Hello { client_name: String },
    Health,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PhaseZeroResponse {
    Welcome { server_name: String },
    Healthy { protocol: String },
    Error { code: String, message: String },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HelloRequest {
    pub client_name: String,
    pub client_version: String,
    pub protocol_major: u16,
    pub protocol_minor: u16,
    pub authentication_token: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WelcomeResponse {
    pub daemon_version: String,
    pub protocol_major: u16,
    pub protocol_minor: u16,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProtocolFailure {
    pub code: String,
    pub message: String,
    pub retryable: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ClientRequest {
    Hello(HelloRequest),
    Health,
    GetSnapshot,
    GetTuiState,
    SaveTuiState {
        state: crate::ui::TuiState,
    },
    GetDesktopState,
    SaveDesktopState {
        state: crate::ui::DesktopState,
    },
    ListProviders,
    ProbeProvider {
        kind: ProviderKind,
    },
    AddWorkspace {
        name: String,
    },
    OpenWorkspace {
        workspace_id: WorkspaceId,
    },
    AddProject {
        workspace_id: WorkspaceId,
        repository_path: String,
    },
    EnsureProject {
        workspace_id: WorkspaceId,
        repository_path: String,
    },
    RenameProject {
        project_id: ProjectId,
        name: String,
    },
    CreateWorktree {
        project_id: ProjectId,
        name: Option<String>,
        branch: String,
        base_ref: Option<String>,
    },
    GetWorktreeStatus {
        worktree_id: WorktreeId,
    },
    RemoveWorktree {
        worktree_id: WorktreeId,
        confirmation_token: String,
    },
    RenameWorktree {
        worktree_id: WorktreeId,
        name: String,
    },
    CreateSession {
        worktree_id: WorktreeId,
        provider: ProviderKind,
        display_name: Option<String>,
        model: Option<String>,
        effort: Option<String>,
        initial_prompt: Option<String>,
        columns: u16,
        rows: u16,
    },
    ResumeSession {
        session_id: SessionId,
        columns: u16,
        rows: u16,
    },
    AttachSession {
        session_id: SessionId,
        from_sequence: u64,
        columns: u16,
        rows: u16,
    },
    DetachSession {
        session_id: SessionId,
    },
    SessionInput {
        session_id: SessionId,
        bytes: Vec<u8>,
    },
    ResizeSession {
        session_id: SessionId,
        columns: u16,
        rows: u16,
    },
    StopSession {
        session_id: SessionId,
    },
    RenameSession {
        session_id: SessionId,
        name: String,
    },
    GetDiff {
        worktree_id: WorktreeId,
    },
    ShutdownDaemon,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum DaemonResponse {
    Welcome(WelcomeResponse),
    Health(DaemonHealth),
    Snapshot(DaemonSnapshot),
    TuiState(Option<crate::ui::TuiState>),
    TuiStateSaved,
    DesktopState(Option<crate::ui::DesktopState>),
    DesktopStateSaved,
    Providers(Vec<ProviderHealth>),
    Provider(ProviderHealth),
    WorkspaceAdded {
        revision: u64,
        workspace: Workspace,
    },
    WorkspaceOpened {
        revision: u64,
        workspace: Workspace,
    },
    ProjectAdded {
        revision: u64,
        project: Project,
        root_worktree: Worktree,
    },
    ProjectReady {
        revision: u64,
        project: Project,
        root_worktree: Worktree,
        created: bool,
    },
    ProjectUpdated {
        revision: u64,
        project: Project,
    },
    WorktreeCreated {
        revision: u64,
        worktree: Worktree,
    },
    WorktreeStatus(GitWorktreeState),
    Diff(GitDiff),
    WorktreeRemoved {
        revision: u64,
        worktree: Worktree,
    },
    WorktreeUpdated {
        revision: u64,
        worktree: Worktree,
    },
    SessionCreated {
        revision: u64,
        session: Session,
    },
    SessionResumed {
        revision: u64,
        session: Session,
    },
    SessionUpdated {
        revision: u64,
        session: Session,
    },
    Attached {
        session: Session,
        role: AttachmentRole,
        earliest_sequence: Option<u64>,
        replay_through_sequence: u64,
        output_gap: bool,
        terminal_snapshot: Option<Vec<u8>>,
    },
    Acknowledged,
    Error(ProtocolFailure),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum DaemonEvent {
    WorkspaceAdded {
        revision: u64,
        workspace: Workspace,
    },
    WorkspaceOpened {
        revision: u64,
        workspace: Workspace,
    },
    ProjectAdded {
        revision: u64,
        project: Project,
        root_worktree: Worktree,
    },
    ProjectUpdated {
        revision: u64,
        project: Project,
    },
    WorktreeAdded {
        revision: u64,
        worktree: Worktree,
    },
    GitStateChanged {
        worktree: GitWorktreeState,
    },
    WorktreeRemoved {
        revision: u64,
        worktree: Worktree,
    },
    WorktreeUpdated {
        revision: u64,
        worktree: Worktree,
    },
    SessionCreated {
        revision: u64,
        session: Session,
    },
    SessionUpdated {
        revision: u64,
        session: Session,
    },
    ProviderHealthChanged {
        health: ProviderHealth,
    },
    ProviderEvent {
        session_id: SessionId,
        event: NormalizedProviderEvent,
    },
    SessionOutput {
        session_id: SessionId,
        sequence: u64,
        bytes: Vec<u8>,
        replay: bool,
    },
    SessionStatusChanged {
        revision: u64,
        session: Session,
    },
    SessionExited {
        revision: u64,
        session_id: SessionId,
        state: SessionState,
        exit_code: Option<i32>,
    },
    StateResynchronizationRequired {
        latest_revision: u64,
    },
    ResynchronizationRequired {
        session_id: SessionId,
        snapshot_sequence: u64,
        columns: u16,
        rows: u16,
        terminal_snapshot: Vec<u8>,
    },
}

/// Validates a terminal size supplied over IPC.
///
/// # Errors
///
/// Returns a descriptive error when either dimension is outside the protocol bounds.
pub fn validate_terminal_size(columns: u16, rows: u16) -> std::result::Result<(), String> {
    if !(MIN_TERMINAL_COLUMNS..=MAX_TERMINAL_COLUMNS).contains(&columns) {
        return Err(format!(
            "terminal columns must be between {MIN_TERMINAL_COLUMNS} and {MAX_TERMINAL_COLUMNS}"
        ));
    }
    if !(MIN_TERMINAL_ROWS..=MAX_TERMINAL_ROWS).contains(&rows) {
        return Err(format!(
            "terminal rows must be between {MIN_TERMINAL_ROWS} and {MAX_TERMINAL_ROWS}"
        ));
    }
    Ok(())
}

/// Validates a PTY input or output chunk length supplied at a protocol boundary.
///
/// # Errors
///
/// Returns a descriptive error when the chunk exceeds [`MAX_PTY_CHUNK_SIZE`].
pub fn validate_pty_chunk_length(length: usize) -> std::result::Result<(), String> {
    if length > MAX_PTY_CHUNK_SIZE {
        return Err(format!(
            "PTY chunk exceeds the {MAX_PTY_CHUNK_SIZE}-byte protocol limit"
        ));
    }
    Ok(())
}

fn take_u8(bytes: &[u8], position: &mut usize) -> Result<u8> {
    let value = *bytes.get(*position).ok_or(CoreError::TruncatedFrame)?;
    *position += 1;
    Ok(value)
}

fn take_u16(bytes: &[u8], position: &mut usize) -> Result<u16> {
    let value = bytes
        .get(*position..*position + 2)
        .ok_or(CoreError::TruncatedFrame)?;
    *position += 2;
    Ok(u16::from_be_bytes(
        value.try_into().map_err(|_| CoreError::TruncatedFrame)?,
    ))
}

fn take_u32(bytes: &[u8], position: &mut usize) -> Result<u32> {
    let value = bytes
        .get(*position..*position + 4)
        .ok_or(CoreError::TruncatedFrame)?;
    *position += 4;
    Ok(u32::from_be_bytes(
        value.try_into().map_err(|_| CoreError::TruncatedFrame)?,
    ))
}

fn take_uuid(bytes: &[u8], position: &mut usize) -> Result<Uuid> {
    let value = bytes
        .get(*position..*position + 16)
        .ok_or(CoreError::TruncatedFrame)?;
    *position += 16;
    Uuid::from_slice(value).map_err(|_| CoreError::TruncatedFrame)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::{MainTab, TuiState};

    #[test]
    fn messagepack_frame_round_trips() {
        let request = PhaseZeroRequest::Hello {
            client_name: "test-client".into(),
        };
        let frame = Frame::message(MessageClass::Request, 1, &request).expect("create frame");
        let decoded = Frame::decode(&frame.encode().expect("encode")).expect("decode");

        assert_eq!(decoded.class, MessageClass::Request);
        assert_eq!(decoded.opcode, 1);
        assert_eq!(decoded.payload_as::<PhaseZeroRequest>().unwrap(), request);
    }

    #[test]
    fn protocol_1_7_ui_state_round_trips() {
        let request = ClientRequest::SaveTuiState {
            state: TuiState {
                selected_project_id: Some(crate::ids::ProjectId::new()),
                selected_worktree_id: Some(crate::ids::WorktreeId::new()),
                selected_session_id: Some(crate::ids::SessionId::new()),
                selected_main_tab: MainTab::Details,
            },
        };
        let frame = Frame::message(MessageClass::Request, 10, &request).unwrap();
        let decoded = Frame::decode(&frame.encode().unwrap()).unwrap();
        assert_eq!(decoded.payload_as::<ClientRequest>().unwrap(), request);
        assert_eq!(PROTOCOL_MINOR, 7);

        let desktop = ClientRequest::SaveDesktopState {
            state: crate::ui::DesktopState::default(),
        };
        let frame = Frame::message(MessageClass::Request, 10, &desktop).unwrap();
        assert_eq!(frame.payload_as::<ClientRequest>().unwrap(), desktop);
    }

    #[test]
    fn rejects_unsupported_protocol_version() {
        let mut frame = Frame::message(MessageClass::Request, 1, &PhaseZeroRequest::Health)
            .unwrap()
            .encode()
            .unwrap();
        frame[8..10].copy_from_slice(&(PROTOCOL_MAJOR + 1).to_be_bytes());

        assert!(matches!(
            Frame::decode(&frame),
            Err(CoreError::UnsupportedProtocol { .. })
        ));

        let mut older_minor = Frame::message(MessageClass::Request, 1, &PhaseZeroRequest::Health)
            .unwrap()
            .encode()
            .unwrap();
        older_minor[10..12].copy_from_slice(&(PROTOCOL_MINOR - 1).to_be_bytes());
        assert!(matches!(
            Frame::decode(&older_minor),
            Err(CoreError::UnsupportedProtocol { .. })
        ));
    }

    #[test]
    fn rejects_oversized_declared_frame_before_allocation() {
        let declared = u32::try_from(MAX_FRAME_SIZE + 1).unwrap();
        let bytes = declared.to_be_bytes();

        assert!(matches!(
            Frame::decode(&bytes),
            Err(CoreError::FrameTooLarge { .. })
        ));
    }

    #[test]
    fn rejects_unknown_frame_flags() {
        let mut frame = Frame::message(MessageClass::Request, 1, &PhaseZeroRequest::Health)
            .unwrap()
            .encode()
            .unwrap();
        frame[15] = 1;

        assert!(matches!(
            Frame::decode(&frame),
            Err(CoreError::UnsupportedFrameFlags(1))
        ));
    }

    #[tokio::test]
    async fn async_codec_round_trips() {
        let (mut writer, mut reader) = tokio::io::duplex(2048);
        let frame = Frame::message(MessageClass::Request, 2, &PhaseZeroRequest::Health).unwrap();
        let expected_id = frame.message_id;

        let write = tokio::spawn(async move { write_frame(&mut writer, &frame).await });
        let decoded = read_frame(&mut reader).await.unwrap();
        write.await.unwrap().unwrap();

        assert_eq!(decoded.message_id, expected_id);
    }

    #[test]
    fn phase_two_request_round_trips() {
        let request = ClientRequest::CreateSession {
            worktree_id: WorktreeId::new(),
            provider: ProviderKind::Shell,
            display_name: Some("shell".into()),
            model: None,
            effort: None,
            initial_prompt: None,
            columns: 120,
            rows: 40,
        };
        let frame = Frame::message(MessageClass::Request, 10, &request).unwrap();
        assert_eq!(frame.payload_as::<ClientRequest>().unwrap(), request);
    }

    #[test]
    fn phase_three_worktree_requests_round_trip() {
        let requests = [
            ClientRequest::CreateWorktree {
                project_id: ProjectId::new(),
                name: Some("feature checkout".into()),
                branch: "feature/protocol".into(),
                base_ref: Some("main".into()),
            },
            ClientRequest::GetWorktreeStatus {
                worktree_id: WorktreeId::new(),
            },
            ClientRequest::RemoveWorktree {
                worktree_id: WorktreeId::new(),
                confirmation_token: "state-bound-token".into(),
            },
        ];

        for request in requests {
            let frame = Frame::message(MessageClass::Request, 10, &request).unwrap();
            assert_eq!(frame.payload_as::<ClientRequest>().unwrap(), request);
        }
    }

    #[test]
    fn mvp_provider_and_review_requests_round_trip() {
        let requests = [
            ClientRequest::ListProviders,
            ClientRequest::ProbeProvider {
                kind: ProviderKind::Codex,
            },
            ClientRequest::ResumeSession {
                session_id: SessionId::new(),
                columns: 100,
                rows: 30,
            },
            ClientRequest::GetDiff {
                worktree_id: WorktreeId::new(),
            },
        ];

        for request in requests {
            let frame = Frame::message(MessageClass::Request, 10, &request).unwrap();
            assert_eq!(frame.payload_as::<ClientRequest>().unwrap(), request);
        }
    }

    #[test]
    fn beta_management_requests_round_trip() {
        let requests = [
            ClientRequest::OpenWorkspace {
                workspace_id: WorkspaceId::new(),
            },
            ClientRequest::EnsureProject {
                workspace_id: WorkspaceId::new(),
                repository_path: "/tmp/repository".into(),
            },
            ClientRequest::RenameProject {
                project_id: ProjectId::new(),
                name: "renamed project".into(),
            },
            ClientRequest::RenameWorktree {
                worktree_id: WorktreeId::new(),
                name: "renamed worktree".into(),
            },
            ClientRequest::RenameSession {
                session_id: SessionId::new(),
                name: "renamed session".into(),
            },
        ];

        for request in requests {
            let frame = Frame::message(MessageClass::Request, 10, &request).unwrap();
            assert_eq!(frame.payload_as::<ClientRequest>().unwrap(), request);
        }
    }

    #[test]
    fn terminal_dimensions_are_bounded() {
        assert!(validate_terminal_size(1, 1).is_ok());
        assert!(validate_terminal_size(500, 200).is_ok());
        assert!(validate_terminal_size(0, 24).is_err());
        assert!(validate_terminal_size(80, 201).is_err());
    }

    #[test]
    fn pty_chunks_are_bounded() {
        assert!(validate_pty_chunk_length(MAX_PTY_CHUNK_SIZE).is_ok());
        assert!(validate_pty_chunk_length(MAX_PTY_CHUNK_SIZE + 1).is_err());
    }
}
