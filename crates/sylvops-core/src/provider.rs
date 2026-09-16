//! Provider-neutral launch, health, resume, and hook contracts.

use std::{collections::BTreeMap, ffi::OsString, fmt, path::PathBuf};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{
    CoreError, Result,
    domain::ProviderKind,
    ids::{SessionId, WorktreeId},
};

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct ProviderCapabilities {
    pub interactive: bool,
    pub resume: bool,
    pub status_hooks: bool,
    pub model_selection: bool,
    pub effort_selection: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderHealth {
    pub kind: ProviderKind,
    pub available: bool,
    pub authenticated: bool,
    pub executable_path: Option<String>,
    pub version: Option<String>,
    pub diagnostic: Option<String>,
    pub capabilities: ProviderCapabilities,
    pub checked_at: i64,
}

#[derive(Clone, Debug)]
pub struct LaunchContext {
    pub session_id: SessionId,
    pub worktree_id: WorktreeId,
    pub cwd: PathBuf,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub initial_prompt: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ResumeContext {
    pub session_id: SessionId,
    pub worktree_id: WorktreeId,
    pub external_session_id: String,
    pub cwd: PathBuf,
    pub model: Option<String>,
    pub effort: Option<String>,
}

#[derive(Clone)]
pub struct LaunchSpec {
    pub executable: PathBuf,
    pub arguments: Vec<OsString>,
    pub environment: BTreeMap<OsString, OsString>,
}

impl fmt::Debug for LaunchSpec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LaunchSpec")
            .field("executable", &self.executable)
            .field("argument_count", &self.arguments.len())
            .field("environment_keys", &self.environment.keys())
            .finish()
    }
}

#[derive(Clone)]
pub struct HookEndpoint {
    pub url: String,
    pub bearer_token: String,
    pub relay_executable: PathBuf,
    pub profile_name: String,
}

impl fmt::Debug for HookEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HookEndpoint")
            .field("url", &self.url)
            .field("bearer_token", &"[REDACTED]")
            .field("relay_executable", &self.relay_executable)
            .field("profile_name", &self.profile_name)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HookInstallation {
    pub profile_name: String,
    pub owned_paths: Vec<PathBuf>,
}

#[async_trait]
pub trait ProviderAdapter: Send + Sync + std::fmt::Debug {
    fn kind(&self) -> ProviderKind;
    async fn probe(&self) -> ProviderHealth;
    /// Builds a structured executable, argument vector, and reviewed environment.
    ///
    /// # Errors
    ///
    /// Returns an error when the provider is unavailable or an option is invalid.
    fn build_launch(&self, context: LaunchContext) -> Result<LaunchSpec>;
    /// Builds a structured resume command when the provider supports resume.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored external identifier or options are invalid.
    fn build_resume(&self, context: ResumeContext) -> Result<Option<LaunchSpec>>;
    /// Installs only application-owned observational status-hook configuration.
    ///
    /// # Errors
    ///
    /// Returns an error when a safe, non-overwriting installation is impossible.
    fn install_status_hooks(
        &self,
        worktree: &std::path::Path,
        endpoint: &HookEndpoint,
    ) -> Result<HookInstallation>;
}

pub fn provider_error(message: impl Into<String>) -> CoreError {
    CoreError::Provider(message.into())
}
