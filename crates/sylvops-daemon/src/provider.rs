//! Built-in provider adapters and the daemon-owned provider registry.

use std::{
    collections::{BTreeMap, HashMap},
    ffi::{OsStr, OsString},
    fmt::{self, Write as _},
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use sylvops_core::{
    domain::ProviderKind,
    provider::{
        HookEndpoint, HookInstallation, LaunchContext, LaunchSpec, ProviderAdapter,
        ProviderCapabilities, ProviderHealth, ResumeContext, provider_error,
    },
};
use tokio::{io::AsyncReadExt, process::Command, time::timeout};

use crate::{DaemonError, Result};

const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const PROBE_OUTPUT_LIMIT: u64 = 64 * 1024;

pub struct ProviderRegistry {
    adapters: HashMap<ProviderKind, Arc<dyn ProviderAdapter>>,
    codex_profile: Option<String>,
    hook_environment: Option<(OsString, OsString)>,
    owned_hook_paths: Vec<PathBuf>,
}

impl fmt::Debug for ProviderRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderRegistry")
            .field("provider_kinds", &self.adapters.keys())
            .field("codex_profile", &self.codex_profile)
            .field("hook_configured", &self.hook_environment.is_some())
            .field("owned_hook_paths", &self.owned_hook_paths)
            .finish()
    }
}

impl ProviderRegistry {
    /// Builds the authoritative built-in provider registry and its owned hook layer.
    ///
    /// # Errors
    ///
    /// Returns an error when the required shell executable cannot be resolved safely.
    pub fn new(hook_endpoint: Option<&HookEndpoint>, enabled: &[ProviderKind]) -> Result<Self> {
        let mut adapters = HashMap::new();
        if enabled.contains(&ProviderKind::Shell) {
            adapters.insert(
                ProviderKind::Shell,
                Arc::new(ShellAdapter::new()?) as Arc<dyn ProviderAdapter>,
            );
        }
        if enabled.contains(&ProviderKind::Codex) {
            adapters.insert(
                ProviderKind::Codex,
                Arc::new(CodexAdapter::discover()) as Arc<dyn ProviderAdapter>,
            );
        }

        let codex = adapters.get(&ProviderKind::Codex);
        let (codex_profile, owned_hook_paths) =
            if let (Some(endpoint), Some(codex)) = (hook_endpoint, codex) {
                match codex.install_status_hooks(Path::new("."), endpoint) {
                    Ok(installation) => (Some(installation.profile_name), installation.owned_paths),
                    Err(error) => {
                        tracing::warn!(%error, "Codex status hooks are unavailable");
                        (None, Vec::new())
                    }
                }
            } else {
                (None, Vec::new())
            };
        let hook_environment = hook_endpoint.map(|endpoint| {
            (
                endpoint.url.clone().into(),
                endpoint.bearer_token.clone().into(),
            )
        });
        Ok(Self {
            adapters,
            codex_profile,
            hook_environment,
            owned_hook_paths,
        })
    }

    /// Probes one known provider with bounded commands.
    ///
    /// # Errors
    ///
    /// Returns an error when the requested provider kind is not registered.
    pub async fn probe(&self, kind: ProviderKind) -> Result<ProviderHealth> {
        Ok(self.adapter(kind)?.probe().await)
    }

    pub async fn probe_all(&self) -> Vec<ProviderHealth> {
        let mut kinds: Vec<_> = self.adapters.keys().copied().collect();
        kinds.sort_by_key(ToString::to_string);
        let mut health = Vec::with_capacity(kinds.len());
        for kind in kinds {
            health.push(self.adapters[&kind].probe().await);
        }
        health
    }

    /// Builds a provider launch and applies daemon-owned profile/environment additions.
    ///
    /// # Errors
    ///
    /// Returns an error when the provider is unknown, unavailable, or options are invalid.
    pub fn launch(&self, kind: ProviderKind, context: LaunchContext) -> Result<LaunchSpec> {
        let mut spec = self.adapter(kind)?.build_launch(context)?;
        self.apply_profile(kind, &mut spec);
        Ok(spec)
    }

    /// Builds a provider resume launch and applies daemon-owned configuration.
    ///
    /// # Errors
    ///
    /// Returns an error when resume is unsupported or the stored identifier is invalid.
    pub fn resume(&self, kind: ProviderKind, context: ResumeContext) -> Result<LaunchSpec> {
        let mut spec = self
            .adapter(kind)?
            .build_resume(context)?
            .ok_or_else(|| DaemonError::Provider(format!("provider {kind} cannot resume")))?;
        self.apply_profile(kind, &mut spec);
        Ok(spec)
    }

    fn adapter(&self, kind: ProviderKind) -> Result<&Arc<dyn ProviderAdapter>> {
        self.adapters
            .get(&kind)
            .ok_or_else(|| DaemonError::Provider(format!("provider {kind} is not enabled")))
    }

