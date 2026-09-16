use thiserror::Error;

/// Errors produced while validating or decoding shared data.
#[derive(Debug, Error)]
pub enum CoreError {
    #[error("frame exceeds the maximum size of {maximum} bytes: {actual} bytes")]
    FrameTooLarge { actual: usize, maximum: usize },
    #[error("frame is truncated")]
    TruncatedFrame,
    #[error("frame length is inconsistent")]
    InvalidFrameLength,
    #[error("invalid frame magic")]
    InvalidMagic,
    #[error("unsupported protocol version {major}.{minor}")]
    UnsupportedProtocol { major: u16, minor: u16 },
    #[error("unknown message class {0}")]
    UnknownMessageClass(u8),
    #[error("unsupported frame flags 0x{0:02x}")]
    UnsupportedFrameFlags(u8),
    #[error("invalid nil message identifier")]
    NilMessageId,
    #[error("message serialization failed: {0}")]
    Serialization(#[from] rmp_serde::encode::Error),
    #[error("message deserialization failed: {0}")]
    Deserialization(#[from] rmp_serde::decode::Error),
    #[error("IPC I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("provider operation failed: {0}")]
    Provider(String),
}

pub type Result<T, E = CoreError> = std::result::Result<T, E>;
