//! Bounded, non-sensitive state shared by replaceable user interfaces.

use serde::{Deserialize, Serialize};

use crate::ids::{ProjectId, SessionId, WorktreeId};

/// Main content shown beside the explorer.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MainTab {
    #[default]
    Terminal,
    Changes,
    Details,
}

/// Navigation state safe to persist between TUI runs.
///
/// Deliberately excludes form values, searches, terminal data, and credentials.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct TuiState {
    pub selected_project_id: Option<ProjectId>,
    pub selected_worktree_id: Option<WorktreeId>,
    pub selected_session_id: Option<SessionId>,
    pub selected_main_tab: MainTab,
}