    fn apply_profile(&self, kind: ProviderKind, spec: &mut LaunchSpec) {
        if kind == ProviderKind::Codex
            && let Some(profile) = &self.codex_profile
        {
            spec.arguments.insert(0, OsString::from(profile));
            spec.arguments.insert(0, OsString::from("--profile"));
        }
        if kind == ProviderKind::Codex
            && let Some((endpoint, token)) = &self.hook_environment
        {
            spec.environment
                .insert("SYLVOPS_HOOK_ENDPOINT".into(), endpoint.clone());
            spec.environment
                .insert("SYLVOPS_HOOK_TOKEN".into(), token.clone());
        }
    }
}

impl Drop for ProviderRegistry {
    fn drop(&mut self) {
        for path in &self.owned_hook_paths {
            let _ = std::fs::remove_file(path);
        }
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
impl ProviderAdapter for ShellAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Shell
    }

    async fn probe(&self) -> ProviderHealth {
        ProviderHealth {
            kind: self.kind(),
            available: true,
            authenticated: true,
            executable_path: path_text(&self.executable).ok(),
            version: None,
            diagnostic: None,
            capabilities: ProviderCapabilities {
                interactive: true,
                ..ProviderCapabilities::default()
            },
            checked_at: now_millis(),
        }
    }

    fn build_launch(&self, context: LaunchContext) -> sylvops_core::Result<LaunchSpec> {
        #[cfg(windows)]
        let arguments = vec![OsString::from("/D")];
        #[cfg(unix)]
        let arguments = Vec::new();
        Ok(LaunchSpec {
            executable: self.executable.clone(),
            // `/D` keeps cmd.exe interactive while disabling user AutoRun entries that can
            // silently replace the daemon-supplied worktree working directory.
            arguments,
            environment: safe_environment(context.session_id, context.worktree_id, false),
        })
    }

    fn build_resume(&self, _context: ResumeContext) -> sylvops_core::Result<Option<LaunchSpec>> {
        Ok(None)
    }

    fn install_status_hooks(
        &self,
        _worktree: &Path,
        _endpoint: &HookEndpoint,
    ) -> sylvops_core::Result<HookInstallation> {
        Ok(HookInstallation {
            profile_name: String::new(),
            owned_paths: Vec::new(),
        })
    }
}

#[derive(Debug)]
struct CodexAdapter {
    executable: Option<PathBuf>,
    discovery_error: Option<String>,
}

impl CodexAdapter {
    fn discover() -> Self {
        match resolve_path_executable("codex") {
            Ok(path) => Self {
                executable: Some(path),
                discovery_error: None,
            },
            Err(error) => Self {
                executable: None,
                discovery_error: Some(error.to_string()),
            },
        }
    }

    fn executable(&self) -> sylvops_core::Result<&Path> {
        self.executable.as_deref().ok_or_else(|| {
            provider_error(
                self.discovery_error
                    .clone()
                    .unwrap_or_else(|| "Codex executable is unavailable".into()),
            )
        })
    }
}

#[async_trait]
impl ProviderAdapter for CodexAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Codex
    }

    async fn probe(&self) -> ProviderHealth {
        let capabilities = ProviderCapabilities {
            interactive: true,
            resume: true,
            status_hooks: true,
            model_selection: true,
            effort_selection: true,
        };
        let Some(executable) = self.executable.as_deref() else {
            return ProviderHealth {
                kind: self.kind(),
                available: false,
                authenticated: false,
                executable_path: None,
                version: None,
                diagnostic: self.discovery_error.clone(),
                capabilities,
                checked_at: now_millis(),
            };
        };
        let version = run_probe(executable, &["--version"]).await;
        let authentication = run_probe(executable, &["login", "status"]).await;
        let version_error = version.as_ref().err().cloned();
        let authentication_error = authentication.as_ref().err().cloned();
        ProviderHealth {
            kind: self.kind(),
            available: version.is_ok(),
            authenticated: authentication.is_ok(),
            executable_path: path_text(executable).ok(),
            version: version.ok(),
            diagnostic: authentication_error.or(version_error),
            capabilities,
            checked_at: now_millis(),
        }
    }

    fn build_launch(&self, context: LaunchContext) -> sylvops_core::Result<LaunchSpec> {
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
        if let Some(prompt) = context.initial_prompt {
            arguments.push(OsString::from(prompt));
        }
        Ok(LaunchSpec {
            executable: self.executable()?.to_path_buf(),
            arguments,
            environment: safe_environment(context.session_id, context.worktree_id, true),
        })
    }

    fn build_resume(&self, context: ResumeContext) -> sylvops_core::Result<Option<LaunchSpec>> {
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
            executable: self.executable()?.to_path_buf(),
            arguments,
            environment: safe_environment(context.session_id, context.worktree_id, true),
        }))
    }

    fn install_status_hooks(
        &self,
        _worktree: &Path,
        endpoint: &HookEndpoint,
    ) -> sylvops_core::Result<HookInstallation> {
        if self.executable.is_none() {
            return Ok(HookInstallation {
                profile_name: endpoint.profile_name.clone(),
                owned_paths: Vec::new(),
            });
        }
        let directory = codex_home()?;
        std::fs::create_dir_all(&directory).map_err(|error| {
            provider_error(format!(
                "cannot create Codex configuration directory: {error}"
            ))
        })?;
        cleanup_stale_profiles(&directory);
        let profile_path = directory.join(format!("{}.config.toml", endpoint.profile_name));
        if profile_path.exists() {
            return Err(provider_error(
                "refusing to overwrite an existing Codex profile",
            ));
        }
        let command = hook_command(&endpoint.relay_executable)?;
        let source = hook_profile_source(&command);
        let temporary = profile_path.with_extension("config.toml.tmp");
        std::fs::write(&temporary, source)
            .map_err(|error| provider_error(format!("cannot write Codex hook profile: {error}")))?;
        std::fs::rename(&temporary, &profile_path).map_err(|error| {
            provider_error(format!("cannot install Codex hook profile: {error}"))
        })?;
        Ok(HookInstallation {
            profile_name: endpoint.profile_name.clone(),
            owned_paths: vec![profile_path],
        })
    }
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
    let mut child = Command::new(executable);
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

