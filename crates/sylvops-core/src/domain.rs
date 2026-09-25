//! Persisted domain entities and snapshot types.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};

use crate::ids::{ProjectId, ProviderProfileId, SessionId, WorkspaceId, WorktreeId};

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
pub struct GitWorktreeState {
    pub worktree_id: WorktreeId,
    pub head_commit: String,
    pub branch: Option<String>,
    pub tracked_changes: u32,
    pub untracked_files: u32,
    pub ignored_files: u32,
    pub clean: bool,
    pub removal_confirmation_token: Option<String>,
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
    pub provider_profiles: Vec<ProviderProfile>,
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
            | SessionState::Disconnected
    )
}

#[cfg(test)]
mod tests {
    use super::{SessionState, state_allows_resume};

    #[test]
    fn only_inactive_provider_states_allow_resume() {
        for state in [
            SessionState::FinishedSeen,
            SessionState::FinishedUnseen,
            SessionState::Failed,
            SessionState::Disconnected,
        ] {
            assert!(state_allows_resume(state), "{state} should allow resume");
        }
        for state in [
            SessionState::Fresh,
            SessionState::Starting,
            SessionState::Running,
            SessionState::NeedsFeedback,
            SessionState::Terminated,
        ] {
            assert!(
                !state_allows_resume(state),
                "{state} should not allow resume"
            );
        }
    }
}
