//! Built-in provider adapters and the daemon-owned provider registry.

use std::{
    collections::{BTreeMap, HashMap},
    ffi::{OsStr, OsString},
    fmt::{self, Write as _},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, RwLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use sylvops_core::{
    domain::ProviderKind,
    provider::{
        AuthenticationRequirement, LaunchArguments, LaunchContext, LaunchSpec,
        ProviderCapabilities, ProviderHealth, ProviderLifecycleEvent, ProviderLifecyclePayload,
        ProviderRuntime, ProviderRuntimeCapabilities, ProviderRuntimeSpec, ResumeContext,
        provider_error,
    },
    status::{
        ConversationIdentity, ConversationIdentityTransition, NormalizedProviderEvent,
        ProviderConversationId, RemainingWork,
    },
};
use tokio::{io::AsyncReadExt, time::timeout};

use crate::{DaemonError, Result, background_process, codex_discovery::discover_codex_executable};

const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const PROBE_OUTPUT_LIMIT: u64 = 64 * 1024;

pub struct ProviderRegistry {
    runtimes: HashMap<ProviderKind, Arc<dyn ProviderRuntime>>,
}

impl fmt::Debug for ProviderRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderRegistry")
            .field("provider_kinds", &self.runtimes.keys())
            .finish()
    }
}

impl ProviderRegistry {
    /// Builds the authoritative built-in provider registry and its owned hook layer.
    ///
    /// # Errors
    ///
    /// Returns an error when the required shell executable cannot be resolved safely.
    pub fn new(hook_relay: Option<&Path>, enabled: &[ProviderKind]) -> Result<Self> {
        let mut runtimes = HashMap::new();
        if enabled.contains(&ProviderKind::Shell) {
            runtimes.insert(
                ProviderKind::Shell,
                Arc::new(ShellAdapter::new()?) as Arc<dyn ProviderRuntime>,
            );
        }
        if enabled.contains(&ProviderKind::Codex) {
            let runtime = if let Some(relay) = hook_relay {
                let directory = codex_home()?;
                std::fs::create_dir_all(&directory).map_err(|error| {
                    DaemonError::Provider(format!(
                        "cannot create Codex configuration directory: {error}"
                    ))
                })?;
                let directory = std::fs::canonicalize(&directory).map_err(|error| {
                    DaemonError::Provider(format!(
                        "cannot canonicalize Codex configuration directory: {error}"
                    ))
                })?;
                cleanup_stale_profiles(&directory);
                CodexAdapter::discover_with_hooks(relay.to_owned(), directory)
            } else {
                CodexAdapter::discover()
            };
            runtimes.insert(
                ProviderKind::Codex,
                Arc::new(runtime) as Arc<dyn ProviderRuntime>,
            );
        }
        Ok(Self { runtimes })
    }

    /// Probes one known provider with bounded commands.
    ///
    /// # Errors
    ///
    /// Returns an error when the requested provider kind is not registered.
    pub async fn probe(&self, kind: ProviderKind) -> Result<ProviderHealth> {
        Ok(self.runtime(kind)?.refresh().await)
    }

    pub async fn probe_all(&self) -> Vec<ProviderHealth> {
        let mut kinds: Vec<_> = self.runtimes.keys().copied().collect();
        kinds.sort_by_key(ToString::to_string);
        let mut health = Vec::with_capacity(kinds.len());
        for kind in kinds {
            health.push(self.runtimes[&kind].probe().await);
        }
        health
    }

    /// Builds a provider launch and applies daemon-owned profile/environment additions.
    ///
    /// # Errors
    ///
    /// Returns an error when the provider is unknown, unavailable, or options are invalid.
    pub fn launch(
        &self,
        kind: ProviderKind,
        context: LaunchContext,
    ) -> Result<ProviderRuntimeSpec> {
        Ok(self.runtime(kind)?.configure_launch(context)?)
    }

    /// Builds a provider resume launch and applies daemon-owned configuration.
    ///
    /// # Errors
    ///
    /// Returns an error when resume is unsupported or the stored identifier is invalid.
    pub fn resume(
        &self,
        kind: ProviderKind,
        context: ResumeContext,
    ) -> Result<ProviderRuntimeSpec> {
        self.runtime(kind)?
            .configure_resume(context)?
            .ok_or_else(|| DaemonError::Provider(format!("provider {kind} cannot resume")))
    }

    /// Normalizes one bounded payload through the owning Provider runtime.
    ///
    /// # Errors
    ///
    /// Returns an error for an unavailable runtime or a malformed recognized payload.
    pub fn normalize_lifecycle_event(
        &self,
        kind: ProviderKind,
        payload: &ProviderLifecyclePayload,
    ) -> Result<ProviderLifecycleEvent> {
        Ok(self.runtime(kind)?.normalize_lifecycle_event(payload)?)
    }

    /// Reports whether the Provider requires authenticated lifecycle delivery.
    ///
    /// # Errors
    ///
    /// Returns an error when the Provider is not enabled.
    pub fn supports_lifecycle_events(&self, kind: ProviderKind) -> Result<bool> {
        Ok(self
            .runtime(kind)?
            .capabilities()
            .runtime
            .is_some_and(|runtime| runtime.lifecycle_events))
    }

    pub fn cleanup_runtime_paths(&self, kind: ProviderKind, paths: &[PathBuf]) {
        let Ok(runtime) = self.runtime(kind) else {
            return;
        };
        if let Err(error) = runtime.cleanup_runtime_paths(paths) {
            tracing::warn!(%error, provider = %kind, "cannot clean provider runtime files");
        }
    }