fn resolve_path_executable(name: &str) -> sylvops_core::Result<PathBuf> {
    let path = std::env::var_os("PATH").ok_or_else(|| provider_error("PATH is unavailable"))?;
    #[cfg(windows)]
    let extensions = ["exe", "com"];
    #[cfg(unix)]
    let extensions = [""];
    for directory in std::env::split_paths(&path) {
        for extension in extensions {
            let candidate = if extension.is_empty() {
                directory.join(name)
            } else {
                directory.join(format!("{name}.{extension}"))
            };
            if executable_candidate(&candidate) {
                let canonical = std::fs::canonicalize(candidate).map_err(|error| {
                    provider_error(format!("cannot canonicalize provider executable: {error}"))
                })?;
                if executable_candidate(&canonical) {
                    return Ok(canonical);
                }
            }
        }
    }
    Err(provider_error(format!(
        "{name} was not found as a native executable on PATH"
    )))
}

fn executable_candidate(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(windows)]
    {
        true
    }
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
            .build_launch(LaunchContext {
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
            })
            .expect("shell launch specification");
        #[cfg(windows)]
        assert_eq!(spec.arguments, vec![OsString::from("/D")]);
        #[cfg(unix)]
        assert!(spec.arguments.is_empty());
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
        let adapter = CodexAdapter {
            executable: Some(PathBuf::from(if cfg!(windows) {
                r"C:\Program Files\Codex\codex.exe"
            } else {
                "/usr/local/bin/codex"
            })),
            discovery_error: None,
        };
        let worktree = PathBuf::from(if cfg!(windows) {
            r"C:\work trees\feature"
        } else {
            "/tmp/work trees/feature"
        });
        let spec = adapter
            .build_launch(LaunchContext {
                session_id: sylvops_core::ids::SessionId::new(),
                worktree_id: sylvops_core::ids::WorktreeId::new(),
                cwd: worktree.clone(),
                model: Some("gpt-test".into()),
                effort: Some("high".into()),
                initial_prompt: Some("fix the parser; do not invoke a shell".into()),
            })
            .expect("launch specification");
        assert_eq!(spec.arguments[0], OsString::from("--cd"));
        assert_eq!(spec.arguments[1], worktree.into_os_string());
        assert_eq!(spec.arguments[2], OsString::from("--model"));
        assert_eq!(spec.arguments[3], OsString::from("gpt-test"));
        assert_eq!(
            spec.arguments.last(),
            Some(&OsString::from("fix the parser; do not invoke a shell"))
        );
    }

    #[test]
    fn codex_resume_validates_external_id() {
        let adapter = CodexAdapter {
            executable: Some(PathBuf::from(if cfg!(windows) {
                r"C:\Codex\codex.exe"
            } else {
                "/usr/bin/codex"
            })),
            discovery_error: None,
        };
        let result = adapter.build_resume(ResumeContext {
            session_id: sylvops_core::ids::SessionId::new(),
            worktree_id: sylvops_core::ids::WorktreeId::new(),
            external_session_id: "../../wrong-worktree".into(),
            cwd: PathBuf::from("worktree"),
            model: None,
            effort: None,
        });
        assert!(result.is_err());
    }
}
