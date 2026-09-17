use std::{
    collections::HashSet,
    time::{Duration, Instant},
};

use ratatui::layout::Rect;
use sylvops_core::{
    domain::{
        DaemonSnapshot, Project, Session, SessionState, Workspace, Worktree, WorktreeStatus,
        attention_priority,
    },
    ids::{ProjectId, SessionId, WorktreeId},
    provider::ProviderHealth,
    ui::{MainTab, TuiState},
};

use crate::{forms::Form, terminal::AttachedTerminal};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FocusZone {
    Explorer,
    Main,
}

#[derive(Clone, Debug)]
pub(crate) enum ConfirmAction {
    RemoveWorktree {
        worktree_id: WorktreeId,
        token: String,
    },
    StopSession(SessionId),
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

#[derive(Clone, Debug)]
pub(crate) enum Mode {
    Navigation,
    Modal(Form),
    Confirmation(Confirmation),
    CommandPalette(Palette),
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
    Tab(MainTab),
    FocusExplorer,
    FocusMain,
    Attach,
    Detach,
    New,
    Rename,
    Delete,
    Attention,
    ModalField(usize),
    ModalProvider(usize),
    ModalSubmit,
    ModalCancel,
    Confirm,
    Cancel,
    PaletteRow(usize),
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
    pub mode: Mode,
    pub focus: FocusZone,
    pub main_tab: MainTab,
    pub selected_project_id: Option<ProjectId>,
    pub selected_worktree_id: Option<WorktreeId>,
    pub selected_session_id: Option<SessionId>,
    pub explorer_index: usize,
    pub expanded_projects: HashSet<ProjectId>,
    pub expanded_worktrees: HashSet<WorktreeId>,
    pub diff: Option<String>,
    pub flash: Option<Flash>,
    pub help: bool,
    pub attached: Option<AttachedTerminal>,
    pub hit_map: HitMap,
    pub navigation_dirty_at: Option<Instant>,
}

impl App {
    pub fn new(
        snapshot: DaemonSnapshot,
        providers: Vec<ProviderHealth>,
        persisted: Option<TuiState>,
    ) -> Self {
        let mut app = Self {
            mode: if snapshot.workspaces.is_empty() {
                Mode::Modal(Form::workspace())
            } else {
                Mode::Navigation
            },
            snapshot,
            providers,
            focus: FocusZone::Explorer,
            main_tab: persisted
                .as_ref()
                .map_or(MainTab::Terminal, |state| state.selected_main_tab),
            selected_project_id: persisted
                .as_ref()
                .and_then(|state| state.selected_project_id),
            selected_worktree_id: persisted
                .as_ref()
                .and_then(|state| state.selected_worktree_id),
            selected_session_id: persisted.and_then(|state| state.selected_session_id),
            explorer_index: 0,
            expanded_projects: HashSet::new(),
            expanded_worktrees: HashSet::new(),
            diff: None,
            flash: None,
            help: false,
            attached: None,
            hit_map: HitMap::default(),
            navigation_dirty_at: None,
        };
        app.restore_selection();
        app
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

    pub fn explorer_rows(&self) -> Vec<ExplorerRow> {
        let mut rows = Vec::new();
        for project in self.visible_projects() {
            let project_expanded = self.expanded_projects.contains(&project.id);
            rows.push(ExplorerRow {
                node: ExplorerNode::Project(project.id),
                depth: 0,
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
                    depth: 1,
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
                            depth: 2,
                            expanded: false,
                            expandable: false,
                        }),
                );
            }
        }
        rows
    }

    pub fn restore_selection(&mut self) {
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
            .or_else(|| self.selected_project_id.map(ExplorerNode::Project));
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
            ExplorerNode::Project(id) => {
                self.selected_project_id = Some(id);
                self.selected_worktree_id = None;
                self.selected_session_id = None;
            }
            ExplorerNode::Worktree(id) => {
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

    pub fn select_attention(&mut self) -> Option<String> {
        let target = self
            .snapshot
            .sessions
            .iter()
            .filter(|session| attention_priority(session.state) < 4)
            .min_by_key(|session| {
                (
                    attention_priority(session.state),
                    std::cmp::Reverse(session.last_activity_at),
                )
            })
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
        Some(format!(
            "{}: {}",
            session.display_name,
            attention_reason(session.state)
        ))
    }

    pub fn attention_count(&self) -> usize {
        self.snapshot
            .sessions
            .iter()
            .filter(|session| attention_priority(session.state) < 4)
            .count()
    }

    pub fn persisted_state(&self) -> TuiState {
        TuiState {
            selected_project_id: self.selected_project_id,
            selected_worktree_id: self.selected_worktree_id,
            selected_session_id: self.selected_session_id,
            selected_main_tab: self.main_tab,
        }
    }

    pub fn mark_navigation_dirty(&mut self) {
        self.navigation_dirty_at = Some(Instant::now());
    }

    pub fn set_tab(&mut self, tab: MainTab) {
        self.main_tab = tab;
        self.focus = FocusZone::Main;
        self.mark_navigation_dirty();
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

pub(crate) fn attention_reason(state: SessionState) -> &'static str {
    match state {
        SessionState::NeedsFeedback => "waiting for your input",
        SessionState::Failed => "failed and needs review",
        SessionState::FinishedUnseen => "finished since you last viewed it",
        SessionState::Running | SessionState::Starting => "currently running",
        _ => "does not require attention",
    }
}
