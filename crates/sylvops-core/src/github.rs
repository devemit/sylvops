//! Validated GitHub connection and Project-association contracts.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};

use crate::ids::ProjectId;

pub const MAX_GITHUB_HOST_LENGTH: usize = 253;
pub const MAX_GITHUB_OWNER_LENGTH: usize = 100;
pub const MAX_GITHUB_REPOSITORY_LENGTH: usize = 100;
pub const MAX_GIT_REMOTE_NAME_LENGTH: usize = 128;
pub const MAX_GITHUB_DIAGNOSTIC_LENGTH: usize = 512;
pub const MAX_GITHUB_REMOTES: usize = 64;
pub const MAX_GITHUB_CONNECTIONS: usize = 64;

macro_rules! validated_string {
    ($name:ident, $validator:ident) => {
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            /// Validates and constructs this bounded GitHub identity value.
            ///
            /// # Errors
            ///
            /// Returns an error when the value is empty, unbounded, or contains characters that
            /// are unsafe for its identity role.
            pub fn new(value: impl Into<String>) -> Result<Self, String> {
                let value = value.into();
                $validator(&value).map(|normalized| Self(normalized))
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = String;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl FromStr for $name {
            type Err = String;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }
    };
}

validated_string!(GitHubHost, validate_host);
validated_string!(GitHubOwner, validate_owner);
validated_string!(GitHubRepositoryName, validate_repository);
validated_string!(GitRemoteName, validate_remote_name);

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct GitHubRepositoryIdentity {
    host: GitHubHost,
    owner: GitHubOwner,
    repository: GitHubRepositoryName,
}

impl GitHubRepositoryIdentity {
    #[must_use]
    pub const fn new(
        host: GitHubHost,
        owner: GitHubOwner,
        repository: GitHubRepositoryName,
    ) -> Self {
        Self {
            host,
            owner,
            repository,
        }
    }

    #[must_use]
    pub const fn host(&self) -> &GitHubHost {
        &self.host
    }

    #[must_use]
    pub const fn owner(&self) -> &GitHubOwner {
        &self.owner
    }

    #[must_use]
    pub const fn repository(&self) -> &GitHubRepositoryName {
        &self.repository
    }
}

impl fmt::Display for GitHubRepositoryIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}/{}/{}",
            self.host, self.owner, self.repository
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GitHubRemote {
    pub name: GitRemoteName,
    pub repository: GitHubRepositoryIdentity,
}

impl fmt::Display for GitHubRemote {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} ({})", self.name, self.repository)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GitHubAssociation {
    pub project_id: ProjectId,
    pub remote_name: GitRemoteName,
    pub repository: GitHubRepositoryIdentity,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitHubConnectionState {
    Ready,
    MissingCli,
    AuthenticationRequired,
    HostUnavailable,
    PermissionFailure,
    RateLimited,
    NetworkFailure,
    TimedOut,
    InvalidResponse,
}

impl GitHubConnectionState {
    #[must_use]
    pub const fn display_name(self) -> &'static str {
        self.metadata().0
    }

    #[must_use]
    pub const fn diagnostic(self) -> &'static str {
        self.metadata().1
    }

    const fn metadata(self) -> (&'static str, &'static str) {
        match self {
            Self::Ready => (
                "Ready",
                "GitHub CLI is authenticated and the repository is accessible.",
            ),
            Self::MissingCli => (
                "GitHub CLI not found",
                "GitHub CLI was not found as a native executable.",
            ),
            Self::AuthenticationRequired => (
                "Authentication required",
                "Authenticate with GitHub CLI outside SylvOps, then refresh.",
            ),
            Self::HostUnavailable => ("Host unavailable", "GitHub CLI could not access this host."),
            Self::PermissionFailure => (
                "Permission failure",
                "The authenticated GitHub account cannot access this repository.",
            ),
            Self::RateLimited => ("Rate limited", "GitHub rate limited this connection."),
            Self::NetworkFailure => (
                "Network failure",
                "GitHub could not be reached over the network.",
            ),
            Self::TimedOut => (
                "Timed out",
                "GitHub CLI did not respond before the deadline.",
            ),
            Self::InvalidResponse => (
                "Invalid response",
                "GitHub CLI returned an invalid or oversized response.",
            ),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitHubFailure {
    ProjectUnavailable,
    RepositoryUnavailable,
    InvalidRemote,
    PersistenceFailure,
}

impl GitHubFailure {
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::ProjectUnavailable => "The Project is no longer available.",
            Self::RepositoryUnavailable => {
                "The Project repository could not be inspected for GitHub remotes."
            }
            Self::InvalidRemote => {
                "Select a validated GitHub remote already configured in the Project repository."
            }
            Self::PersistenceFailure => "The GitHub Project association could not be stored.",
        }
    }

    #[must_use]
    pub const fn retryable(self) -> bool {
        matches!(
            self,
            Self::ProjectUnavailable | Self::RepositoryUnavailable | Self::PersistenceFailure
        )
    }
}

