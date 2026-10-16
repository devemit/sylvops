use std::{
    collections::HashSet,
    path::PathBuf,
    time::{Duration, Instant},
};

use ratatui::layout::Rect;
use sylvops_core::{
    domain::{
        DaemonSnapshot, Project, Session, SessionState, Workspace, Worktree, WorktreeStatus,
        active_session_priority, state_is_active,
    },
    github::GitHubIntegrationStatus,
    ids::{ProjectId, SessionId, WorkspaceId, WorktreeId},
    protocol::{
        ProjectRemovalAuthorization, SessionDeletionAuthorization, WorkspaceDeletionAuthorization,
    },
    provider::ProviderHealth,
    ui::{
        MAX_TUI_NAVIGATOR_WIDTH, MIN_TUI_NAVIGATOR_WIDTH, TuiContentView, TuiDensity,
        TuiNavigatorView, TuiSessionPresentation, TuiState, TuiTheme,
    },
};

use crate::{
    directory_browser::DirectoryBrowser, forms::Form, github_modal::GitHubModal,
    terminal::AttachedTerminal,
};

pub(crate) const MAIN_VIEWS: [TuiContentView; 3] = [
    TuiContentView::Terminal,
    TuiContentView::Changes,
    TuiContentView::Details,
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FocusZone {
    Explorer,
    Main,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum NavigatorResizeState {
    #[default]
    Idle,
    Dragging,
}

#[derive(Clone, Debug)]
pub(crate) enum ConfirmAction {
    DeleteWorkspace {
        workspace_id: WorkspaceId,
        token: WorkspaceDeletionAuthorization,
    },
    RemoveProject {
        project_id: ProjectId,
        token: ProjectRemovalAuthorization,
    },
    RemoveWorktree {
        worktree_id: WorktreeId,
        token: String,
    },
    StopSession(SessionId),
    DeleteSession {
        session_id: SessionId,
        token: SessionDeletionAuthorization,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct Confirmation {
    pub title: String,
    pub detail: String,
    pub action: ConfirmAction,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Palette {
    pub query: String,
    pub selected: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SettingsFocus {
    GitHub,
    Theme,
    Presentation,
    Density,
    Apply,
    Cancel,
}

impl SettingsFocus {
    pub(crate) fn moved(self, delta: isize) -> Self {
        let choices = [
            Self::GitHub,
            Self::Theme,
            Self::Presentation,
            Self::Density,
            Self::Apply,
            Self::Cancel,
        ];
        let current = choices
            .iter()
            .position(|choice| *choice == self)
            .unwrap_or(0);
        let next = current
            .saturating_add_signed(delta)
            .min(choices.len().saturating_sub(1));
        choices[next]
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Settings {
    pub original_theme: TuiTheme,
    pub original_presentation: TuiSessionPresentation,
    pub original_density: TuiDensity,
    pub focus: SettingsFocus,
    pub github_remote_index: usize,
}

#[derive(Clone, Debug)]
pub(crate) enum Mode {
    Navigation,
    Modal(Form),
    DirectoryBrowser(DirectoryBrowser),
    Confirmation(Confirmation),
    CommandPalette(Palette),
    Settings(Settings),
    GitHub(Box<GitHubModal>),
    TerminalAttached,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FlashKind {
    Info,
    Success,
    Error,
}

#[derive(Clone, Debug)]
pub(crate) struct Flash {
    pub kind: FlashKind,
    pub text: String,
    pub expires_at: Option<Instant>,
}

impl Flash {
    pub fn transient(kind: FlashKind, text: impl Into<String>) -> Self {
        Self {
            kind,
            text: text.into(),
            expires_at: Some(Instant::now() + Duration::from_secs(4)),
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self {
            kind: FlashKind::Error,
            text: text.into(),
            expires_at: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExplorerNode {
    Workspace(WorkspaceId),
    Project(ProjectId),
    Worktree(WorktreeId),
    Session(SessionId),
}

#[derive(Clone, Debug)]
pub(crate) struct ExplorerRow {
    pub node: ExplorerNode,
    pub depth: u8,
    pub expanded: bool,
    pub expandable: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HitTarget {
    ExplorerRow(usize),
    Tab(TuiContentView),
    ActiveSession(SessionId),
    ActiveSessionOpen(SessionId),
    ActiveSessionStop(SessionId),
    NavigatorExplorer,
    NavigatorActive,
    NavigatorDivider,
    FocusExplorer,
    FocusMain,
    Attach,
    Detach,
    Resume,
    New,
    NewWorktree,
    Rename,
    Delete,
    Diff,
    Attention,
    Settings,
    Commands,
    Help,
    SettingsTheme(TuiTheme),
    SettingsPresentation(TuiSessionPresentation),
    SettingsDensity(TuiDensity),
    SettingsGithubRemote(usize),
    SettingsGithubClear,
    SettingsGithubRefresh,
    SettingsApply,
    SettingsCancel,
    ModalField(usize),
    ModalProvider(usize),
    ModalProviderProbe,
    ModalSubmit,
    ModalCancel,
    ModalBrowse,
    DirectoryBrowserRow(usize),
    DirectoryBrowserSelect,
    DirectoryBrowserCancel,
    Confirm,
    Cancel,
    PaletteRow(usize),
    GitHubIssues,
    GitHubPullRequests,
    GitHubRow(usize),
    GitHubFilter,
    GitHubRefresh,
    GitHubOpen,
    GitHubPrevious,
    GitHubNext,
    GitHubMoreComments,
    GitHubScroll(isize),
    GitHubStartWork,
    GitHubStartAnother,
    GitHubStartSubmit,
    GitHubRetrySession,
    GitHubOpenWorktree,
    GitHubRemoveWorktree,
    GitHubBack,
}

#[derive(Clone, Debug)]
pub(crate) struct HitRegion {
    pub area: Rect,
    pub target: HitTarget,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct HitMap(pub Vec<HitRegion>);

impl HitMap {
    pub fn push(&mut self, area: Rect, target: HitTarget) {
        if area.width > 0 && area.height > 0 {
            self.0.push(HitRegion { area, target });
        }
    }

    pub fn target_at(&self, column: u16, row: u16) -> Option<HitTarget> {
        self.0
            .iter()
            .rev()
            .find(|region| {
                column >= region.area.x
                    && column < region.area.x.saturating_add(region.area.width)
                    && row >= region.area.y
                    && row < region.area.y.saturating_add(region.area.height)
            })
            .map(|region| region.target)
    }
}

pub(crate) struct App {
    pub snapshot: DaemonSnapshot,
    pub providers: Vec<ProviderHealth>,
    pub github_status: Option<GitHubIntegrationStatus>,
    pub mode: Mode,
    pub focus: FocusZone,
    pub main_tab: TuiContentView,
    pub theme: TuiTheme,
    pub session_presentation: TuiSessionPresentation,
    pub navigator_view: TuiNavigatorView,
    pub navigator_width: u16,
    pub navigator_resizing: NavigatorResizeState,
    pub density: TuiDensity,
    pub selected_project_id: Option<ProjectId>,
    pub selected_worktree_id: Option<WorktreeId>,
    pub selected_session_id: Option<SessionId>,
    pub explorer_index: usize,
    pub selected_workspace: bool,
    pub workspace_expanded: bool,
    pub expanded_projects: HashSet<ProjectId>,
    pub expanded_worktrees: HashSet<WorktreeId>,
    pub diff: Option<String>,
    pub diff_scroll: u16,
    pub flash: Option<Flash>,
    pub help: bool,
    pub attached: Option<AttachedTerminal>,
    pub resume_pending: Option<SessionId>,
    pub hit_map: HitMap,
    pub navigation_dirty_at: Option<Instant>,
    pub invocation_directory: PathBuf,
    status_animation_started_at: Instant,
}

impl App {
    #[cfg(test)]
    pub fn new(
        snapshot: DaemonSnapshot,
        providers: Vec<ProviderHealth>,
        persisted: Option<TuiState>,
    ) -> Self {
        let invocation_directory = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self::new_at(snapshot, providers, persisted, invocation_directory)
    }

    pub fn new_at(
        snapshot: DaemonSnapshot,
        providers: Vec<ProviderHealth>,
        persisted: Option<TuiState>,
        invocation_directory: PathBuf,
    ) -> Self {
        let selected_workspace = persisted.as_ref().is_some_and(|state| {
            state.selected_project_id.is_none()
                && state.selected_worktree_id.is_none()
                && state.selected_session_id.is_none()
        });
        let persisted = persisted.unwrap_or_default().normalized();
        let mut app = Self {
            mode: if snapshot.workspaces.is_empty() {
                Mode::Modal(Form::workspace())
            } else {
                Mode::Navigation
            },
            snapshot,
            providers,
            github_status: None,
            focus: FocusZone::Explorer,
            main_tab: persisted.selected_content_view,
            theme: persisted.theme,
            session_presentation: persisted.session_presentation,
            navigator_view: persisted.navigator_view,
            navigator_width: persisted.navigator_width,
            navigator_resizing: NavigatorResizeState::Idle,
            density: persisted.density,
            selected_project_id: persisted.selected_project_id,
            selected_worktree_id: persisted.selected_worktree_id,
            selected_session_id: persisted.selected_session_id,
            explorer_index: 0,
            selected_workspace,
            workspace_expanded: true,
            expanded_projects: HashSet::new(),
            expanded_worktrees: HashSet::new(),
            diff: None,
            diff_scroll: 0,
            flash: None,
            help: false,
            attached: None,
            resume_pending: None,
            hit_map: HitMap::default(),
            navigation_dirty_at: None,
            invocation_directory,
            status_animation_started_at: Instant::now(),
        };
        app.restore_selection();
        app.reconcile_active_session_selection();
        app
    }

    pub fn replace_snapshot(&mut self, snapshot: DaemonSnapshot) {
        let selected_before = self.selected_node();
        let fallback_candidates = self.selection_fallback_candidates();
        self.snapshot = snapshot;
        if let Mode::GitHub(modal) = &mut self.mode {
            modal.sync_local_work(&self.snapshot);
        }

        let attached_missing = self
            .attached
            .as_ref()
            .is_some_and(|attached| !self.node_exists(ExplorerNode::Session(attached.session_id)));
        if attached_missing {
            self.attached = None;
            if matches!(&self.mode, Mode::TerminalAttached) {
                self.mode = Mode::Navigation;
            }
        }

        if self.resume_pending.is_some_and(|session_id| {
            !self
                .snapshot
                .sessions
                .iter()
                .any(|session| session.id == session_id)
        }) {
            self.resume_pending = None;
        }

        let confirmation_target_missing = match &self.mode {
            Mode::Confirmation(confirmation) => match &confirmation.action {
                ConfirmAction::DeleteWorkspace { workspace_id, .. } => !self
                    .snapshot
                    .workspaces
                    .iter()
                    .any(|workspace| workspace.id == *workspace_id),
                ConfirmAction::RemoveProject { project_id, .. } => !self
                    .snapshot
                    .projects
                    .iter()
                    .any(|project| project.id == *project_id),
                ConfirmAction::RemoveWorktree { worktree_id, .. } => {
                    !self.snapshot.worktrees.iter().any(|worktree| {
                        worktree.id == *worktree_id && worktree.status != WorktreeStatus::Removed
                    })
                }
                ConfirmAction::StopSession(session_id)
                | ConfirmAction::DeleteSession { session_id, .. } => !self
                    .snapshot
                    .sessions
                    .iter()
                    .any(|session| session.id == *session_id),
            },
            _ => false,
        };
        if confirmation_target_missing {
            self.mode = Mode::Navigation;
            self.flash = Some(Flash::error(
                "The removal target changed; inspect it again before confirming.",
            ));
        }

        let selected_stale = selected_before.is_some_and(|selected| !self.node_exists(selected));
        let reconciled_selection = if selected_stale {
            self.clear_selection();
            fallback_candidates
                .into_iter()
                .find(|candidate| self.node_exists(*candidate))
                .or_else(|| {
                    self.active_workspace()
                        .map(|workspace| ExplorerNode::Workspace(workspace.id))
                })
        } else {
            selected_before
        };

        self.restore_selection();
        if let Some(selection) = reconciled_selection {
            self.reveal_and_select(selection);
        }
        self.reconcile_active_session_selection();
        if self.snapshot.workspaces.is_empty() && matches!(&self.mode, Mode::Navigation) {
            self.mode = Mode::Modal(Form::workspace());
        }
    }

    pub fn active_workspace(&self) -> Option<&Workspace> {
        self.snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.is_open)
            .or_else(|| self.snapshot.workspaces.first())
    }

    pub fn selected_project(&self) -> Option<&Project> {
        self.selected_project_id
            .and_then(|id| self.snapshot.projects.iter().find(|item| item.id == id))
    }

    pub fn selected_worktree(&self) -> Option<&Worktree> {
        self.selected_worktree_id.and_then(|id| {
            self.snapshot
                .worktrees
                .iter()
                .find(|item| item.id == id && item.status != WorktreeStatus::Removed)
        })
    }

    pub fn selected_session(&self) -> Option<&Session> {
        self.selected_session_id
            .and_then(|id| self.snapshot.sessions.iter().find(|item| item.id == id))
    }

    pub fn visible_projects(&self) -> Vec<&Project> {
        let workspace = self.active_workspace().map(|workspace| workspace.id);
        let mut projects: Vec<_> = self
            .snapshot
            .projects
            .iter()
            .filter(|project| Some(project.workspace_id) == workspace)
            .collect();
        projects.sort_by_key(|project| std::cmp::Reverse(project.last_activity_at));
        projects
    }

    pub fn project_worktrees(&self, project_id: ProjectId) -> Vec<&Worktree> {
        let mut worktrees: Vec<_> = self
            .snapshot
            .worktrees
            .iter()
            .filter(|worktree| {
                worktree.project_id == project_id && worktree.status != WorktreeStatus::Removed
            })
            .collect();
        worktrees.sort_by_key(|worktree| {
            (
                !worktree.is_root_checkout,
                std::cmp::Reverse(worktree.last_activity_at),
            )
        });
        worktrees
    }

    pub fn worktree_sessions(&self, worktree_id: WorktreeId) -> Vec<&Session> {
        let mut sessions: Vec<_> = self
            .snapshot
            .sessions
            .iter()
            .filter(|session| session.worktree_id == worktree_id)
            .collect();
        sessions.sort_by_key(|session| std::cmp::Reverse(session.last_activity_at));
        sessions
    }

    pub fn active_sessions(&self) -> Vec<&Session> {
        let mut sessions: Vec<_> = self
            .snapshot
            .sessions
            .iter()
            .filter(|session| {
                state_is_active(session.state)
                    && self.node_exists(ExplorerNode::Session(session.id))
            })
            .collect();
        sessions.sort_by_key(|session| {
            (
                active_session_priority(session.state),
                std::cmp::Reverse(session.last_activity_at),
            )
        });
        sessions
    }

    pub fn move_active_session(&mut self, delta: isize) {
        let ids = self
            .active_sessions()
            .into_iter()
            .map(|session| session.id)
            .collect::<Vec<_>>();
        if ids.is_empty() {
            return;
        }
        let current = self
            .selected_session_id
            .and_then(|selected| ids.iter().position(|id| *id == selected))
            .unwrap_or(0);
        let next = current
            .saturating_add_signed(delta)
            .min(ids.len().saturating_sub(1));
        self.reveal_and_select(ExplorerNode::Session(ids[next]));
    }

    pub fn select_active_session(&mut self, session_id: SessionId) -> bool {
        if self
            .active_sessions()
            .iter()
            .any(|session| session.id == session_id)
        {
            self.reveal_and_select(ExplorerNode::Session(session_id));
            true
        } else {
            false
        }
    }

    fn reconcile_active_session_selection(&mut self) {
        if self.navigator_view != TuiNavigatorView::Active {
            return;
        }
        let ids = self
            .active_sessions()
            .into_iter()
            .map(|session| session.id)
            .collect::<Vec<_>>();
        if self
            .selected_session_id
            .is_some_and(|selected| ids.contains(&selected))
        {
            return;
        }
        if let Some(session_id) = ids.first().copied() {
            self.reveal_and_select(ExplorerNode::Session(session_id));
        } else {
            self.selected_session_id = None;
            self.sync_explorer_index();
        }
    }

    pub fn explorer_rows(&self) -> Vec<ExplorerRow> {
        let mut rows = Vec::new();
        let Some(workspace) = self.active_workspace() else {
            return rows;
        };
        let projects = self.visible_projects();
        rows.push(ExplorerRow {
            node: ExplorerNode::Workspace(workspace.id),
            depth: 0,
            expanded: self.workspace_expanded,
            expandable: !projects.is_empty(),
        });
        if !self.workspace_expanded {
            return rows;
        }
        for project in projects {
            let project_expanded = self.expanded_projects.contains(&project.id);
            rows.push(ExplorerRow {
                node: ExplorerNode::Project(project.id),
                depth: 1,
                expanded: project_expanded,
                expandable: !self.project_worktrees(project.id).is_empty(),
            });
            if !project_expanded {
                continue;
            }
            for worktree in self.project_worktrees(project.id) {
                let worktree_expanded = self.expanded_worktrees.contains(&worktree.id);
                rows.push(ExplorerRow {
                    node: ExplorerNode::Worktree(worktree.id),
                    depth: 2,
                    expanded: worktree_expanded,
                    expandable: !self.worktree_sessions(worktree.id).is_empty(),
                });
                if !worktree_expanded {
                    continue;
                }
                rows.extend(
                    self.worktree_sessions(worktree.id)
                        .into_iter()
                        .map(|session| ExplorerRow {
                            node: ExplorerNode::Session(session.id),
                            depth: 3,
                            expanded: false,
                            expandable: false,
                        }),
                );
            }
        }
        rows
    }

    pub fn restore_selection(&mut self) {
        if self.active_workspace().is_none() {
            self.selected_workspace = false;
            self.selected_project_id = None;
            self.selected_worktree_id = None;
            self.selected_session_id = None;
            self.explorer_index = 0;
            return;
        }
        if self.selected_workspace {
            self.selected_project_id = None;
            self.selected_worktree_id = None;
            self.selected_session_id = None;
            self.sync_explorer_index();
            return;
        }
        let projects = self.visible_projects();
        let valid_project = self
            .selected_project_id
            .filter(|id| projects.iter().any(|item| item.id == *id))
            .or_else(|| projects.first().map(|item| item.id));
        self.selected_project_id = valid_project;
        let valid_worktree = valid_project.and_then(|project_id| {
            let items = self.project_worktrees(project_id);
            self.selected_worktree_id
                .filter(|id| items.iter().any(|item| item.id == *id))
                .or_else(|| items.first().map(|item| item.id))
        });
        self.selected_worktree_id = valid_worktree;
        self.selected_session_id = valid_worktree.and_then(|worktree_id| {
            let items = self.worktree_sessions(worktree_id);
            self.selected_session_id
                .filter(|id| items.iter().any(|item| item.id == *id))
                .or_else(|| items.first().map(|item| item.id))
        });
        self.selected_workspace = valid_project.is_none();
        if let Some(id) = valid_project {
            self.expanded_projects.insert(id);
        }
        if let Some(id) = valid_worktree {
            self.expanded_worktrees.insert(id);
        }
        self.sync_explorer_index();
    }

    pub fn sync_explorer_index(&mut self) {
        let rows = self.explorer_rows();
        let target = self
            .selected_session_id
            .map(ExplorerNode::Session)
            .or_else(|| self.selected_worktree_id.map(ExplorerNode::Worktree))
            .or_else(|| self.selected_project_id.map(ExplorerNode::Project))
            .or_else(|| {
                self.selected_workspace
                    .then(|| {
                        self.active_workspace()
                            .map(|item| ExplorerNode::Workspace(item.id))
                    })
                    .flatten()
            });
        self.explorer_index = target
            .and_then(|target| rows.iter().position(|row| row.node == target))
            .unwrap_or(0)
            .min(rows.len().saturating_sub(1));
    }

    pub fn select_explorer_index(&mut self, index: usize) {
        let rows = self.explorer_rows();
        let Some(row) = rows.get(index.min(rows.len().saturating_sub(1))) else {
            return;
        };
        self.explorer_index = index.min(rows.len().saturating_sub(1));
        match row.node {
            ExplorerNode::Workspace(_) => {
                self.selected_workspace = true;
                self.selected_project_id = None;
                self.selected_worktree_id = None;
                self.selected_session_id = None;
            }
            ExplorerNode::Project(id) => {
                self.selected_workspace = false;
                self.selected_project_id = Some(id);
                self.selected_worktree_id = None;
                self.selected_session_id = None;
            }
            ExplorerNode::Worktree(id) => {
                self.selected_workspace = false;
                self.selected_worktree_id = Some(id);
                self.selected_session_id = None;
                self.selected_project_id = self
                    .snapshot
                    .worktrees
                    .iter()
                    .find(|item| item.id == id)
                    .map(|item| item.project_id);
            }
            ExplorerNode::Session(id) => {
                self.selected_workspace = false;
                self.selected_session_id = Some(id);
                if let Some(worktree_id) = self
                    .snapshot
                    .sessions
                    .iter()
                    .find(|item| item.id == id)
                    .map(|item| item.worktree_id)
                {
                    self.selected_worktree_id = Some(worktree_id);
                    self.selected_project_id = self
                        .snapshot
                        .worktrees
                        .iter()
                        .find(|item| item.id == worktree_id)
                        .map(|item| item.project_id);
                }
            }
        }
        self.mark_navigation_dirty();
    }

    pub fn move_explorer(&mut self, delta: isize) {
        let length = self.explorer_rows().len();
        if length == 0 {
            return;
        }
        let index = if delta < 0 {
            self.explorer_index.saturating_sub(delta.unsigned_abs())
        } else {
            (self.explorer_index + delta.unsigned_abs()).min(length - 1)
        };
        self.select_explorer_index(index);
    }

    pub fn expand_selected(&mut self) {
        match self
            .explorer_rows()
            .get(self.explorer_index)
            .map(|row| row.node)
        {
            Some(ExplorerNode::Workspace(_)) => {
                self.workspace_expanded = true;
            }
            Some(ExplorerNode::Project(id)) => {
                self.expanded_projects.insert(id);
            }
            Some(ExplorerNode::Worktree(id)) => {
                self.expanded_worktrees.insert(id);
            }
            _ => {}
        }
    }

    pub fn collapse_selected(&mut self) {
        match self
            .explorer_rows()
            .get(self.explorer_index)
            .map(|row| row.node)
        {
            Some(ExplorerNode::Workspace(_)) if self.workspace_expanded => {
                self.workspace_expanded = false;
            }
            Some(ExplorerNode::Project(id)) if self.expanded_projects.remove(&id) => {}
            Some(ExplorerNode::Worktree(id)) if self.expanded_worktrees.remove(&id) => {}
            Some(ExplorerNode::Worktree(id)) => {
                if let Some(project_id) = self
                    .snapshot
                    .worktrees
                    .iter()
                    .find(|item| item.id == id)
                    .map(|item| item.project_id)
                {
                    self.select_node(ExplorerNode::Project(project_id));
                }
            }
            Some(ExplorerNode::Session(id)) => {
                if let Some(worktree_id) = self
                    .snapshot
                    .sessions
                    .iter()
                    .find(|item| item.id == id)
                    .map(|item| item.worktree_id)
                {
                    self.select_node(ExplorerNode::Worktree(worktree_id));
                }
            }
            _ => {}
        }
        self.sync_explorer_index();
    }

    pub fn select_node(&mut self, node: ExplorerNode) {
        if let Some(index) = self.explorer_rows().iter().position(|row| row.node == node) {
            self.select_explorer_index(index);
        }
    }

    pub fn selected_node(&self) -> Option<ExplorerNode> {
        self.explorer_rows()
            .get(self.explorer_index)
            .map(|row| row.node)
    }

    fn selection_fallback_candidates(&self) -> Vec<ExplorerNode> {
        let Some(selected) = self.selected_node() else {
            return Vec::new();
        };
        let (siblings, parents) = match selected {
            ExplorerNode::Workspace(_) => (Vec::new(), Vec::new()),
            ExplorerNode::Project(_) => {
                let siblings = self
                    .visible_projects()
                    .into_iter()
                    .map(|project| ExplorerNode::Project(project.id))
                    .collect();
                let parents = self
                    .active_workspace()
                    .map(|workspace| vec![ExplorerNode::Workspace(workspace.id)])
                    .unwrap_or_default();
                (siblings, parents)
            }
            ExplorerNode::Worktree(worktree_id) => {
                let Some(worktree) = self
                    .snapshot
                    .worktrees
                    .iter()
                    .find(|worktree| worktree.id == worktree_id)
                else {
                    return Vec::new();
                };
                let mut parents = vec![ExplorerNode::Project(worktree.project_id)];
                if let Some(workspace_id) = self
                    .snapshot
                    .projects
                    .iter()
                    .find(|project| project.id == worktree.project_id)
                    .map(|project| project.workspace_id)
                {
                    parents.push(ExplorerNode::Workspace(workspace_id));
                }
                (
                    self.project_worktrees(worktree.project_id)
                        .into_iter()
                        .map(|item| ExplorerNode::Worktree(item.id))
                        .collect(),
                    parents,
                )
            }
            ExplorerNode::Session(session_id) => {
                let Some(session) = self
                    .snapshot
                    .sessions
                    .iter()
                    .find(|session| session.id == session_id)
                else {
                    return Vec::new();
                };
                let mut parents = vec![ExplorerNode::Worktree(session.worktree_id)];
                if let Some(worktree) = self
                    .snapshot
                    .worktrees
                    .iter()
                    .find(|worktree| worktree.id == session.worktree_id)
                {
                    parents.push(ExplorerNode::Project(worktree.project_id));
                    if let Some(workspace_id) = self
                        .snapshot
                        .projects
                        .iter()
                        .find(|project| project.id == worktree.project_id)
                        .map(|project| project.workspace_id)
                    {
                        parents.push(ExplorerNode::Workspace(workspace_id));
                    }
                }
                (
                    self.worktree_sessions(session.worktree_id)
                        .into_iter()
                        .map(|item| ExplorerNode::Session(item.id))
                        .collect(),
                    parents,
                )
            }
        };
        let mut candidates = nearest_siblings(&siblings, selected);
        candidates.extend(parents);
        candidates
    }

    fn node_exists(&self, node: ExplorerNode) -> bool {
        match node {
            ExplorerNode::Workspace(workspace_id) => self
                .active_workspace()
                .is_some_and(|workspace| workspace.id == workspace_id),
            ExplorerNode::Project(project_id) => self
                .snapshot
                .projects
                .iter()
                .find(|project| project.id == project_id)
                .is_some_and(|project| {
                    self.active_workspace()
                        .is_some_and(|workspace| workspace.id == project.workspace_id)
                }),
            ExplorerNode::Worktree(worktree_id) => self
                .snapshot
                .worktrees
                .iter()
                .find(|worktree| {
                    worktree.id == worktree_id && worktree.status != WorktreeStatus::Removed
                })
                .is_some_and(|worktree| {
                    self.node_exists(ExplorerNode::Project(worktree.project_id))
                }),
            ExplorerNode::Session(session_id) => self
                .snapshot
                .sessions
                .iter()
                .find(|session| session.id == session_id)
                .is_some_and(|session| {
                    self.node_exists(ExplorerNode::Worktree(session.worktree_id))
                }),
        }
    }

    fn clear_selection(&mut self) {
        self.selected_workspace = false;
        self.selected_project_id = None;
        self.selected_worktree_id = None;
        self.selected_session_id = None;
    }

    pub(crate) fn reveal_and_select(&mut self, node: ExplorerNode) {
        self.workspace_expanded = true;
        match node {
            ExplorerNode::Worktree(worktree_id) => {
                if let Some(project_id) = self
                    .snapshot
                    .worktrees
                    .iter()
                    .find(|worktree| worktree.id == worktree_id)
                    .map(|worktree| worktree.project_id)
                {
                    self.expanded_projects.insert(project_id);
                }
            }
            ExplorerNode::Session(session_id) => {
                if let Some(worktree_id) = self
                    .snapshot
                    .sessions
                    .iter()
                    .find(|session| session.id == session_id)
                    .map(|session| session.worktree_id)
                {
                    self.expanded_worktrees.insert(worktree_id);
                    if let Some(project_id) = self
                        .snapshot
                        .worktrees
                        .iter()
                        .find(|worktree| worktree.id == worktree_id)
                        .map(|worktree| worktree.project_id)
                    {
                        self.expanded_projects.insert(project_id);
                    }
                }
            }
            ExplorerNode::Workspace(_) | ExplorerNode::Project(_) => {}
        }
        self.select_node(node);
    }

    pub fn select_attention(&mut self) -> Option<String> {
        let target = self
            .snapshot
            .sessions
            .iter()
            .filter(|session| session.state == SessionState::NeedsFeedback)
            .max_by_key(|session| session.last_activity_at)
            .cloned();
        let session = target?;
        if let Some(worktree) = self
            .snapshot
            .worktrees
            .iter()
            .find(|item| item.id == session.worktree_id)
        {
            self.expanded_projects.insert(worktree.project_id);
            self.expanded_worktrees.insert(worktree.id);
        }
        self.select_node(ExplorerNode::Session(session.id));
        Some(format!("{}: waiting for your input", session.display_name))
    }

    pub fn attention_count(&self) -> usize {
        self.snapshot
            .sessions
            .iter()
            .filter(|session| session.state == SessionState::NeedsFeedback)
            .count()
    }

    pub fn status_animation_frame(&self) -> usize {
        const FRAME_MILLIS: u128 = 120;
        let elapsed_frames = self.status_animation_started_at.elapsed().as_millis() / FRAME_MILLIS;
        usize::try_from(elapsed_frames).unwrap_or(usize::MAX)
    }

    pub fn persisted_state(&self) -> TuiState {
        TuiState {
            selected_project_id: self.selected_project_id,
            selected_worktree_id: self.selected_worktree_id,
            selected_session_id: self.selected_session_id,
            selected_content_view: self.main_tab,
            session_presentation: self.session_presentation,
            navigator_view: self.navigator_view,
            navigator_width: self.navigator_width,
            theme: self.theme,
            density: self.density,
        }
        .normalized()
    }

    pub fn open_settings(&mut self) {
        self.mode = Mode::Settings(Settings {
            original_theme: self.theme,
            original_presentation: self.session_presentation,
            original_density: self.density,
            focus: SettingsFocus::Theme,
            github_remote_index: 0,
        });
    }

    pub fn apply_settings(&mut self) {
        self.mode = Mode::Navigation;
        self.mark_navigation_dirty();
        self.flash = Some(Flash::transient(
            FlashKind::Success,
            "TUI settings applied.",
        ));
    }

    pub fn cancel_settings(&mut self) {
        if let Mode::Settings(settings) = self.mode {
            self.theme = settings.original_theme;
            self.session_presentation = settings.original_presentation;
            self.density = settings.original_density;
        }
        self.mode = Mode::Navigation;
    }

    pub fn mark_navigation_dirty(&mut self) {
        self.navigation_dirty_at = Some(Instant::now());
    }

    pub fn set_tab(&mut self, tab: TuiContentView) {
        self.main_tab = if tab == TuiContentView::Sessions {
            TuiContentView::Terminal
        } else {
            tab
        };
        self.focus = FocusZone::Main;
        self.mark_navigation_dirty();
    }

    pub fn scroll_diff(&mut self, delta: isize) {
        let maximum = self
            .diff
            .as_deref()
            .map_or(0, |diff| diff.lines().count().saturating_sub(1));
        self.diff_scroll = usize::from(self.diff_scroll)
            .saturating_add_signed(delta)
            .min(maximum)
            .try_into()
            .unwrap_or(u16::MAX);
    }

    pub fn select_navigator_view(&mut self, view: TuiNavigatorView) {
        self.navigator_view = view;
        self.navigator_resizing = NavigatorResizeState::Idle;
        self.focus = FocusZone::Explorer;
        self.reconcile_active_session_selection();
        self.mark_navigation_dirty();
    }

    pub fn set_navigator_width(&mut self, width: u16) {
        self.navigator_width = width.clamp(MIN_TUI_NAVIGATOR_WIDTH, MAX_TUI_NAVIGATOR_WIDTH);
        self.mark_navigation_dirty();
    }

    pub fn adjust_navigator_width(&mut self, delta: i16) {
        self.set_navigator_width(self.navigator_width.saturating_add_signed(delta));
    }

    pub fn expire_flash(&mut self) {
        if self
            .flash
            .as_ref()
            .and_then(|flash| flash.expires_at)
            .is_some_and(|expiry| Instant::now() >= expiry)
        {
            self.flash = None;
        }
    }
}

fn nearest_siblings(siblings: &[ExplorerNode], selected: ExplorerNode) -> Vec<ExplorerNode> {
    let Some(position) = siblings.iter().position(|node| *node == selected) else {
        return Vec::new();
    };
    let mut nearest = Vec::with_capacity(siblings.len().saturating_sub(1));
    for distance in 1..siblings.len() {
        if let Some(node) = siblings.get(position.saturating_add(distance)) {
            nearest.push(*node);
        }
        if let Some(index) = position.checked_sub(distance)
            && let Some(node) = siblings.get(index)
        {
            nearest.push(*node);
        }
    }
    nearest
}
