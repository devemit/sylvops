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
    pub arguments: LaunchArguments,
    pub environment: BTreeMap<OsString, OsString>,
}

#[derive(Clone, Default)]
pub struct LaunchArguments {
    values: Vec<OsString>,
    persisted_len: usize,
}

impl fmt::Debug for LaunchArguments {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LaunchArguments")
            .field("argument_count", &self.values.len())
            .field("persisted_argument_count", &self.persisted_len)
            .finish()
    }
}

impl LaunchArguments {
    #[must_use]
    pub fn persisted(values: Vec<OsString>) -> Self {
        Self {
            persisted_len: values.len(),
            values,
        }
    }

    #[must_use]
    pub fn with_transient_tail(mut persisted: Vec<OsString>, transient: Vec<OsString>) -> Self {
        let persisted_len = persisted.len();
        persisted.extend(transient);
        Self {
            values: persisted,
            persisted_len,
        }
    }

    pub fn prepend_persisted(&mut self, values: impl IntoIterator<Item = OsString>) {
        let values: Vec<_> = values.into_iter().collect();
        self.persisted_len = self.persisted_len.saturating_add(values.len());
        self.values.splice(0..0, values);
    }

    #[must_use]
    pub fn all(&self) -> &[OsString] {
        &self.values
    }

    #[must_use]
    pub fn persisted_values(&self) -> &[OsString] {
        &self.values[..self.persisted_len]
    }

    #[must_use]
    pub fn into_all(self) -> Vec<OsString> {
        self.values
    }
}

impl fmt::Debug for LaunchSpec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LaunchSpec")
            .field("executable", &self.executable)
            .field("argument_count", &self.arguments.all().len())
            .field(
                "persisted_argument_count",
                &self.arguments.persisted_values().len(),
            )
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
    async fn refresh(&self) -> ProviderHealth {
        self.probe().await
    }
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