impl fmt::Display for GitHubFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GitHubConnection {
    pub host: GitHubHost,
    pub state: GitHubConnectionState,
    #[serde(deserialize_with = "deserialize_diagnostic")]
    pub diagnostic: String,
    pub checked_at: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GitHubIntegrationStatus {
    #[serde(deserialize_with = "deserialize_executable_path")]
    pub executable_path: Option<String>,
    #[serde(deserialize_with = "deserialize_connections")]
    pub connections: Vec<GitHubConnection>,
    #[serde(deserialize_with = "deserialize_remotes")]
    pub remotes: Vec<GitHubRemote>,
    pub association: Option<GitHubAssociation>,
}

/// Parses a GitHub HTTPS, SSH URL, or SSH scp-style remote without retaining credentials or URL
/// text.
///
/// # Errors
///
/// Returns an error for unbounded, credentialed, unsupported, or malformed remote URLs and for
/// invalid host, owner, or repository identity values.
pub fn parse_github_remote(remote: &str) -> Result<GitHubRepositoryIdentity, String> {
    if remote.is_empty() || remote.len() > 4_096 || remote.chars().any(char::is_control) {
        return Err("GitHub remote URL is invalid".into());
    }
    let (host, path) = if remote.contains("://") {
        let parsed = url::Url::parse(remote).map_err(|_| "GitHub remote URL is invalid")?;
        let username_valid = match parsed.scheme() {
            "https" => parsed.username().is_empty(),
            "ssh" => parsed.username() == "git",
            _ => false,
        };
        if !username_valid
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
            || parsed.port().is_some()
        {
            return Err("GitHub remote URL is invalid".into());
        }
        let host = parsed
            .host_str()
            .ok_or("GitHub remote URL has no host")?
            .to_owned();
        (host, parsed.path().trim_start_matches('/').to_owned())
    } else {
        let (authority, path) = remote
            .split_once(':')
            .ok_or("GitHub remote URL is invalid")?;
        let (user, host) = authority
            .split_once('@')
            .ok_or("GitHub SSH remote is invalid")?;
        if user != "git" || host.is_empty() || path.is_empty() {
            return Err("GitHub SSH remote is invalid".into());
        }
        (host.to_owned(), path.to_owned())
    };

    let path = path.strip_suffix(".git").unwrap_or(&path);
    let mut segments = path.split('/');
    let owner = segments.next().unwrap_or_default();
    let repository = segments.next().unwrap_or_default();
    if segments.next().is_some() {
        return Err("GitHub remote must identify exactly one owner and repository".into());
    }
    Ok(GitHubRepositoryIdentity::new(
        GitHubHost::new(host)?,
        GitHubOwner::new(owner)?,
        GitHubRepositoryName::new(repository)?,
    ))
}

fn deserialize_diagnostic<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    if value.len() > MAX_GITHUB_DIAGNOSTIC_LENGTH || value.chars().any(char::is_control) {
        return Err(serde::de::Error::custom("GitHub diagnostic is invalid"));
    }
    Ok(value)
}

fn deserialize_executable_path<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    if value
        .as_ref()
        .is_some_and(|path| path.len() > 4_096 || path.chars().any(char::is_control))
    {
        return Err(serde::de::Error::custom(
            "GitHub CLI executable path is invalid",
        ));
    }
    Ok(value)
}

fn deserialize_connections<'de, D>(deserializer: D) -> Result<Vec<GitHubConnection>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let values = Vec::<GitHubConnection>::deserialize(deserializer)?;
    if values.len() > MAX_GITHUB_CONNECTIONS {
        return Err(serde::de::Error::custom("too many GitHub connections"));
    }
    Ok(values)
}