    fn runtime(&self, kind: ProviderKind) -> Result<&Arc<dyn ProviderRuntime>> {
        self.runtimes
            .get(&kind)
            .ok_or_else(|| DaemonError::Provider(format!("provider {kind} is not enabled")))
    }
}

#[derive(Debug)]
struct ShellAdapter {
    executable: PathBuf,
}

impl ShellAdapter {
    fn new() -> Result<Self> {
        Ok(Self {
            executable: resolve_shell()?,
        })
    }
}

#[async_trait]
impl ProviderRuntime for ShellAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Shell
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            interactive: true,
            ..ProviderCapabilities::default()
        }
    }

    async fn probe(&self) -> ProviderHealth {
        ProviderHealth {
            kind: ProviderKind::Shell,
            available: true,
            authenticated: true,
            executable_path: path_text(&self.executable).ok(),
            version: None,
            diagnostic: None,
            capabilities: self.capabilities(),
            checked_at: now_millis(),
        }
    }

    fn configure_launch(
        &self,
        context: LaunchContext,
    ) -> sylvops_core::Result<ProviderRuntimeSpec> {
        #[cfg(windows)]
        let arguments = vec![OsString::from("/D")];
        #[cfg(unix)]
        let arguments = Vec::new();
        Ok(ProviderRuntimeSpec {
            launch: LaunchSpec {
                executable: self.executable.clone(),
                // `/D` keeps cmd.exe interactive while disabling user AutoRun entries that can
                // silently replace the daemon-supplied worktree working directory.
                arguments: LaunchArguments::persisted(arguments),
                environment: safe_environment(context.session_id, context.worktree_id, false),
            },
            owned_paths: Vec::new(),
        })
    }

    fn configure_resume(
        &self,
        _context: ResumeContext,
    ) -> sylvops_core::Result<Option<ProviderRuntimeSpec>> {
        Ok(None)
    }

    fn normalize_lifecycle_event(
        &self,
        _payload: &ProviderLifecyclePayload,
    ) -> sylvops_core::Result<ProviderLifecycleEvent> {
        Ok(ProviderLifecycleEvent::default())
    }
}

#[derive(Clone, Debug)]
struct CodexDiscovery {
    executable: Option<PathBuf>,
    error: Option<String>,
}

#[derive(Debug)]
struct CodexAdapter {
    discovery: RwLock<CodexDiscovery>,
    hook_relay: Option<PathBuf>,
    hook_directory: Option<PathBuf>,
}

impl CodexAdapter {
    fn discover() -> Self {
        Self {
            discovery: RwLock::new(Self::discover_now()),
            hook_relay: None,
            hook_directory: None,
        }
    }

    fn discover_with_hooks(hook_relay: PathBuf, hook_directory: PathBuf) -> Self {
        Self {
            discovery: RwLock::new(Self::discover_now()),
            hook_relay: Some(hook_relay),
            hook_directory: Some(hook_directory),
        }
    }

    #[cfg(test)]
    fn from_executable(executable: PathBuf) -> Self {
        Self {
            discovery: RwLock::new(CodexDiscovery {
                executable: Some(executable),
                error: None,
            }),
            hook_relay: None,
            hook_directory: None,
        }
    }

    #[cfg(test)]
    fn from_executable_with_hooks(
        executable: PathBuf,
        hook_relay: PathBuf,
        hook_directory: PathBuf,
    ) -> Self {
        let hook_directory = std::fs::canonicalize(hook_directory)
            .expect("test hook configuration directory is canonicalizable");
        Self {
            discovery: RwLock::new(CodexDiscovery {
                executable: Some(executable),
                error: None,
            }),
            hook_relay: Some(hook_relay),
            hook_directory: Some(hook_directory),
        }
    }

    fn discover_now() -> CodexDiscovery {
        match discover_codex_executable() {
            Ok(path) => CodexDiscovery {
                executable: Some(path),
                error: None,
            },
            Err(error) => CodexDiscovery {
                executable: None,
                error: Some(error.to_string()),
            },
        }
    }

    fn discovery(&self) -> std::result::Result<CodexDiscovery, String> {
        self.discovery
            .read()
            .map(|discovery| discovery.clone())
            .map_err(|_| "Codex discovery state is unavailable".into())
    }

    fn executable(&self) -> sylvops_core::Result<PathBuf> {
        let discovery = self.discovery().map_err(provider_error)?;
        discovery.executable.ok_or_else(|| {
            provider_error(
                discovery
                    .error
                    .unwrap_or_else(|| "Codex executable is unavailable".into()),
            )
        })
    }

