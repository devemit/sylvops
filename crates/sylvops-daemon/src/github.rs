//! Daemon-owned, bounded access to an externally authenticated GitHub CLI.

use std::{
    collections::BTreeSet,
    fs::File,
    io::Read as _,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use sylvops_core::github::{
    GitHubConnection, GitHubConnectionState, GitHubHost, GitHubRepositoryIdentity,
    MAX_GITHUB_CONNECTIONS,
};
use tokio::{io::AsyncReadExt as _, time::timeout};

use crate::background_process;

const GH_TIMEOUT: Duration = Duration::from_secs(5);
const GH_OUTPUT_LIMIT: u64 = 64 * 1024;
const MAX_PATH_DIRECTORIES: usize = 128;

#[derive(Clone, Debug)]
pub struct GitHubGateway {
    executable: Option<PathBuf>,
}

impl GitHubGateway {
    #[must_use]
    pub fn discover() -> Self {
        Self {
            executable: discover_gh_executable(),
        }
    }

    /// Creates a gateway around a specific native executable for deterministic integration tests.
    #[doc(hidden)]
    pub fn with_executable(executable: &Path) -> Result<Self, String> {
        let executable = native_executable(executable)
            .ok_or_else(|| "GitHub CLI path is not a native executable".to_owned())?;
        Ok(Self {
            executable: Some(executable),
        })
    }

    #[must_use]
    pub fn executable_path(&self) -> Option<String> {
        self.executable
            .as_ref()
            .and_then(|path| path.to_str())
            .filter(|path| path.len() <= 4_096 && !path.chars().any(char::is_control))
            .map(str::to_owned)
    }

    pub async fn probe(&self, repositories: &[GitHubRepositoryIdentity]) -> Vec<GitHubConnection> {
        let mut hosts = BTreeSet::new();
        if let Ok(host) = GitHubHost::new("github.com") {
            hosts.insert(host);
        }
        for repository in repositories.iter().take(MAX_GITHUB_CONNECTIONS) {
            hosts.insert(repository.host().clone());
        }

        let mut connections = Vec::with_capacity(hosts.len());
        for host in hosts.into_iter().take(MAX_GITHUB_CONNECTIONS) {
            let repository = repositories
                .iter()
                .find(|repository| repository.host() == &host);
            connections.push(self.probe_host(host, repository).await);
        }
        connections
    }

    async fn probe_host(
        &self,
        host: GitHubHost,
        repository: Option<&GitHubRepositoryIdentity>,
    ) -> GitHubConnection {
        let Some(executable) = self.executable.as_deref() else {
            return connection(
                host,
                GitHubConnectionState::MissingCli,
                "Install GitHub CLI and authenticate outside SylvOps.",
            );
        };
        if native_executable(executable).as_deref() != Some(executable) {
            return connection(
                host,
                GitHubConnectionState::MissingCli,
                "The discovered GitHub CLI executable changed; refresh after reinstalling it.",
            );
        }

        let host_text = host.as_str();
        let authentication = run_gh(
            executable,
            &["api", "--hostname", host_text, "user", "--jq", ".login"],
        )
        .await;
        let state = match authentication {
            Ok(output) if output.success => GitHubConnectionState::Ready,
            Ok(output) => classify_failure(&output),
            Err(CaptureError::TimedOut) => GitHubConnectionState::TimedOut,
            Err(CaptureError::Oversized) => GitHubConnectionState::InvalidResponse,
            Err(CaptureError::Spawn) => GitHubConnectionState::MissingCli,
        };
        if state != GitHubConnectionState::Ready {
            return connection(host, state, state.diagnostic());
        }

        if let Some(repository) = repository {
            let endpoint = format!(
                "repos/{}/{}",
                repository.owner().as_str(),
                repository.repository().as_str()
            );
            match run_gh(
                executable,
                &[
                    "api",
                    "--hostname",
                    host_text,
                    endpoint.as_str(),
                    "--silent",
                ],
            )
            .await
            {
                Ok(output) if output.success => {}
                Ok(output) => {
                    let state = classify_repository_failure(&output);
                    return connection(host, state, state.diagnostic());
                }
                Err(CaptureError::TimedOut) => {
                    return connection(
                        host,
                        GitHubConnectionState::TimedOut,
                        GitHubConnectionState::TimedOut.diagnostic(),
                    );
                }
                Err(CaptureError::Oversized) => {
                    return connection(
                        host,
                        GitHubConnectionState::InvalidResponse,
                        GitHubConnectionState::InvalidResponse.diagnostic(),
                    );
                }
                Err(CaptureError::Spawn) => {
                    return connection(
                        host,
                        GitHubConnectionState::MissingCli,
                        GitHubConnectionState::MissingCli.diagnostic(),
                    );
                }
            }
        }
        connection(
            host,
            GitHubConnectionState::Ready,
            GitHubConnectionState::Ready.diagnostic(),
        )
    }
}

fn connection(
    host: GitHubHost,
    state: GitHubConnectionState,
    diagnostic: &str,
) -> GitHubConnection {
    GitHubConnection {
        host,
        state,
        diagnostic: diagnostic.to_owned(),
        checked_at: now_millis(),
    }
}

#[derive(Debug)]
struct CommandOutput {
    success: bool,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CaptureError {
    Spawn,
    TimedOut,
    Oversized,
}

async fn run_gh(executable: &Path, arguments: &[&str]) -> Result<CommandOutput, CaptureError> {
    let mut command = background_process::command(executable);
    command
        .args(arguments)
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .env_remove("GH_ENTERPRISE_TOKEN")
        .env_remove("GITHUB_ENTERPRISE_TOKEN")
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(|_| CaptureError::Spawn)?;
    let stdout = child.stdout.take().ok_or(CaptureError::Spawn)?;
    let stderr = child.stderr.take().ok_or(CaptureError::Spawn)?;
    let capture = async move {
        let stdout_task = tokio::spawn(read_bounded(stdout));
        let stderr_task = tokio::spawn(read_bounded(stderr));
        let status = child.wait().await.map_err(|_| CaptureError::Spawn)?;
        let stdout = stdout_task.await.map_err(|_| CaptureError::Spawn)??;
        let stderr = stderr_task.await.map_err(|_| CaptureError::Spawn)??;
        Ok(CommandOutput {
            success: status.success(),
            stdout,
            stderr,
        })
    };
    timeout(GH_TIMEOUT, capture)
        .await
        .map_err(|_| CaptureError::TimedOut)?
}

async fn read_bounded<R: tokio::io::AsyncRead + Unpin>(reader: R) -> Result<Vec<u8>, CaptureError> {
    let mut bytes = Vec::new();
    reader
        .take(GH_OUTPUT_LIMIT + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| CaptureError::Spawn)?;
    if bytes.len() as u64 > GH_OUTPUT_LIMIT {
        return Err(CaptureError::Oversized);
    }
    Ok(bytes)
}

fn classify_failure(output: &CommandOutput) -> GitHubConnectionState {
    classify_text(output, false)
}

fn classify_repository_failure(output: &CommandOutput) -> GitHubConnectionState {
    classify_text(output, true)
}

fn classify_text(output: &CommandOutput, repository_probe: bool) -> GitHubConnectionState {
    let mut bytes = Vec::with_capacity(output.stdout.len().saturating_add(output.stderr.len()));
    bytes.extend_from_slice(&output.stdout);
    bytes.extend_from_slice(&output.stderr);
    let text = String::from_utf8_lossy(&bytes).to_ascii_lowercase();
    if text.contains("rate limit") || text.contains("api rate") || text.contains("429") {
        GitHubConnectionState::RateLimited
    } else if text.contains("not logged")
        || text.contains("authentication")
        || text.contains("authenticate")
        || text.contains("bad credentials")
        || text.contains("401")
    {
        GitHubConnectionState::AuthenticationRequired
    } else if repository_probe
        && (text.contains("403")
            || text.contains("404")
            || text.contains("forbidden")
            || text.contains("permission")
            || text.contains("not found"))
    {
        GitHubConnectionState::PermissionFailure
    } else if text.contains("resolve host")
        || text.contains("network")
        || text.contains("connection")
        || text.contains("tls")
        || text.contains("offline")
    {
        GitHubConnectionState::NetworkFailure
    } else {
        GitHubConnectionState::HostUnavailable
    }
}

fn discover_gh_executable() -> Option<PathBuf> {
    let executable_name = if cfg!(windows) { "gh.exe" } else { "gh" };
    let path_candidates = std::env::var_os("PATH")
        .map(|value| {
            std::env::split_paths(&value)
                .filter(|path| path.is_absolute())
                .take(MAX_PATH_DIRECTORIES)
                .map(|path| path.join(executable_name))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut candidates = Vec::new();
    if let Some(explicit) = std::env::var_os("SYLVOPS_GH_CLI_PATH") {
        let explicit = PathBuf::from(explicit);
        if explicit.is_absolute() {
            candidates.push(explicit);
        }
    }
    candidates.extend(path_candidates);
    #[cfg(windows)]
    if let Some(program_files) = std::env::var_os("ProgramFiles") {
        candidates.push(
            PathBuf::from(program_files)
                .join("GitHub CLI")
                .join("gh.exe"),
        );
    }
    #[cfg(unix)]
    candidates.extend([
        PathBuf::from("/usr/bin/gh"),
        PathBuf::from("/usr/local/bin/gh"),
        PathBuf::from("/opt/homebrew/bin/gh"),
    ]);
    candidates
        .into_iter()
        .find_map(|candidate| native_executable(&candidate))
}

fn native_executable(candidate: &Path) -> Option<PathBuf> {
    let metadata = std::fs::metadata(candidate).ok()?;
    if !metadata.is_file() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o111 == 0 {
            return None;
        }
    }
    let canonical = std::fs::canonicalize(candidate).ok()?;
    let mut file = File::open(&canonical).ok()?;
    let mut header = [0_u8; 4];
    file.read_exact(&mut header).ok()?;
    let native = if cfg!(windows) {
        header[..2] == *b"MZ"
    } else {
        header == *b"\x7fELF"
            || matches!(
                header,
                [0xfe, 0xed, 0xfa, 0xcf]
                    | [0xcf, 0xfa, 0xed, 0xfe]
                    | [0xca, 0xfe, 0xba, 0xbe | 0xbf]
            )
    };
    if !native || std::fs::canonicalize(candidate).ok()? != canonical {
        return None;
    }
    Some(canonical)
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

    fn failed(message: &str) -> CommandOutput {
        CommandOutput {
            success: false,
            stdout: Vec::new(),
            stderr: message.as_bytes().to_vec(),
        }
    }

    #[test]
    fn maps_github_cli_failures_to_typed_connection_states() {
        assert_eq!(
            classify_failure(&failed("not logged into github.example.com")),
            GitHubConnectionState::AuthenticationRequired
        );
        assert_eq!(
            classify_failure(&failed("API rate limit exceeded")),
            GitHubConnectionState::RateLimited
        );
        assert_eq!(
            classify_failure(&failed("could not resolve host")),
            GitHubConnectionState::NetworkFailure
        );
        assert_eq!(
            classify_repository_failure(&failed("HTTP 403 forbidden")),
            GitHubConnectionState::PermissionFailure
        );
        assert_eq!(
            classify_failure(&failed("unsupported hostname")),
            GitHubConnectionState::HostUnavailable
        );
    }

    #[tokio::test]
    async fn missing_cli_is_reported_independently_for_each_host() {
        let gateway = GitHubGateway { executable: None };
        let repository = GitHubRepositoryIdentity::new(
            GitHubHost::new("github.example.com").unwrap(),
            sylvops_core::github::GitHubOwner::new("acme").unwrap(),
            sylvops_core::github::GitHubRepositoryName::new("widgets").unwrap(),
        );

        let connections = gateway.probe(&[repository]).await;

        assert_eq!(connections.len(), 2);
        assert!(connections.iter().all(|connection| {
            connection.state == GitHubConnectionState::MissingCli
                && !connection.diagnostic.contains("token")
        }));
    }
}