fn deserialize_remotes<'de, D>(deserializer: D) -> Result<Vec<GitHubRemote>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let values = Vec::<GitHubRemote>::deserialize(deserializer)?;
    if values.len() > MAX_GITHUB_REMOTES {
        return Err(serde::de::Error::custom("too many GitHub remotes"));
    }
    Ok(values)
}

fn validate_host(value: &str) -> Result<String, String> {
    let value = value.to_ascii_lowercase();
    if value.is_empty()
        || value.len() > MAX_GITHUB_HOST_LENGTH
        || value.starts_with('.')
        || value.ends_with('.')
        || value.split('.').any(|label| {
            label.is_empty()
                || label.len() > 63
                || label.starts_with('-')
                || label.ends_with('-')
                || !label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
    {
        return Err("GitHub host is invalid".into());
    }
    Ok(value)
}

fn validate_owner(value: &str) -> Result<String, String> {
    validate_path_segment(value, MAX_GITHUB_OWNER_LENGTH, "GitHub owner")
}

fn validate_repository(value: &str) -> Result<String, String> {
    validate_path_segment(value, MAX_GITHUB_REPOSITORY_LENGTH, "GitHub repository")
}

fn validate_path_segment(value: &str, maximum: usize, label: &str) -> Result<String, String> {
    if value.is_empty()
        || value.len() > maximum
        || matches!(value, "." | "..")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(format!("{label} is invalid"));
    }
    Ok(value.to_owned())
}

fn validate_remote_name(value: &str) -> Result<String, String> {
    if value.is_empty()
        || value.len() > MAX_GIT_REMOTE_NAME_LENGTH
        || value.starts_with('-')
        || value.starts_with('/')
        || value.ends_with('/')
        || value.contains("..")
        || value.contains("@{")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'/'))
    {
        return Err("Git remote name is invalid".into());
    }
    Ok(value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_https_and_ssh_github_remotes_into_the_same_identity() {
        let https = parse_github_remote("https://github.example.com/acme/widgets.git").unwrap();
        let ssh = parse_github_remote("git@github.example.com:acme/widgets.git").unwrap();

        assert_eq!(https, ssh);
        assert_eq!(https.host().as_str(), "github.example.com");
        assert_eq!(https.owner().as_str(), "acme");
        assert_eq!(https.repository().as_str(), "widgets");
    }

    #[test]
    fn refuses_credentialed_malformed_and_unbounded_remote_urls() {
        for remote in [
            "https://token@github.com/acme/widgets.git",
            "http://github.com/acme/widgets.git",
            "file:///tmp/widgets.git",
            "git@github.com:acme/widgets/extra.git",
            "git@github.com:acme/widgets.git?token=secret",
            "git@github.com:acme/.git",
        ] {
            assert!(parse_github_remote(remote).is_err(), "accepted {remote}");
        }

        let oversized = format!("https://github.com/{}/widgets.git", "a".repeat(101));
        assert!(parse_github_remote(&oversized).is_err());
    }

    #[test]
    fn validated_identity_values_normalize_hosts_and_reject_controls() {
        assert_eq!(
            GitHubHost::new("GitHub.COM").unwrap().as_str(),
            "github.com"
        );
        assert!(GitHubHost::new("github.com\nmalicious").is_err());
        assert!(GitRemoteName::new("upstream").is_ok());
        assert!(GitRemoteName::new("bad remote").is_err());
    }

    #[test]
    fn integration_status_rejects_unbounded_collections_and_diagnostics() {
        let connection = serde_json::json!({
            "host": "github.com",
            "state": "ready",
            "diagnostic": "ready",
            "checked_at": 1
        });
        let oversized = serde_json::json!({
            "executable_path": null,
            "connections": vec![connection; MAX_GITHUB_CONNECTIONS + 1],
            "remotes": [],
            "association": null
        });
        assert!(serde_json::from_value::<GitHubIntegrationStatus>(oversized).is_err());

        let controlled = serde_json::json!({
            "executable_path": null,
            "connections": [{
                "host": "github.com",
                "state": "ready",
                "diagnostic": "ready\nsecret",
                "checked_at": 1
            }],
            "remotes": [],
            "association": null
        });
        assert!(serde_json::from_value::<GitHubIntegrationStatus>(controlled).is_err());
    }
}
