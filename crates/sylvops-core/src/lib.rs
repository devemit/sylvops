//! Shared, platform-neutral `SylvOps` types.

pub mod config;
pub mod domain;
pub mod error;
pub mod ids;
pub mod protocol;
pub mod provider;
pub mod status;

pub use error::{CoreError, Result};