    fn configure_lifecycle_overlay(
        &self,
        context: &LaunchContext,
        launch: &mut LaunchSpec,
    ) -> sylvops_core::Result<Vec<PathBuf>> {
        let (Some(relay), Some(directory)) = (&self.hook_relay, &self.hook_directory) else {
            return Ok(Vec::new());
        };
        let endpoint = context.lifecycle_endpoint.as_ref().ok_or_else(|| {
            provider_error("Codex lifecycle endpoint is required for a managed Session")
        })?;
        if !endpoint.is_bound_to(ProviderKind::Codex, context.session_id, context.worktree_id) {
            return Err(provider_error(
                "Codex lifecycle endpoint identity does not match the Session",
            ));
        }
        let metadata = std::fs::symlink_metadata(directory).map_err(|error| {
            provider_error(format!(
                "cannot revalidate Codex configuration directory: {error}"
            ))
        })?;
        if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
            return Err(provider_error(
                "Codex configuration directory identity is invalid",
            ));
        }
        let revalidated_directory = std::fs::canonicalize(directory).map_err(|error| {
            provider_error(format!(
                "cannot canonicalize Codex configuration directory: {error}"
            ))
        })?;
        if revalidated_directory != *directory {
            return Err(provider_error(
                "Codex configuration directory identity changed",
            ));
        }
        let profile_name = format!("sylvops-{}", context.session_id);
        let profile_path = revalidated_directory.join(format!("{profile_name}.config.toml"));
        let command = hook_command(relay)?;
        let source = hook_profile_source(&command);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&profile_path)
            .map_err(|error| {
                provider_error(format!("cannot install Codex hook profile: {error}"))
            })?;
        if let Err(error) = std::io::Write::write_all(&mut file, source.as_bytes()) {
            drop(file);
            let _ = std::fs::remove_file(&profile_path);
            return Err(provider_error(format!(
                "cannot write Codex hook profile: {error}"
            )));
        }
        if let Err(error) = file.sync_all() {
            drop(file);
            let _ = std::fs::remove_file(&profile_path);
            return Err(provider_error(format!(
                "cannot sync Codex hook profile: {error}"
            )));
        }

        launch
            .arguments
            .prepend_persisted([OsString::from("--profile"), OsString::from(profile_name)]);
        launch.environment.insert(
            OsString::from("SYLVOPS_HOOK_ENDPOINT"),
            OsString::from(endpoint.url()),
        );
        launch.environment.insert(
            OsString::from("SYLVOPS_HOOK_TOKEN"),
            OsString::from(endpoint.bearer_token()),
        );
        Ok(vec![profile_path])
    }
}

impl CodexAdapter {
    async fn probe_health(&self) -> ProviderHealth {
        let capabilities = ProviderCapabilities {
            interactive: true,
            resume: true,
            status_hooks: true,
            model_selection: true,
            effort_selection: true,
            authentication: AuthenticationRequirement::ExistingLogin,
            runtime: Some(ProviderRuntimeCapabilities {
                lifecycle_events: true,
                ..ProviderRuntimeCapabilities::default()
            }),
        };
        let discovery = match self.discovery() {
            Ok(discovery) => discovery,
            Err(error) => {
                return ProviderHealth {
                    kind: ProviderKind::Codex,
                    available: false,
                    authenticated: false,
                    executable_path: None,
                    version: None,
                    diagnostic: Some(error),
                    capabilities,
                    checked_at: now_millis(),
                };
            }
        };
        let Some(executable) = discovery.executable else {
            return ProviderHealth {
                kind: ProviderKind::Codex,
                available: false,
                authenticated: false,
                executable_path: None,
                version: None,
                diagnostic: discovery.error,
                capabilities,
                checked_at: now_millis(),
            };
        };
        let version = run_probe(&executable, &["--version"]).await;
        let authentication = run_probe(&executable, &["login", "status"]).await;
        let version_error = version.as_ref().err().cloned();
        let authentication_error = authentication.as_ref().err().cloned();
        ProviderHealth {
            kind: ProviderKind::Codex,
            available: version.is_ok(),
            authenticated: authentication.is_ok(),
            executable_path: path_text(&executable).ok(),
            version: version.ok(),
            diagnostic: authentication_error.or(version_error),
            capabilities,
            checked_at: now_millis(),
        }
    }

    async fn refresh_health(&self) -> ProviderHealth {
        let discovery = Self::discover_now();
        match self.discovery.write() {
            Ok(mut current) => *current = discovery,
            Err(_) => {
                return ProviderHealth {
                    kind: ProviderKind::Codex,
                    available: false,
                    authenticated: false,
                    executable_path: None,
                    version: None,
                    diagnostic: Some("Codex discovery state is unavailable".into()),
                    capabilities: ProviderCapabilities {
                        interactive: true,
                        resume: true,
                        status_hooks: true,
                        model_selection: true,
                        effort_selection: true,
                        authentication: AuthenticationRequirement::ExistingLogin,
                        runtime: Some(ProviderRuntimeCapabilities {
                            lifecycle_events: true,
                            ..ProviderRuntimeCapabilities::default()
                        }),
                    },
                    checked_at: now_millis(),
                };
            }
        }
        self.probe_health().await
    }

    fn build_launch_spec(&self, context: LaunchContext) -> sylvops_core::Result<LaunchSpec> {
        validate_selector(context.model.as_deref(), "model")?;
        validate_selector(context.effort.as_deref(), "effort")?;
        validate_prompt(context.initial_prompt.as_deref())?;
        let mut arguments = vec![OsString::from("--cd"), context.cwd.as_os_str().to_owned()];
        if let Some(model) = context.model {
            arguments.extend([OsString::from("--model"), OsString::from(model)]);
        }
        if let Some(effort) = context.effort {
            arguments.extend([
                OsString::from("--config"),
                OsString::from(format!("model_reasoning_effort={effort}")),
            ]);
        }
        let arguments = match context.initial_prompt {
            Some(prompt) => {
                LaunchArguments::with_transient_tail(arguments, vec![OsString::from(prompt)])
            }
            None => LaunchArguments::persisted(arguments),
        };
        Ok(LaunchSpec {
            executable: self.executable()?,
            arguments,
            environment: safe_environment(context.session_id, context.worktree_id, true),
        })
    }

