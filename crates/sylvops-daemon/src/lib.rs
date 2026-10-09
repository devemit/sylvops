//! Authoritative daemon, persistence, local IPC, and runtime supervision building blocks.

mod atomic_file;
mod background_process;
mod claude_discovery;
pub mod client;
mod codex_discovery;
pub mod config_store;
pub mod daemon;
pub mod data_removal;
pub mod database;
pub mod git;
pub mod hook;
pub mod ipc;
pub mod native_upgrade;
pub mod provider;
mod pty;
pub mod runtime;
pub mod session;
pub mod upgrade;

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
    #[error("project request refused: {0}")]
    InvalidProject(String),
    #[error("workspace request refused: {0}")]
    InvalidWorkspace(String),
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
    #[error(transparent)]
    Upgrade(#[from] upgrade::UpgradeError),
    #[error(transparent)]
    DataRemoval(#[from] data_removal::DataRemovalError),
}

pub type Result<T, E = DaemonError> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    #[test]
    fn background_commands_use_the_hidden_process_factory() {
        for (name, source) in [
            ("git", include_str!("git.rs")),
            ("provider", include_str!("provider.rs")),
        ] {
            let production = source
                .split_once("#[cfg(test)]")
                .map_or(source, |(code, _)| code);
            assert!(
                !production.contains("Command::new("),
                "{name} launches a background command without the Windows no-window policy"
            );
        }
    }
}
