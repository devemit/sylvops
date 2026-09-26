//! Shared, platform-neutral `SylvOps` types.

/// Stable application identity shared by the desktop process, shortcuts, packages, and updater.
pub const APPLICATION_ID: &str = "com.devemit.sylvops";

/// Stable publisher recorded in native package and executable metadata.
pub const APPLICATION_PUBLISHER: &str = "devemit";

pub mod config;
pub mod domain;
pub mod error;
pub mod ids;
pub mod protocol;
pub mod provider;
pub mod status;
pub mod ui;
pub mod ui_forms;

pub use error::{CoreError, Result};
