//! Persisted domain entities and snapshot types.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};

use crate::{
    github::WorkSource,
    ids::{ProjectId, ProviderProfileId, SessionId, WorkspaceId, WorktreeId},
};

pub const SNAPSHOT_WORK_SOURCE_BYTE_BUDGET: usize = 256 * 1_024;
pub const MAX_ENCODED_WORK_SOURCE_BYTES: usize = 1_536;
pub const MAX_SNAPSHOT_WORK_SOURCES: usize =
    SNAPSHOT_WORK_SOURCE_BYTE_BUDGET / MAX_ENCODED_WORK_SOURCE_BYTES;

macro_rules! string_enum {
    ($name:ident { $($variant:ident => $value:literal),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $($variant),+ }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                let value = match self { $(Self::$variant => $value),+ };
                formatter.write_str(value)
            }
        }

        impl FromStr for $name {
            type Err = String;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                match value {
                    $($value => Ok(Self::$variant),)+
                    _ => Err(format!("unknown {} value: {value}", stringify!($name))),
                }
            }
        }
    };
}

string_enum!(ProviderKind {
    Shell => "shell",
    Codex => "codex",
    Claude => "claude",
    Cursor => "cursor",
    Pi => "pi",
});

impl ProviderKind {
    #[must_use]
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Shell => "Shell",
            Self::Codex => "Codex",
            Self::Claude => "Claude Code",
            Self::Cursor => "Cursor",
            Self::Pi => "Pi",
        }
    }
}

#[allow(clippy::derivable_impls)]
impl Default for ProviderKind {
    fn default() -> Self {
        Self::Shell
    }
}

string_enum!(WorktreeStatus {
    Active => "active",
    Creating => "creating",
    Removing => "removing",
    Removed => "removed",
    Missing => "missing",
    Invalid => "invalid",
});

string_enum!(SessionState {
    Fresh => "fresh",
    Starting => "starting",
    Running => "running",
    NeedsFeedback => "needs_feedback",
    FinishedUnseen => "finished_unseen",
    FinishedSeen => "finished_seen",
    Failed => "failed",
    Terminated => "terminated",
    Disconnected => "disconnected",
});