    fn build_resume_spec(
        &self,
        context: ResumeContext,
    ) -> sylvops_core::Result<Option<LaunchSpec>> {
        validate_external_id(&context.external_session_id)?;
        validate_selector(context.model.as_deref(), "model")?;
        validate_selector(context.effort.as_deref(), "effort")?;
        let mut arguments = vec![
            OsString::from("resume"),
            OsString::from(context.external_session_id),
            OsString::from("--cd"),
            context.cwd.as_os_str().to_owned(),
        ];
        if let Some(model) = context.model {
            arguments.extend([OsString::from("--model"), OsString::from(model)]);
        }
        if let Some(effort) = context.effort {
            arguments.extend([
                OsString::from("--config"),
                OsString::from(format!("model_reasoning_effort={effort}")),
            ]);
        }
        Ok(Some(LaunchSpec {
            executable: self.executable()?,
            arguments: LaunchArguments::persisted(arguments),
            environment: safe_environment(context.session_id, context.worktree_id, true),
        }))
    }
}

#[async_trait]
impl ProviderRuntime for CodexAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Codex
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            interactive: true,
            resume: true,
            status_hooks: true,
            model_selection: true,
            effort_selection: true,
            authentication: AuthenticationRequirement::ExistingLogin,
            runtime: Some(ProviderRuntimeCapabilities {
                lifecycle_events: true,
                ..ProviderRuntimeCapabilities::default()
            }),
        }
    }

    async fn probe(&self) -> ProviderHealth {
        self.probe_health().await
    }

    async fn refresh(&self) -> ProviderHealth {
        self.refresh_health().await
    }

    fn configure_launch(
        &self,
        context: LaunchContext,
    ) -> sylvops_core::Result<ProviderRuntimeSpec> {
        let mut launch = self.build_launch_spec(context.clone())?;
        let owned_paths = self.configure_lifecycle_overlay(&context, &mut launch)?;
        Ok(ProviderRuntimeSpec {
            launch,
            owned_paths,
        })
    }

    fn configure_resume(
        &self,
        context: ResumeContext,
    ) -> sylvops_core::Result<Option<ProviderRuntimeSpec>> {
        let launch_context = LaunchContext {
            session_id: context.session_id,
            worktree_id: context.worktree_id,
            cwd: context.cwd.clone(),
            model: context.model.clone(),
            effort: context.effort.clone(),
            initial_prompt: None,
            lifecycle_endpoint: context.lifecycle_endpoint.clone(),
        };
        let Some(mut launch) = self.build_resume_spec(context)? else {
            return Ok(None);
        };
        let owned_paths = self.configure_lifecycle_overlay(&launch_context, &mut launch)?;
        Ok(Some(ProviderRuntimeSpec {
            launch,
            owned_paths,
        }))
    }

    fn cleanup_runtime_paths(&self, paths: &[PathBuf]) -> sylvops_core::Result<()> {
        let Some(directory) = self.hook_directory.as_ref() else {
            if paths.is_empty() {
                return Ok(());
            }
            return Err(provider_error(
                "Codex runtime cleanup has no owned configuration directory",
            ));
        };
        for path in paths {
            let Some(name) = path.file_name().and_then(OsStr::to_str) else {
                return Err(provider_error("Codex runtime path has no valid file name"));
            };
            if !name.starts_with("sylvops-") || !name.ends_with(".config.toml") {
                return Err(provider_error(
                    "Codex runtime path is not application-owned",
                ));
            }
            let parent = path
                .parent()
                .ok_or_else(|| provider_error("Codex runtime path has no parent"))?;
            let canonical_parent = std::fs::canonicalize(parent).map_err(|error| {
                provider_error(format!(
                    "cannot revalidate Codex runtime directory: {error}"
                ))
            })?;
            if canonical_parent != *directory {
                return Err(provider_error(
                    "Codex runtime path escaped its owned configuration directory",
                ));
            }
            let metadata = match std::fs::symlink_metadata(path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(provider_error(format!(
                        "cannot inspect Codex runtime file: {error}"
                    )));
                }
            };
            if !metadata.file_type().is_file()
                || metadata.file_type().is_symlink()
                || metadata.len() > 64 * 1024
            {
                return Err(provider_error("Codex runtime file identity is invalid"));
            }
            let canonical_path = std::fs::canonicalize(path).map_err(|error| {
                provider_error(format!("cannot canonicalize Codex runtime file: {error}"))
            })?;
            if canonical_path.parent() != Some(directory.as_path()) {
                return Err(provider_error(
                    "Codex runtime file escaped its owned configuration directory",
                ));
            }
            let source = std::fs::read_to_string(&canonical_path).map_err(|error| {
                provider_error(format!("cannot read Codex runtime file: {error}"))
            })?;
            if !owned_profile_checksum_is_valid(&source) {
                return Err(provider_error(
                    "Codex runtime file is no longer application-owned",
                ));
            }
            std::fs::remove_file(&canonical_path).map_err(|error| {
                provider_error(format!("cannot remove Codex runtime file: {error}"))
            })?;
        }
        Ok(())
    }

    fn normalize_lifecycle_event(
        &self,
        payload: &ProviderLifecyclePayload,
    ) -> sylvops_core::Result<ProviderLifecycleEvent> {
        let payload: serde_json::Value = serde_json::from_slice(payload.as_bytes())
            .map_err(|_| provider_error("Codex lifecycle payload is not valid JSON"))?;
        let conversation_id = payload
            .get("session_id")
            .and_then(serde_json::Value::as_str)
            .map(ProviderConversationId::new)
            .transpose()
            .map_err(provider_error)?;
        let turn_id = payload
            .get("turn_id")
            .and_then(serde_json::Value::as_str)
            .map(ProviderConversationId::new)
            .transpose()
            .map_err(provider_error)?
            .map(|value| value.to_string());
        let event = match payload
            .get("hook_event_name")
            .and_then(serde_json::Value::as_str)
        {
            Some("SessionStart") => {
                let id = conversation_id.clone().ok_or_else(|| {
                    provider_error("Codex SessionStart is missing its session ID")
                })?;
                Some(NormalizedProviderEvent::TurnStarted {
                    conversation: Some(ConversationIdentity {
                        id,
                        transition: ConversationIdentityTransition::Established,
                    }),
                })
            }
            Some("SessionEnd") => Some(NormalizedProviderEvent::SessionEnded),
            Some("UserPromptSubmit") => Some(NormalizedProviderEvent::PromptSubmitted),
            Some("PermissionRequest") => Some(NormalizedProviderEvent::PermissionRequested),
            Some("SubagentStart") => Some(NormalizedProviderEvent::SubagentStarted {
                agent_id: bounded_payload_field(&payload, "agent_id"),
            }),
            Some("SubagentStop") => Some(NormalizedProviderEvent::SubagentStopped {
                agent_id: bounded_payload_field(&payload, "agent_id"),
            }),
            Some("Stop" | "Interrupt") => Some(NormalizedProviderEvent::TurnStopped {
                remaining_work: RemainingWork::default(),
            }),
            Some(_) => return Err(provider_error("unknown Codex lifecycle event")),
            None => return Err(provider_error("Codex lifecycle event type is missing")),
        };
        Ok(ProviderLifecycleEvent {
            event,
            conversation_id,
            turn_id,
        })
    }
}

