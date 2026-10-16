//! Ephemeral state for the project-level GitHub browser.

use std::{collections::HashMap, str::FromStr};

use sylvops_core::{
    domain::{DaemonSnapshot, ProviderKind, WorktreeStatus},
    github::{
        GitHubBrowseFailure, GitHubCheckFilter, GitHubCursor, GitHubItemDetail, GitHubItemIdentity,
        GitHubItemKind, GitHubItemNumber, GitHubItemState, GitHubLabelName, GitHubListFilters,
        GitHubListPage, GitHubListQuery, GitHubListState, GitHubLogin, GitHubPageSize,
        GitHubReviewFilter, GitHubSearchText, GitHubViewQuery, IssueWorkStatus,
        MAX_GITHUB_FILTER_LABELS, MAX_START_WORK_BRANCH_LENGTH, MAX_START_WORK_INSTRUCTIONS_LENGTH,
        StartWorkBranch, StartWorkFailure, StartWorkInstructions, StartWorkPreparation,
    },
    ids::{ProjectId, WorktreeId},
    protocol::ClientRequest,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GitHubDepth {
    List,
    Detail,
    StartWork,
    Recovery,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StartWorkFocus {
    Branch,
    Provider,
    SessionName,
    Instructions,
    BaseRef,
    Confirm,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IssueRemoteStatus {
    Open,
    Closed,
    Unavailable,
    RepositoryChanged,
}

impl IssueRemoteStatus {
    pub(crate) const fn display_name(self) -> &'static str {
        match self {
            Self::Open => "Open",
            Self::Closed => "Closed",
            Self::Unavailable => "Unavailable",
            Self::RepositoryChanged => "Repository changed",
        }
    }
}

impl StartWorkFocus {
    pub(crate) fn moved(self, delta: isize) -> Self {
        let values = [
            Self::Branch,
            Self::Provider,
            Self::SessionName,
            Self::Instructions,
            Self::BaseRef,
            Self::Confirm,
        ];
        let current: usize = match self {
            Self::Branch => 0,
            Self::Provider => 1,
            Self::SessionName => 2,
            Self::Instructions => 3,
            Self::BaseRef => 4,
            Self::Confirm => 5,
        };
        values[current.saturating_add_signed(delta).min(values.len() - 1)]
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StartWorkForm {
    pub base_mode: sylvops_core::ui_forms::WorktreeBaseMode,
    pub base_ref: String,
    pub base_ready: bool,
    pub preparation: StartWorkPreparation,
    pub branch: String,
    pub provider_index: usize,
    pub session_name: String,
    pub instructions: String,
    pub focus: StartWorkFocus,
    pub start_another_worktree: bool,
    pub pending: bool,
    pub error: Option<StartWorkFailure>,
    pub branch_conflict: Option<String>,
    pub context_scroll: u16,
}

impl StartWorkForm {
    fn new(preparation: StartWorkPreparation, start_another_worktree: bool) -> Self {
        let branch = preparation.suggested_branch.as_str().to_owned();
        let branch_conflict = preparation
            .suggested_branch_conflict
            .then(|| branch.clone());
        Self {
            base_mode: sylvops_core::ui_forms::WorktreeBaseMode::from_selection(
                &preparation.base.selection,
            ),
            base_ref: match &preparation.base.selection {
                sylvops_core::domain::WorktreeBaseSelection::LocalRef { reference } => {
                    reference.clone()
                }
                sylvops_core::domain::WorktreeBaseSelection::LatestDefaultBranch { .. } => {
                    "HEAD".into()
                }
            },
            base_ready: true,
            branch,
            preparation,
            provider_index: 0,
            session_name: String::new(),
            instructions: String::new(),
            focus: StartWorkFocus::Branch,
            start_another_worktree,
            pending: false,
            error: None,
            branch_conflict,
            context_scroll: 0,
        }
    }

    pub(crate) fn provider(&self) -> Option<ProviderKind> {
        self.preparation
            .eligible_providers
            .get(self.provider_index)
            .copied()
    }

    pub(crate) fn cycle_base(&mut self) {
        use sylvops_core::ui_forms::WorktreeBaseMode;
        let index = WorktreeBaseMode::ALL
            .iter()
            .position(|mode| *mode == self.base_mode)
            .unwrap_or(0);
        self.base_mode = WorktreeBaseMode::ALL[(index + 1) % WorktreeBaseMode::ALL.len()];
        self.base_ready = false;
    }

    pub(crate) fn move_provider(&mut self, delta: isize) {
        if !self.preparation.eligible_providers.is_empty() {
            self.provider_index = self
                .provider_index
                .saturating_add_signed(delta)
                .min(self.preparation.eligible_providers.len() - 1);
        }
    }

    pub(crate) fn push_character(&mut self, character: char) {
        if self.pending || character.is_control() {
            return;
        }
        let target = match self.focus {
            StartWorkFocus::Branch => Some((&mut self.branch, MAX_START_WORK_BRANCH_LENGTH)),
            StartWorkFocus::SessionName => Some((&mut self.session_name, 200)),
            StartWorkFocus::Instructions => {
                Some((&mut self.instructions, MAX_START_WORK_INSTRUCTIONS_LENGTH))
            }
            StartWorkFocus::BaseRef => Some((&mut self.base_ref, 1024)),
            StartWorkFocus::Provider | StartWorkFocus::Confirm => None,
        };
        if let Some((value, maximum)) = target
            && value.len().saturating_add(character.len_utf8()) <= maximum
        {
            value.push(character);
            if self.focus == StartWorkFocus::BaseRef {
                self.base_mode = sylvops_core::ui_forms::WorktreeBaseMode::CustomRef;
                self.base_ready = false;
            }
            self.error = None;
            if self.focus == StartWorkFocus::Branch {
                self.branch_conflict = None;
            }
        }
    }

    pub(crate) fn pop_character(&mut self) {
        if self.pending {
            return;
        }
        let changed = match self.focus {
            StartWorkFocus::Branch => self.branch.pop().is_some(),
            StartWorkFocus::SessionName => self.session_name.pop().is_some(),
            StartWorkFocus::Instructions => self.instructions.pop().is_some(),
            StartWorkFocus::BaseRef => self.base_ref.pop().is_some(),
            StartWorkFocus::Provider | StartWorkFocus::Confirm => false,
        };
        if changed {
            if self.focus == StartWorkFocus::BaseRef {
                self.base_mode = sylvops_core::ui_forms::WorktreeBaseMode::CustomRef;
                self.base_ready = false;
            }
            self.error = None;
            if self.focus == StartWorkFocus::Branch {
                self.branch_conflict = None;
            }
        }
    }

    pub(crate) fn scroll_context(&mut self, delta: isize) {
        let amount = u16::try_from(delta.unsigned_abs()).unwrap_or(u16::MAX);
        self.context_scroll = if delta.is_negative() {
            self.context_scroll.saturating_sub(amount)
        } else {
            self.context_scroll.saturating_add(amount)
        };
    }

    pub(crate) fn request(&self, project_id: ProjectId) -> Result<ClientRequest, String> {
        if self.pending {
            return Err("Start Work is already in progress.".into());
        }
        if !self.base_ready {
            return Err("Refresh the base preview with Ctrl+R before confirming.".into());
        }
        let branch = StartWorkBranch::new(self.branch.clone())?;
        if self.branch_conflict.as_deref() == Some(branch.as_str()) {
            return Err("The selected branch already exists. Choose another branch.".into());
        }
        let provider = self
            .provider()
            .ok_or_else(|| "Choose an eligible task-capable Provider.".to_owned())?;
        let additional_instructions = if self.instructions.trim().is_empty() {
            None
        } else {
            Some(StartWorkInstructions::new(self.instructions.clone())?)
        };
        let session_name = self.session_name.trim();
        if session_name.chars().count() > 200 || session_name.chars().any(char::is_control) {
            return Err("Session name must be at most 200 characters without controls.".into());
        }
        Ok(ClientRequest::StartWork {
            confirmation: self.preparation.confirmation(project_id),
            branch,
            provider,
            display_name: (!session_name.is_empty()).then(|| session_name.to_owned()),
            additional_instructions,
            start_another_worktree: self.start_another_worktree,
            columns: 80,
            rows: 24,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StartWorkRecovery {
    pub worktree_id: WorktreeId,
    pub branch: String,
    pub provider: ProviderKind,
    pub session_name: String,
    pub failure: StartWorkFailure,
    pub pending: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GitHubEscape {
    CancelledFilter,
    ReturnedToList,
    Close,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GitHubRequestTarget {
    List,
    Detail,
    Browser,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) enum GitHubLoad<T> {
    #[default]
    Idle,
    Loading(Option<T>),
    Ready(T),
    Failed(GitHubBrowseFailure),
}

impl<T> GitHubLoad<T> {
    pub(crate) const fn is_loading(&self) -> bool {
        matches!(self, Self::Loading(_))
    }

    pub(crate) const fn ready(&self) -> Option<&T> {
        match self {
            Self::Ready(value) | Self::Loading(Some(value)) => Some(value),
            Self::Idle | Self::Loading(None) | Self::Failed(_) => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GitHubModal {
    pub project_id: ProjectId,
    pub kind: GitHubItemKind,
    pub depth: GitHubDepth,
    pub filters: GitHubListFilters,
    pub filter_input: String,
    pub filter_editing: bool,
    pub filter_error: Option<String>,
    pub list: GitHubLoad<GitHubListPage>,
    pub detail: GitHubLoad<GitHubItemDetail>,
    pub selected_index: usize,
    selected: Option<GitHubItemIdentity>,
    pub detail_scroll: u16,
    pub browser_pending: bool,
    pub browser_error: Option<String>,
    pub configuration_error: Option<String>,
    pub start_work: Option<StartWorkForm>,
    pub start_work_preparing: bool,
    pub start_work_initial_base: Option<sylvops_core::domain::WorktreeBaseSelection>,
    pub start_work_error: Option<StartWorkFailure>,
    pub recovery: Option<StartWorkRecovery>,
    issue_work: HashMap<GitHubItemIdentity, WorktreeId>,
    issue_status: HashMap<GitHubItemIdentity, IssueWorkStatus>,
    issue_remote_status: HashMap<GitHubItemIdentity, IssueRemoteStatus>,
    current_cursor: Option<GitHubCursor>,
    previous_cursors: Vec<Option<GitHubCursor>>,
    active_list_request: Option<u64>,
    active_detail_request: Option<u64>,
    active_browser_request: Option<u64>,
    active_start_work_request: Option<u64>,
}

impl GitHubModal {
    pub(crate) fn open(project_id: ProjectId) -> (Self, GitHubListQuery) {
        let mut modal = Self {
            project_id,
            kind: GitHubItemKind::Issue,
            depth: GitHubDepth::List,
            filters: GitHubListFilters::default(),
            filter_input: String::new(),
            filter_editing: false,
            filter_error: None,
            list: GitHubLoad::Idle,
            detail: GitHubLoad::Idle,
            selected_index: 0,
            selected: None,
            detail_scroll: 0,
            browser_pending: false,
            browser_error: None,
            configuration_error: None,
            start_work: None,
            start_work_preparing: false,
            start_work_initial_base: None,
            start_work_error: None,
            recovery: None,
            issue_work: HashMap::new(),
            issue_status: HashMap::new(),
            issue_remote_status: HashMap::new(),
            current_cursor: None,
            previous_cursors: Vec::new(),
            active_list_request: None,
            active_detail_request: None,
            active_browser_request: None,
            active_start_work_request: None,
        };
        let query = modal.begin_list(false);
        (modal, query)
    }

    pub(crate) fn switch_kind(&mut self, kind: GitHubItemKind) -> Option<GitHubListQuery> {
        if self.kind == kind {
            return None;
        }
        self.kind = kind;
        self.depth = GitHubDepth::List;
        self.filters = GitHubListFilters::default();
        self.filter_input.clear();
        self.filter_editing = false;
        self.filter_error = None;
        self.list = GitHubLoad::Idle;
        self.detail = GitHubLoad::Idle;
        self.selected_index = 0;
        self.selected = None;
        self.detail_scroll = 0;
        self.current_cursor = None;
        self.previous_cursors.clear();
        self.active_list_request = None;
        self.active_detail_request = None;
        self.active_browser_request = None;
        self.active_start_work_request = None;
        self.browser_pending = false;
        self.browser_error = None;
        self.configuration_error = None;
        self.start_work = None;
        self.start_work_preparing = false;
        self.start_work_error = None;
        self.recovery = None;
        Some(self.begin_list(false))
    }

    pub(crate) fn refresh(&mut self) -> Option<GitHubQuery> {
        match self.depth {
            GitHubDepth::List if !self.list.is_loading() => {
                Some(GitHubQuery::List(self.begin_list(true)))
            }
            GitHubDepth::Detail if !self.detail.is_loading() => self
                .selected_identity()
                .map(|identity| GitHubQuery::Detail(self.begin_detail(&identity, true, None))),
            GitHubDepth::List
            | GitHubDepth::Detail
            | GitHubDepth::StartWork
            | GitHubDepth::Recovery => None,
        }
    }

    pub(crate) fn register_list_request(&mut self, request_id: u64) {
        self.active_list_request = Some(request_id);
    }

    pub(crate) fn register_detail_request(&mut self, request_id: u64) {
        self.active_detail_request = Some(request_id);
    }

    pub(crate) fn register_browser_request(&mut self, request_id: u64) {
        self.active_browser_request = Some(request_id);
    }

    pub(crate) fn accepts_list(&self, request_id: u64) -> bool {
        self.active_list_request == Some(request_id)
    }

    pub(crate) fn accepts_detail(&self, request_id: u64) -> bool {
        self.active_detail_request == Some(request_id)
    }

    pub(crate) fn accepts_browser(&self, request_id: u64) -> bool {
        self.active_browser_request == Some(request_id)
    }

    pub(crate) fn register_start_work_request(&mut self, request_id: u64) {
        self.active_start_work_request = Some(request_id);
    }

    pub(crate) fn accepts_start_work(&self, request_id: u64) -> bool {
        self.active_start_work_request == Some(request_id)
    }

    pub(crate) fn receive_list(&mut self, page: GitHubListPage) {
        self.configuration_error = None;
        self.active_list_request = None;
        self.selected_index = self.selected_index.min(page.items.len().saturating_sub(1));
        for item in &page.items {
            if item.identity.kind == GitHubItemKind::Issue {
                self.issue_remote_status.insert(
                    item.identity.clone(),
                    if item.state == GitHubItemState::Closed {
                        IssueRemoteStatus::Closed
                    } else {
                        IssueRemoteStatus::Open
                    },
                );
            }
        }
        self.list = GitHubLoad::Ready(page);
    }

    pub(crate) fn move_selection(&mut self, delta: isize) {
        let count = self.list.ready().map_or(0, |page| page.items.len());
        if count > 0 {
            self.selected_index = self
                .selected_index
                .saturating_add_signed(delta)
                .min(count - 1);
        }
    }

    pub(crate) fn scroll_detail(&mut self, delta: isize) {
        self.detail_scroll = if delta.is_negative() {
            self.detail_scroll.saturating_sub(1)
        } else if delta.is_positive() {
            self.detail_scroll.saturating_add(1)
        } else {
            self.detail_scroll
        };
    }

    pub(crate) fn selected_identity(&self) -> Option<GitHubItemIdentity> {
        if matches!(
            self.depth,
            GitHubDepth::Detail | GitHubDepth::StartWork | GitHubDepth::Recovery
        ) {
            return self.selected.clone();
        }
        self.list
            .ready()?
            .items
            .get(self.selected_index)
            .map(|item| item.identity.clone())
    }

    pub(crate) fn open_selected(&mut self) -> Option<GitHubViewQuery> {
        if self.list.is_loading() {
            return None;
        }
        let identity = self.selected_identity()?;
        self.selected = Some(identity.clone());
        self.depth = GitHubDepth::Detail;
        self.detail_scroll = 0;
        Some(self.begin_detail(&identity, false, None))
    }

    pub(crate) fn open_identity(&mut self, identity: &GitHubItemIdentity) -> GitHubViewQuery {
        self.kind = identity.kind;
        self.selected = Some(identity.clone());
        self.depth = GitHubDepth::Detail;
        self.detail_scroll = 0;
        self.begin_detail(identity, false, None)
    }

    pub(crate) fn receive_detail(&mut self, detail: GitHubItemDetail) {
        self.configuration_error = None;
        self.active_detail_request = None;
        self.depth = GitHubDepth::Detail;
        self.selected = Some(detail.summary.identity.clone());
        if detail.summary.identity.kind == GitHubItemKind::Issue {
            self.issue_remote_status.insert(
                detail.summary.identity.clone(),
                if detail.summary.state == GitHubItemState::Closed {
                    IssueRemoteStatus::Closed
                } else {
                    IssueRemoteStatus::Open
                },
            );
        }
        self.detail = GitHubLoad::Ready(detail);
    }

    pub(crate) fn load_more_comments(&mut self) -> Option<GitHubViewQuery> {
        if self.detail.is_loading() {
            return None;
        }
        let detail = self.detail.ready()?.clone();
        let cursor = detail.comments.next_cursor.clone()?;
        let identity = detail.summary.identity.clone();
        self.detail = GitHubLoad::Loading(Some(detail));
        Some(self.begin_detail(&identity, false, Some(cursor)))
    }

    pub(crate) fn append_comments(&mut self, mut detail: GitHubItemDetail) {
        if let Some(previous) = self.detail.ready()
            && previous.summary.identity == detail.summary.identity
        {
            let mut comments = previous.comments.comments.clone();
            comments.append(&mut detail.comments.comments);
            detail.comments.comments = comments;
        }
        self.receive_detail(detail);
    }

    pub(crate) fn next_page(&mut self) -> Option<GitHubListQuery> {
        if self.list.is_loading() {
            return None;
        }
        let cursor = self.list.ready()?.next_cursor.clone()?;
        self.previous_cursors.push(self.current_cursor.clone());
        self.current_cursor = Some(cursor);
        self.selected_index = 0;
        Some(self.begin_list(false))
    }

    pub(crate) fn previous_page(&mut self) -> Option<GitHubListQuery> {
        if self.list.is_loading() {
            return None;
        }
        self.current_cursor = self.previous_cursors.pop()?;
        self.selected_index = 0;
        Some(self.begin_list(false))
    }

    pub(crate) const fn can_go_back(&self) -> bool {
        !self.previous_cursors.is_empty()
    }

    pub(crate) fn start_filtering(&mut self) {
        self.filter_editing = true;
        self.filter_error = None;
    }

    pub(crate) fn apply_filter(&mut self) -> Result<GitHubListQuery, String> {
        self.filters = parse_filters(&self.filter_input, self.kind)?;
        self.filter_editing = false;
        self.filter_error = None;
        self.current_cursor = None;
        self.previous_cursors.clear();
        self.selected_index = 0;
        self.detail = GitHubLoad::Idle;
        self.selected = None;
        Ok(self.begin_list(false))
    }

    pub(crate) fn set_filter_error(&mut self, error: String) {
        self.filter_error = Some(error);
    }

    pub(crate) fn set_configuration_error(&mut self, error: String) {
        self.configuration_error = Some(error);
        match self.depth {
            GitHubDepth::List => {
                self.list = GitHubLoad::Idle;
                self.active_list_request = None;
            }
            GitHubDepth::Detail | GitHubDepth::StartWork | GitHubDepth::Recovery => {
                self.detail = GitHubLoad::Idle;
                self.active_detail_request = None;
            }
        }
    }

    pub(crate) fn begin_browser_open(&mut self) -> Option<GitHubItemIdentity> {
        if self.browser_pending {
            return None;
        }
        let identity = self.selected_identity()?;
        self.browser_pending = true;
        self.browser_error = None;
        Some(identity)
    }

    pub(crate) fn begin_start_work(
        &mut self,
        start_another_worktree: bool,
    ) -> Option<(GitHubItemNumber, bool)> {
        let identity = self.selected_identity()?;
        if identity.kind != GitHubItemKind::Issue {
            return None;
        }
        self.depth = GitHubDepth::StartWork;
        self.start_work = None;
        self.start_work_preparing = true;
        self.start_work_initial_base = None;
        self.start_work_error = None;
        Some((identity.number, start_another_worktree))
    }

    pub(crate) fn receive_start_work_preparation(
        &mut self,
        preparation: StartWorkPreparation,
        start_another_worktree: bool,
    ) {
        self.depth = GitHubDepth::StartWork;
        self.start_work_preparing = false;
        self.active_start_work_request = None;
        self.start_work_error = None;
        if let Some(form) = &mut self.start_work {
            form.preparation = preparation;
            form.pending = false;
            form.base_ready = true;
            form.error = None;
        } else {
            self.start_work = Some(StartWorkForm::new(preparation, start_another_worktree));
        }
    }

    pub(crate) fn fail_start_work(&mut self, failure: StartWorkFailure) {
        self.start_work_preparing = false;
        self.active_start_work_request = None;
        if let Some(form) = &mut self.start_work {
            form.pending = false;
            if failure.kind == sylvops_core::github::StartWorkFailureKind::BaseMoved {
                form.base_ready = false;
            }
            form.error = Some(failure);
        } else {
            self.start_work_error = Some(failure);
        }
    }

    pub(crate) fn begin_recovery(&mut self, recovery: StartWorkRecovery) {
        self.depth = GitHubDepth::Recovery;
        self.start_work = None;
        self.start_work_preparing = false;
        self.active_start_work_request = None;
        self.start_work_error = None;
        self.recovery = Some(recovery);
    }

    pub(crate) fn sync_local_work(&mut self, snapshot: &DaemonSnapshot) {
        self.issue_work.clear();
        self.issue_status.clear();
        for source in snapshot
            .work_sources
            .iter()
            .filter(|source| source.project_id == self.project_id)
        {
            if snapshot.worktrees.iter().any(|worktree| {
                worktree.id == source.worktree_id && worktree.status == WorktreeStatus::Active
            }) {
                match self.issue_work.entry(source.item.clone()) {
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        entry.insert(source.worktree_id);
                    }
                    std::collections::hash_map::Entry::Occupied(mut entry) => {
                        let current_created_at = snapshot
                            .work_sources
                            .iter()
                            .find(|candidate| candidate.worktree_id == *entry.get())
                            .map_or(i64::MIN, |candidate| candidate.created_at);
                        if source.created_at > current_created_at {
                            entry.insert(source.worktree_id);
                        }
                    }
                }
            }
        }
        for identity in self.issue_work.keys() {
            self.issue_status.insert(
                identity.clone(),
                sylvops_core::github::issue_work_status(
                    identity,
                    &snapshot.work_sources,
                    &snapshot.worktrees,
                    &snapshot.sessions,
                ),
            );
        }
    }

    pub(crate) fn existing_worktree(&self) -> Option<WorktreeId> {
        let identity = self.selected_identity()?;
        self.issue_work.get(&identity).copied()
    }

    pub(crate) fn status_for(&self, identity: &GitHubItemIdentity) -> IssueWorkStatus {
        self.issue_status
            .get(identity)
            .copied()
            .unwrap_or(IssueWorkStatus::NotStarted)
    }

    pub(crate) fn remote_status_for(
        &self,
        identity: &GitHubItemIdentity,
    ) -> Option<IssueRemoteStatus> {
        self.issue_remote_status.get(identity).copied()
    }

    pub(crate) fn mark_repository_changed(&mut self, identity: GitHubItemIdentity) {
        self.issue_remote_status
            .insert(identity, IssueRemoteStatus::RepositoryChanged);
    }

    pub(crate) fn browser_opened(&mut self) {
        self.browser_pending = false;
        self.browser_error = None;
        self.active_browser_request = None;
    }

    pub(crate) fn browser_failed(&mut self, error: String) {
        self.browser_pending = false;
        self.browser_error = Some(error);
        self.active_browser_request = None;
    }

    pub(crate) fn fail(
        &mut self,
        target: GitHubRequestTarget,
        mut failure: GitHubBrowseFailure,
        diagnostic: String,
    ) {
        failure.diagnostic = diagnostic;
        if target == GitHubRequestTarget::Detail
            && failure.kind == sylvops_core::github::GitHubBrowseFailureKind::UnavailableItem
            && let Some(identity) = self.selected_identity()
            && identity.kind == GitHubItemKind::Issue
        {
            self.issue_remote_status
                .insert(identity, IssueRemoteStatus::Unavailable);
        }
        match target {
            GitHubRequestTarget::List => self.list = GitHubLoad::Failed(failure),
            GitHubRequestTarget::Detail => self.detail = GitHubLoad::Failed(failure),
            GitHubRequestTarget::Browser => {
                self.browser_pending = false;
                self.browser_error = Some(failure.diagnostic);
                self.active_browser_request = None;
            }
        }
        match target {
            GitHubRequestTarget::List => self.active_list_request = None,
            GitHubRequestTarget::Detail => self.active_detail_request = None,
            GitHubRequestTarget::Browser => {}
        }
    }

    pub(crate) fn escape(&mut self) -> GitHubEscape {
        if self.filter_editing {
            self.filter_editing = false;
            self.filter_error = None;
            return GitHubEscape::CancelledFilter;
        }
        if matches!(self.depth, GitHubDepth::StartWork | GitHubDepth::Recovery) {
            self.depth = GitHubDepth::Detail;
            self.start_work = None;
            self.start_work_preparing = false;
            self.start_work_error = None;
            self.recovery = None;
            self.active_start_work_request = None;
            return GitHubEscape::ReturnedToList;
        }
        if self.depth == GitHubDepth::Detail {
            self.depth = GitHubDepth::List;
            self.detail_scroll = 0;
            self.detail = GitHubLoad::Idle;
            self.selected = None;
            self.active_detail_request = None;
            self.browser_pending = false;
            self.browser_error = None;
            self.active_browser_request = None;
            self.active_start_work_request = None;
            self.start_work = None;
            self.start_work_preparing = false;
            self.start_work_error = None;
            self.recovery = None;
            return GitHubEscape::ReturnedToList;
        }
        GitHubEscape::Close
    }

    fn begin_list(&mut self, refresh: bool) -> GitHubListQuery {
        let previous = self.list.ready().cloned();
        self.list = GitHubLoad::Loading(previous);
        self.configuration_error = None;
        GitHubListQuery {
            kind: self.kind,
            filters: self.filters.clone(),
            page_size: GitHubPageSize::default(),
            cursor: self.current_cursor.clone(),
            refresh,
        }
    }

    fn begin_detail(
        &mut self,
        identity: &GitHubItemIdentity,
        refresh: bool,
        comments_cursor: Option<GitHubCursor>,
    ) -> GitHubViewQuery {
        let previous = self.detail.ready().cloned();
        self.detail = GitHubLoad::Loading(previous);
        self.configuration_error = None;
        GitHubViewQuery {
            kind: identity.kind,
            number: identity.number,
            comments_page_size: GitHubPageSize::default(),
            comments_cursor,
            refresh,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum GitHubQuery {
    List(GitHubListQuery),
    Detail(GitHubViewQuery),
}

fn parse_filters(value: &str, kind: GitHubItemKind) -> Result<GitHubListFilters, String> {
    let mut filters = GitHubListFilters::default();
    let mut text = Vec::new();
    for token in value.split_whitespace() {
        let Some((name, raw)) = token.split_once(':') else {
            text.push(token);
            continue;
        };
        if raw.is_empty() {
            return Err(format!("Filter {name}: needs a value."));
        }
        match name {
            "state" => filters.state = GitHubListState::from_str(raw)?,
            "author" => filters.author = Some(GitHubLogin::new(raw.to_owned())?),
            "assignee" => filters.assignee = Some(GitHubLogin::new(raw.to_owned())?),
            "label" => {
                let label = GitHubLabelName::new(raw.to_owned())?;
                if !filters.labels.contains(&label) {
                    filters.labels.push(label);
                }
                if filters.labels.len() > MAX_GITHUB_FILTER_LABELS {
                    return Err("Too many GitHub label filters.".into());
                }
            }
            "draft" if kind == GitHubItemKind::PullRequest => {
                filters.draft = Some(match raw {
                    "true" | "yes" | "draft" => true,
                    "false" | "no" | "ready" => false,
                    _ => return Err("draft: must be true, false, draft, or ready.".into()),
                });
            }
            "review" if kind == GitHubItemKind::PullRequest => {
                filters.review = Some(GitHubReviewFilter::from_str(raw)?);
            }
            "check" if kind == GitHubItemKind::PullRequest => {
                filters.check = Some(GitHubCheckFilter::from_str(raw)?);
            }
            "draft" | "review" | "check" => {
                return Err(format!("{name}: is available only for Pull Requests."));
            }
            _ => return Err(format!("Unknown GitHub filter: {name}:")),
        }
    }
    if !text.is_empty() {
        filters.text = Some(GitHubSearchText::new(text.join(" "))?);
    }
    Ok(filters)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sylvops_core::github::{
        GitHubHost, GitHubItemNumber, GitHubOwner, GitHubRepositoryIdentity, GitHubRepositoryName,
    };

    fn identity(kind: GitHubItemKind, number: u64) -> GitHubItemIdentity {
        GitHubItemIdentity::new(
            GitHubRepositoryIdentity::new(
                GitHubHost::new("github.example.com").unwrap(),
                GitHubOwner::new("acme").unwrap(),
                GitHubRepositoryName::new("widgets").unwrap(),
            ),
            kind,
            GitHubItemNumber::new(number).unwrap(),
        )
    }

    #[test]
    fn only_issues_can_begin_start_work() {
        let (mut modal, _) = GitHubModal::open(ProjectId::new());
        modal.depth = GitHubDepth::Detail;
        modal.selected = Some(identity(GitHubItemKind::Issue, 129));

        assert_eq!(
            modal.begin_start_work(false),
            Some((GitHubItemNumber::new(129).unwrap(), false))
        );

        modal.selected = Some(identity(GitHubItemKind::PullRequest, 129));
        assert_eq!(modal.begin_start_work(false), None);
    }

    #[test]
    fn opening_builds_a_bounded_default_issues_query() {
        let project_id = ProjectId::new();
        let (modal, query) = GitHubModal::open(project_id);

        assert_eq!(modal.project_id, project_id);
        assert_eq!(modal.kind, GitHubItemKind::Issue);
        assert_eq!(query.kind, GitHubItemKind::Issue);
        assert_eq!(query.page_size.get(), 30);
        assert!(query.cursor.is_none());
        assert!(!query.refresh);
    }

    #[test]
    fn escape_returns_from_detail_before_closing_the_modal() {
        let project_id = ProjectId::new();
        let (mut modal, _) = GitHubModal::open(project_id);
        modal.depth = GitHubDepth::Detail;

        assert_eq!(modal.escape(), GitHubEscape::ReturnedToList);
        assert_eq!(modal.depth, GitHubDepth::List);
        assert_eq!(modal.escape(), GitHubEscape::Close);
    }

    #[test]
    fn typed_filters_cover_issue_and_pull_request_capabilities() {
        let issues = parse_filters(
            "state:closed author:octocat assignee:maintainer label:bug scheduler",
            GitHubItemKind::Issue,
        )
        .unwrap();
        assert_eq!(issues.state, GitHubListState::Closed);
        assert_eq!(issues.text.unwrap().as_str(), "scheduler");
        assert_eq!(issues.labels.len(), 1);

        let pull_requests = parse_filters(
            "draft:ready review:review-required check:failure",
            GitHubItemKind::PullRequest,
        )
        .unwrap();
        assert_eq!(pull_requests.draft, Some(false));
        assert_eq!(
            pull_requests.review,
            Some(GitHubReviewFilter::ReviewRequired)
        );
        assert_eq!(pull_requests.check, Some(GitHubCheckFilter::Failure));
        assert!(parse_filters("draft:true", GitHubItemKind::Issue).is_err());
    }
}
