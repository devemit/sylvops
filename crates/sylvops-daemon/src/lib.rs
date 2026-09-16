//! Authoritative daemon, persistence, local IPC, and runtime supervision building blocks.

mod atomic_file;
pub mod client;
pub mod config_store;
pub mod daemon;
pub mod database;
pub mod git;
pub mod hook;
pub mod ipc;
pub mod provider;
pub mod runtime;
pub mod session;

mod process_tree;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum DaemonError {
    #[error(transparent)]
    Protocol(#[from] sylvops_core::CoreError),
    #[error("local IPC failed: {0}")]
    Ipc(#[from] std::io::Error),
    #[error("PTY operation failed: {0}")]
    Pty(String),
    #[error("process-tree operation failed: {0}")]
    ProcessTree(String),
    #[error("session actor stopped")]
    SessionStopped,
    #[error("session request was cancelled")]
    RequestCancelled,
    #[error("invalid session specification: {0}")]
    InvalidSession(String),
    #[error("session attachment refused: {0}")]
    Attachment(String),
    #[error("database operation failed: {0}")]
    Database(String),
    #[error("configuration failed: {0}")]
    Configuration(String),
    #[error("Git operation failed: {0}")]
    Git(String),
    #[error("provider operation failed: {0}")]
    Provider(String),
    #[error("daemon lifecycle failed: {0}")]
    Lifecycle(String),
}

pub type Result<T, E = DaemonError> = std::result::Result<T, E>;