fn bounded_payload_field(payload: &serde_json::Value, name: &str) -> String {
    payload
        .get(name)
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown")
        .chars()
        .take(200)
        .collect()
}

fn hook_profile_source(command: &str) -> String {
    let command = toml::Value::String(command.to_owned()).to_string();
    let mut body = String::from("[features]\nhooks = true\n");
    for event in [
        "SessionStart",
        "SessionEnd",
        "UserPromptSubmit",
        "PermissionRequest",
        "SubagentStart",
        "SubagentStop",
        "Stop",
        "Interrupt",
    ] {
        let _ = write!(
            body,
            "\n[[hooks.{event}]]\n[[hooks.{event}.hooks]]\ntype = \"command\"\ncommand = {command}\ncommand_windows = {command}\nasync = true\ntimeout = 2\n"
        );
    }
    let checksum = format!("{:x}", Sha256::digest(body.as_bytes()));
    format!("# sylvops-managed-v1 sha256={checksum}\n{body}")
}

fn cleanup_stale_profiles(directory: &Path) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("sylvops-") || !name.ends_with(".config.toml") {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file() || file_type.is_symlink() {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.len() > 64 * 1024 {
            continue;
        }
        let Ok(source) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        if owned_profile_checksum_is_valid(&source) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn owned_profile_checksum_is_valid(source: &str) -> bool {
    let Some((header, body)) = source.split_once('\n') else {
        return false;
    };
    let Some(expected) = header.strip_prefix("# sylvops-managed-v1 sha256=") else {
        return false;
    };
    let actual = format!("{:x}", Sha256::digest(body.as_bytes()));
    expected == actual
}

fn hook_command(executable: &Path) -> sylvops_core::Result<String> {
    let text = executable
        .to_str()
        .ok_or_else(|| provider_error("hook relay path is not valid Unicode"))?;
    if text.contains(['\n', '\r', '"']) {
        return Err(provider_error("hook relay path contains unsafe characters"));
    }
    Ok(format!("\"{text}\" hook emit"))
}

fn codex_home() -> sylvops_core::Result<PathBuf> {
    if let Some(path) = std::env::var_os("CODEX_HOME").map(PathBuf::from) {
        if path.is_absolute() {
            return Ok(path);
        }
        return Err(provider_error("CODEX_HOME must be absolute"));
    }
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE");
    #[cfg(unix)]
    let home = std::env::var_os("HOME");
    home.map(PathBuf::from)
        .map(|path| path.join(".codex"))
        .ok_or_else(|| provider_error("cannot locate Codex configuration directory"))
}

async fn run_probe(executable: &Path, arguments: &[&str]) -> std::result::Result<String, String> {
    let mut child = background_process::command(executable);
    child
        .args(arguments)
        .env_clear()
        .envs(reviewed_environment(true))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = child
        .spawn()
        .map_err(|error| format!("probe launch failed: {error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "probe stdout unavailable".to_owned())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "probe stderr unavailable".to_owned())?;
    let capture = async move {
        let stdout_task = tokio::spawn(read_probe_output(stdout));
        let stderr_task = tokio::spawn(read_probe_output(stderr));
        let status = child.wait().await.map_err(|error| error.to_string())?;
        let stdout = stdout_task.await.map_err(|error| error.to_string())??;
        let stderr = stderr_task.await.map_err(|error| error.to_string())??;
        let output = if stdout.is_empty() { stderr } else { stdout };
        let output = String::from_utf8_lossy(&output)
            .trim()
            .chars()
            .take(512)
            .collect::<String>();
        if status.success() {
            Ok(output)
        } else if output.is_empty() {
            Err(format!("probe exited with {status}"))
        } else {
            Err(output)
        }
    };
    timeout(PROBE_TIMEOUT, capture)
        .await
        .map_err(|_| "probe timed out".to_owned())?
}

async fn read_probe_output<R: tokio::io::AsyncRead + Unpin>(
    reader: R,
) -> std::result::Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    reader
        .take(PROBE_OUTPUT_LIMIT + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| error.to_string())?;
    if bytes.len() as u64 > PROBE_OUTPUT_LIMIT {
        return Err("probe output exceeded 64 KiB".into());
    }
    Ok(bytes)
}

fn safe_environment(
    session_id: sylvops_core::ids::SessionId,
    worktree_id: sylvops_core::ids::WorktreeId,
    codex: bool,
) -> BTreeMap<OsString, OsString> {
    let mut environment = reviewed_environment(codex);
    environment.insert("SYLVOPS_SESSION_ID".into(), session_id.to_string().into());
    environment.insert("SYLVOPS_WORKTREE_ID".into(), worktree_id.to_string().into());
    environment
        .entry("TERM".into())
        .or_insert_with(|| "xterm-256color".into());
    environment
}

fn reviewed_environment(codex: bool) -> BTreeMap<OsString, OsString> {
    let mut environment = BTreeMap::new();
    for (key, value) in std::env::vars_os() {
        if environment_allowed(&key)
            || (codex && key.eq_ignore_ascii_case(OsStr::new("CODEX_HOME")))
        {
            environment.insert(key, value);
        }
    }
    environment
}

fn environment_allowed(key: &OsStr) -> bool {
    let key = key.to_string_lossy().to_ascii_uppercase();
    #[cfg(unix)]
    {
        matches!(
            key.as_str(),
            "PATH" | "HOME" | "USER" | "LOGNAME" | "SHELL" | "TERM" | "COLORTERM" | "TMPDIR"
        ) || key == "LANG"
            || key.starts_with("LC_")
    }
    #[cfg(windows)]
    {
        matches!(
            key.as_str(),
            "SYSTEMROOT"
                | "WINDIR"
                | "COMSPEC"
                | "PATH"
                | "PATHEXT"
                | "TEMP"
                | "TMP"
                | "USERPROFILE"
                | "HOMEDRIVE"
                | "HOMEPATH"
                | "APPDATA"
                | "LOCALAPPDATA"
                | "USERNAME"
                | "TERM"
                | "COLORTERM"
        )
    }
}

fn resolve_shell() -> Result<PathBuf> {
    #[cfg(unix)]
    let candidate =
        std::env::var_os("SHELL").map_or_else(|| PathBuf::from("/bin/sh"), PathBuf::from);
    #[cfg(windows)]
    let candidate = std::env::var_os("COMSPEC").map_or_else(
        || {
            PathBuf::from(
                std::env::var_os("SystemRoot").unwrap_or_else(|| OsString::from(r"C:\Windows")),
            )
            .join("System32")
            .join("cmd.exe")
        },
        PathBuf::from,
    );
    std::fs::canonicalize(candidate)
        .map_err(|error| DaemonError::Provider(format!("cannot resolve shell executable: {error}")))
}

fn validate_selector(value: Option<&str>, label: &str) -> sylvops_core::Result<()> {
    if value.is_some_and(|value| {
        value.is_empty() || value.len() > 128 || value.chars().any(char::is_control)
    }) {
        return Err(provider_error(format!("invalid {label} selector")));
    }
    Ok(())
}

fn validate_prompt(value: Option<&str>) -> sylvops_core::Result<()> {
    if value.is_some_and(|value| value.len() > 64 * 1024 || value.contains('\0')) {
        return Err(provider_error(
            "initial prompt exceeds 64 KiB or contains NUL",
        ));
    }
    Ok(())
}

fn validate_external_id(value: &str) -> sylvops_core::Result<()> {
    if value.is_empty()
        || value.len() > 200
        || !value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err(provider_error("invalid external session identifier"));
    }
    Ok(())
}

fn path_text(path: &Path) -> Result<String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| DaemonError::Provider("provider path is not valid Unicode".into()))
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use sylvops_core::{
        provider::{ProviderLifecyclePayload, ProviderRuntime},
        status::NormalizedProviderEvent,
    };

    #[derive(Debug)]
    struct RefreshTrackingAdapter {
        refreshes: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ProviderRuntime for RefreshTrackingAdapter {
        fn kind(&self) -> ProviderKind {
            ProviderKind::Codex
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities::default()
        }

        async fn probe(&self) -> ProviderHealth {
            ProviderHealth {
                kind: ProviderKind::Codex,
                available: false,
                authenticated: false,
                executable_path: None,
                version: None,
                diagnostic: Some("cached discovery miss".into()),
                capabilities: ProviderCapabilities::default(),
                checked_at: 0,
            }
        }

        async fn refresh(&self) -> ProviderHealth {
            self.refreshes.fetch_add(1, Ordering::SeqCst);
            ProviderHealth {
                kind: ProviderKind::Codex,
                available: true,
                authenticated: true,
                executable_path: Some("native-codex".into()),
                version: Some("codex-cli test".into()),
                diagnostic: None,
                capabilities: ProviderCapabilities::default(),
                checked_at: 1,
            }
        }

        fn configure_launch(
            &self,
            _context: LaunchContext,
        ) -> sylvops_core::Result<ProviderRuntimeSpec> {
            unreachable!("not used by refresh test")
        }

        fn configure_resume(
            &self,
            _context: ResumeContext,
        ) -> sylvops_core::Result<Option<ProviderRuntimeSpec>> {
            unreachable!("not used by refresh test")
        }

        fn normalize_lifecycle_event(
            &self,
            _payload: &ProviderLifecyclePayload,
        ) -> sylvops_core::Result<ProviderLifecycleEvent> {
            Ok(ProviderLifecycleEvent::default())
        }
    }

    #[tokio::test]
    async fn explicit_provider_probe_refreshes_discovery() {
        let refreshes = Arc::new(AtomicUsize::new(0));
        let adapter = Arc::new(RefreshTrackingAdapter {
            refreshes: Arc::clone(&refreshes),
        });
        let registry = ProviderRegistry {
            runtimes: HashMap::from([(ProviderKind::Codex, adapter as Arc<dyn ProviderRuntime>)]),
        };

        let health = registry
            .probe(ProviderKind::Codex)
            .await
            .expect("refresh known provider");

        assert!(health.available);
        assert_eq!(refreshes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn environment_excludes_credentials() {
        assert!(!environment_allowed(OsStr::new("OPENAI_API_KEY")));
        assert!(!environment_allowed(OsStr::new("GH_TOKEN")));
        assert!(environment_allowed(OsStr::new("PATH")));
    }

    #[test]
    fn shell_launch_disables_windows_autorun_without_changing_directory() {
        let adapter = ShellAdapter::new().expect("resolve platform shell");
        let spec = adapter
            .configure_launch(LaunchContext {
                session_id: sylvops_core::ids::SessionId::new(),
                worktree_id: sylvops_core::ids::WorktreeId::new(),
                cwd: PathBuf::from(if cfg!(windows) {
                    r"C:\work trees\feature"
                } else {
                    "/tmp/work trees/feature"
                }),
                model: None,
                effort: None,
                initial_prompt: None,
                lifecycle_endpoint: None,
            })
            .expect("shell launch specification")
            .launch;
        #[cfg(windows)]
        assert_eq!(spec.arguments.all(), [OsString::from("/D")]);
        #[cfg(unix)]
        assert_eq!(spec.arguments.all(), Vec::<OsString>::new());
    }

    #[test]
    fn hook_profile_has_observational_events_only() {
        let source = hook_profile_source("sylvops hook emit");
        assert!(owned_profile_checksum_is_valid(&source));
        assert!(source.contains("hooks.PermissionRequest"));
        assert!(source.contains("hooks.SubagentStop"));
        assert!(!source.contains("behavior = \"allow\""));
    }

    #[test]
    fn codex_launch_uses_structured_arguments() {
        let adapter = CodexAdapter::from_executable(PathBuf::from(if cfg!(windows) {
            r"C:\Program Files\Codex\codex.exe"
        } else {
            "/usr/local/bin/codex"
        }));
        let worktree = PathBuf::from(if cfg!(windows) {
            r"C:\work trees\feature"
        } else {
            "/tmp/work trees/feature"
        });
        let spec = adapter
            .configure_launch(LaunchContext {
                session_id: sylvops_core::ids::SessionId::new(),
                worktree_id: sylvops_core::ids::WorktreeId::new(),
                cwd: worktree.clone(),
                model: Some("gpt-test".into()),
                effort: Some("high".into()),
                initial_prompt: Some("fix the parser; do not invoke a shell".into()),
                lifecycle_endpoint: None,
            })
            .expect("launch specification")
            .launch;
        assert_eq!(spec.arguments.all()[0], OsString::from("--cd"));
        assert_eq!(spec.arguments.all()[1], worktree.into_os_string());
        assert_eq!(spec.arguments.all()[2], OsString::from("--model"));
        assert_eq!(spec.arguments.all()[3], OsString::from("gpt-test"));
        assert_eq!(
            spec.arguments.all().last(),
            Some(&OsString::from("fix the parser; do not invoke a shell"))
        );
        assert!(
            !spec
                .arguments
                .persisted_values()
                .contains(&OsString::from("fix the parser; do not invoke a shell"))
        );
    }

    #[test]
    fn codex_resume_validates_external_id() {
        let adapter = CodexAdapter::from_executable(PathBuf::from(if cfg!(windows) {
            r"C:\Codex\codex.exe"
        } else {
            "/usr/bin/codex"
        }));
        let result = adapter.configure_resume(ResumeContext {
            session_id: sylvops_core::ids::SessionId::new(),
            worktree_id: sylvops_core::ids::WorktreeId::new(),
            external_session_id: "../../wrong-worktree".into(),
            cwd: PathBuf::from("worktree"),
            model: None,
            effort: None,
            lifecycle_endpoint: None,
        });
        assert!(result.is_err());
    }

    #[test]
    fn codex_runtime_owns_lifecycle_event_normalization() {
        let runtime = CodexAdapter::from_executable(PathBuf::from(if cfg!(windows) {
            r"C:\Codex\codex.exe"
        } else {
            "/usr/bin/codex"
        }));
        let permission = ProviderLifecyclePayload::new(
            br#"{"hook_event_name":"PermissionRequest","session_id":"codex-session-1"}"#.to_vec(),
        )
        .expect("bounded payload");
        let normalized_permission = runtime
            .normalize_lifecycle_event(&permission)
            .expect("known event");
        assert_eq!(
            normalized_permission.event,
            Some(NormalizedProviderEvent::PermissionRequested)
        );
        assert_eq!(
            normalized_permission
                .conversation_id
                .as_ref()
                .map(ProviderConversationId::as_str),
            Some("codex-session-1")
        );

        let unknown =
            ProviderLifecyclePayload::new(br#"{"hook_event_name":"FutureCodexEvent"}"#.to_vec())
                .expect("bounded payload");
        assert!(runtime.normalize_lifecycle_event(&unknown).is_err());

        let started = ProviderLifecyclePayload::new(
            br#"{"hook_event_name":"SessionStart","session_id":"codex-session-1","source":"startup"}"#
                .to_vec(),
        )
        .expect("bounded payload");
        assert!(matches!(
            runtime
                .normalize_lifecycle_event(&started)
                .expect("session start")
                .event,
            Some(NormalizedProviderEvent::TurnStarted {
                conversation: Some(ConversationIdentity {
                    transition: ConversationIdentityTransition::Established,
                    ..
                })
            })
        ));

        let invalid_identity = ProviderLifecyclePayload::new(
            br#"{"hook_event_name":"SessionStart","session_id":"../../other"}"#.to_vec(),
        )
        .expect("bounded payload");
        assert!(
            runtime
                .normalize_lifecycle_event(&invalid_identity)
                .is_err()
        );
    }

    #[test]
    fn codex_runtime_configures_a_session_owned_hook_overlay() {
        let temporary = tempfile::tempdir().expect("temporary hook directory");
        let runtime = CodexAdapter::from_executable_with_hooks(
            PathBuf::from(if cfg!(windows) {
                r"C:\Codex\codex.exe"
            } else {
                "/usr/bin/codex"
            }),
            std::env::current_exe().expect("test executable"),
            temporary.path().to_owned(),
        );
        let session_id = sylvops_core::ids::SessionId::new();
        let worktree_id = sylvops_core::ids::WorktreeId::new();
        let endpoint = sylvops_core::provider::ProviderLifecycleEndpoint::new(
            ProviderKind::Codex,
            session_id,
            worktree_id,
            "http://127.0.0.1:3210/v1/events",
            "0123456789abcdef0123456789abcdef",
        )
        .expect("lifecycle endpoint");
        let configured = runtime
            .configure_launch(LaunchContext {
                session_id,
                worktree_id,
                cwd: PathBuf::from("worktree"),
                model: None,
                effort: None,
                initial_prompt: None,
                lifecycle_endpoint: Some(endpoint.clone()),
            })
            .expect("configured launch");

        assert_eq!(
            configured
                .launch
                .environment
                .get(OsStr::new("SYLVOPS_HOOK_ENDPOINT")),
            Some(&OsString::from(endpoint.url()))
        );
        assert_eq!(
            configured
                .launch
                .environment
                .get(OsStr::new("SYLVOPS_HOOK_TOKEN")),
            Some(&OsString::from(endpoint.bearer_token()))
        );
        assert_eq!(configured.owned_paths.len(), 1);
        let source = std::fs::read_to_string(&configured.owned_paths[0])
            .expect("application-owned hook profile");
        assert!(owned_profile_checksum_is_valid(&source));
        assert_eq!(
            configured.launch.arguments.all()[0],
            OsStr::new("--profile")
        );
    }

    #[test]
    fn runtime_cleanup_removes_only_intact_application_owned_profiles() {
        let temporary = tempfile::tempdir().expect("temporary hook directory");
        let runtime = Arc::new(CodexAdapter::from_executable_with_hooks(
            PathBuf::from(if cfg!(windows) {
                r"C:\Codex\codex.exe"
            } else {
                "/usr/bin/codex"
            }),
            std::env::current_exe().expect("test executable"),
            temporary.path().to_owned(),
        ));
        let registry = ProviderRegistry {
            runtimes: HashMap::from([(
                ProviderKind::Codex,
                Arc::clone(&runtime) as Arc<dyn ProviderRuntime>,
            )]),
        };

        let configure = |session_id| {
            let worktree_id = sylvops_core::ids::WorktreeId::new();
            let endpoint = sylvops_core::provider::ProviderLifecycleEndpoint::new(
                ProviderKind::Codex,
                session_id,
                worktree_id,
                "http://127.0.0.1:3210/v1/events",
                "0123456789abcdef0123456789abcdef",
            )
            .expect("lifecycle endpoint");
            runtime
                .configure_launch(LaunchContext {
                    session_id,
                    worktree_id,
                    cwd: PathBuf::from("worktree"),
                    model: None,
                    effort: None,
                    initial_prompt: None,
                    lifecycle_endpoint: Some(endpoint),
                })
                .expect("configured launch")
        };

        let intact = configure(sylvops_core::ids::SessionId::new());
        let intact_path = intact.owned_paths[0].clone();
        registry.cleanup_runtime_paths(ProviderKind::Codex, &intact.owned_paths);
        assert!(!intact_path.exists());

        let tampered = configure(sylvops_core::ids::SessionId::new());
        let tampered_path = tampered.owned_paths[0].clone();
        std::fs::write(&tampered_path, "user-owned replacement").expect("replace managed profile");
        registry.cleanup_runtime_paths(ProviderKind::Codex, &tampered.owned_paths);
        assert!(tampered_path.exists());
    }
}