string_enum!(AttachmentRole {
    Controller => "controller",
    Observer => "observer",
});

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub name: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_opened_at: Option<i64>,
    pub is_open: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub id: ProjectId,
    pub workspace_id: WorkspaceId,
    pub name: String,
    pub repository_path: String,
    pub canonical_repository_path: String,
    pub default_branch: Option<String>,
    pub remote_url: Option<String>,
    pub created_at: i64,
    pub last_activity_at: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Worktree {
    pub id: WorktreeId,
    pub project_id: ProjectId,
    pub name: String,
    pub path: String,
    pub canonical_path: String,
    pub branch: Option<String>,
    pub base_ref: String,
    pub base_commit: String,
    pub is_root_checkout: bool,
    pub status: WorktreeStatus,
    pub created_at: i64,
    pub last_activity_at: i64,
    pub removed_at: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum WorktreeBlockingKind {
    Tracked,
    Untracked,
    Ignored,
}

pub const MAX_WORKTREE_BLOCKING_PATHS: usize = 10;
pub const MAX_WORKTREE_BLOCKING_PATH_BYTES: usize = 512;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorktreeBlockingPath {
    pub kind: WorktreeBlockingKind,
    /// Escaped, bounded display text only; never use this value for filesystem operations.
    pub path: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GitWorktreeState {
    pub worktree_id: WorktreeId,
    pub head_commit: String,
    pub branch: Option<String>,
    pub tracked_changes: u32,
    pub untracked_files: u32,
    pub ignored_files: u32,
    #[serde(deserialize_with = "deserialize_worktree_blocking_paths")]
    pub blocking_paths: Vec<WorktreeBlockingPath>,
    pub blocking_paths_truncated: bool,
    pub clean: bool,
    pub removal_confirmation_token: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum WorktreeBaseSelection {
    LatestDefaultBranch {
        remote: Option<crate::github::GitRemoteName>,
    },
    LocalRef {
        reference: String,
    },
}

impl Default for WorktreeBaseSelection {
    fn default() -> Self {
        Self::LatestDefaultBranch { remote: None }
    }
}

impl From<String> for WorktreeBaseSelection {
    fn from(reference: String) -> Self {
        Self::LocalRef { reference }
    }
}

impl From<&str> for WorktreeBaseSelection {
    fn from(reference: &str) -> Self {
        reference.to_owned().into()
    }
}

fn deserialize_worktree_blocking_paths<'de, D>(
    deserializer: D,
) -> Result<Vec<WorktreeBlockingPath>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let paths = Vec::<WorktreeBlockingPath>::deserialize(deserializer)?;
    if paths.len() > MAX_WORKTREE_BLOCKING_PATHS
        || paths.iter().any(|entry| {
            entry.path.len() > MAX_WORKTREE_BLOCKING_PATH_BYTES
                || entry.path.chars().any(char::is_control)
        })
    {
        return Err(serde::de::Error::custom(
            "invalid Worktree blocking path sample",
        ));
    }
    Ok(paths)
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GitDiff {
    pub worktree_id: WorktreeId,
    pub text: String,
    pub truncated: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub worktree_id: WorktreeId,
    pub provider_profile_id: Option<ProviderProfileId>,
    pub provider_kind: ProviderKind,
    pub display_name: String,
    pub state: SessionState,
    /// Expanded authoritative state; `state` remains for clients during the expand phase.
    pub status: crate::session_state::SessionStatus,
    pub process_id: Option<u32>,
    pub external_session_id: Option<String>,
    pub command: String,
    pub arguments_json: String,
    pub cwd: String,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub ended_at: Option<i64>,
    pub last_activity_at: i64,
    pub last_seen_output_sequence: u64,
    pub exit_code: Option<i32>,
    pub failure_reason: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderProfile {
    pub id: ProviderProfileId,
    pub kind: ProviderKind,
    pub display_name: String,
    pub executable_path: Option<String>,
    pub default_model: Option<String>,
    pub default_effort: Option<String>,
    pub enabled: bool,
    pub capabilities_json: String,
    pub last_probe_status: Option<String>,
    pub last_probe_at: Option<i64>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct DaemonSnapshot {
    pub revision: u64,
    pub captured_at: i64,
    pub workspaces: Vec<Workspace>,
    pub projects: Vec<Project>,
    pub worktrees: Vec<Worktree>,
    pub sessions: Vec<Session>,
    #[serde(default, deserialize_with = "deserialize_snapshot_work_sources")]
    pub work_sources: Vec<WorkSource>,
    pub provider_profiles: Vec<ProviderProfile>,
}

fn deserialize_snapshot_work_sources<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<WorkSource>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let sources = Vec::<WorkSource>::deserialize(deserializer)?;
    if sources.len() > MAX_SNAPSHOT_WORK_SOURCES {
        return Err(serde::de::Error::custom(
            "too many Work Sources in daemon snapshot",
        ));
    }
    Ok(sources)
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DaemonHealth {
    pub daemon_version: String,
    pub process_id: u32,
    pub protocol_major: u16,
    pub protocol_minor: u16,
    pub uptime_seconds: u64,
    pub database_ready: bool,
    pub connected_clients: u32,
}

#[must_use]
pub const fn attention_priority(state: SessionState) -> u8 {
    match state {
        SessionState::NeedsFeedback => 0,
        SessionState::Failed => 1,
        SessionState::FinishedUnseen => 2,
        SessionState::Running | SessionState::Starting => 3,
        _ => 4,
    }
}

/// Whether a Session belongs in cross-client active-session supervision.
#[must_use]
pub const fn state_is_active(state: SessionState) -> bool {
    matches!(
        state,
        SessionState::Starting | SessionState::Running | SessionState::NeedsFeedback
    )
}

/// Stable ordering for the Active Sessions overview.
#[must_use]
pub const fn active_session_priority(state: SessionState) -> u8 {
    match state {
        SessionState::NeedsFeedback => 0,
        SessionState::Running => 1,
        SessionState::Starting => 2,
        _ => 3,
    }
}

/// Whether a terminal session state is eligible for provider-level resume.
///
/// A verified external session identifier is still required by the daemon.
#[must_use]
pub const fn state_allows_resume(state: SessionState) -> bool {
    matches!(
        state,
        SessionState::FinishedSeen
            | SessionState::FinishedUnseen
            | SessionState::Failed
            | SessionState::Terminated
            | SessionState::Disconnected
    )
}

/// Whether a Session state is eligible for permanent metadata deletion.
///
/// The daemon must additionally prove that no owned process tree remains.
#[must_use]
pub const fn state_allows_deletion(state: SessionState) -> bool {
    matches!(
        state,
        SessionState::FinishedSeen
            | SessionState::FinishedUnseen
            | SessionState::Failed
            | SessionState::Terminated
            | SessionState::Disconnected
    )
}

/// Whether a session is the latest eligible record for a verified provider conversation.
///
/// Clients must additionally check the current Provider health and resume capability.
#[must_use]
pub fn session_record_can_resume(session: &Session, sessions: &[Session]) -> bool {
    let Some(external_session_id) = session.external_session_id.as_deref() else {
        return false;
    };
    state_allows_resume(session.state)
        && !sessions.iter().any(|candidate| {
            candidate.id != session.id
                && candidate.provider_kind == session.provider_kind
                && candidate.worktree_id == session.worktree_id
                && candidate.external_session_id.as_deref() == Some(external_session_id)
                && (candidate.created_at, candidate.id.as_uuid())
                    > (session.created_at, session.id.as_uuid())
        })
}

#[cfg(test)]
mod tests {
    use super::{
        DaemonSnapshot, MAX_ENCODED_WORK_SOURCE_BYTES, MAX_SNAPSHOT_WORK_SOURCES, ProviderKind,
        SNAPSHOT_WORK_SOURCE_BYTE_BUDGET, SessionState, active_session_priority,
        state_allows_deletion, state_allows_resume, state_is_active,
    };
    use crate::{
        github::{
            GitHubHost, GitHubItemIdentity, GitHubItemKind, GitHubItemNumber, GitHubOwner,
            GitHubRepositoryIdentity, GitHubRepositoryName, WorkSource,
        },
        ids::{ProjectId, WorktreeId},
    };

    #[test]
    fn provider_display_names_are_distinct_from_protocol_values() {
        assert_eq!(ProviderKind::Claude.to_string(), "claude");
        assert_eq!(ProviderKind::Claude.display_name(), "Claude Code");
    }

    #[test]
    fn only_inactive_provider_states_allow_resume() {
        for state in [
            SessionState::FinishedSeen,
            SessionState::FinishedUnseen,
            SessionState::Failed,
            SessionState::Terminated,
            SessionState::Disconnected,
        ] {
            assert!(state_allows_resume(state), "{state} should allow resume");
        }
        for state in [
            SessionState::Fresh,
            SessionState::Starting,
            SessionState::Running,
            SessionState::NeedsFeedback,
        ] {
            assert!(
                !state_allows_resume(state),
                "{state} should not allow resume"
            );
        }
    }

    #[test]
    fn only_inactive_terminal_states_allow_deletion() {
        for state in [
            SessionState::FinishedSeen,
            SessionState::FinishedUnseen,
            SessionState::Failed,
            SessionState::Terminated,
            SessionState::Disconnected,
        ] {
            assert!(
                state_allows_deletion(state),
                "{state} should allow deletion"
            );
        }
        for state in [
            SessionState::Fresh,
            SessionState::Starting,
            SessionState::Running,
            SessionState::NeedsFeedback,
        ] {
            assert!(
                !state_allows_deletion(state),
                "{state} should not allow deletion"
            );
        }
    }

    #[test]
    fn active_session_states_have_a_stable_supervision_order() {
        let active = [
            SessionState::NeedsFeedback,
            SessionState::Running,
            SessionState::Starting,
        ];
        for state in active {
            assert!(state_is_active(state));
        }
        assert_eq!(active_session_priority(SessionState::NeedsFeedback), 0);
        assert_eq!(active_session_priority(SessionState::Running), 1);
        assert_eq!(active_session_priority(SessionState::Starting), 2);
        assert!(!state_is_active(SessionState::FinishedSeen));
        assert_eq!(active_session_priority(SessionState::FinishedSeen), 3);
    }

    #[test]
    fn daemon_snapshot_rejects_unbounded_work_sources() {
        let project_id = ProjectId::new();
        let item = GitHubItemIdentity::new(
            GitHubRepositoryIdentity::new(
                GitHubHost::new("github.example.com").unwrap(),
                GitHubOwner::new("acme").unwrap(),
                GitHubRepositoryName::new("widgets").unwrap(),
            ),
            GitHubItemKind::Issue,
            GitHubItemNumber::new(128).unwrap(),
        );
        let source = WorkSource {
            worktree_id: WorktreeId::new(),
            project_id,
            canonical_url: item.browser_target(),
            item,
            created_at: 1,
        };
        let value = serde_json::json!({
            "revision": 0,
            "captured_at": 0,
            "workspaces": [],
            "projects": [],
            "worktrees": [],
            "sessions": [],
            "work_sources": vec![source; MAX_SNAPSHOT_WORK_SOURCES + 1],
            "provider_profiles": [],
        });

        assert!(serde_json::from_value::<DaemonSnapshot>(value).is_err());
    }

    #[test]
    fn maximum_snapshot_work_sources_stay_within_their_frame_budget() {
        let host = [
            "a".repeat(63),
            "b".repeat(63),
            "c".repeat(63),
            "d".repeat(61),
        ]
        .join(".");
        let project_id = ProjectId::new();
        let item = GitHubItemIdentity::new(
            GitHubRepositoryIdentity::new(
                GitHubHost::new(host).unwrap(),
                GitHubOwner::new("o".repeat(100)).unwrap(),
                GitHubRepositoryName::new("r".repeat(100)).unwrap(),
            ),
            GitHubItemKind::Issue,
            GitHubItemNumber::new(u64::MAX).unwrap(),
        );
        let source = WorkSource {
            worktree_id: WorktreeId::new(),
            project_id,
            canonical_url: item.browser_target(),
            item,
            created_at: i64::MAX,
        };

        assert!(rmp_serde::to_vec_named(&source).unwrap().len() <= MAX_ENCODED_WORK_SOURCE_BYTES);
        assert!(
            rmp_serde::to_vec_named(&vec![source; MAX_SNAPSHOT_WORK_SOURCES])
                .unwrap()
                .len()
                <= SNAPSHOT_WORK_SOURCE_BYTE_BUDGET
        );
    }
}
