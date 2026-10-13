//! Ephemeral state for the project-level GitHub browser.

use std::str::FromStr;

use sylvops_core::{
    github::{
        GitHubBrowseFailure, GitHubCheckFilter, GitHubCursor, GitHubItemDetail, GitHubItemIdentity,
        GitHubItemKind, GitHubLabelName, GitHubListFilters, GitHubListPage, GitHubListQuery,
        GitHubListState, GitHubLogin, GitHubPageSize, GitHubReviewFilter, GitHubSearchText,
        GitHubViewQuery, MAX_GITHUB_FILTER_LABELS,
    },
    ids::ProjectId,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GitHubDepth {
    List,
    Detail,
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
    current_cursor: Option<GitHubCursor>,
    previous_cursors: Vec<Option<GitHubCursor>>,
    active_list_request: Option<u64>,
    active_detail_request: Option<u64>,
    active_browser_request: Option<u64>,
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
            current_cursor: None,
            previous_cursors: Vec::new(),
            active_list_request: None,
            active_detail_request: None,
            active_browser_request: None,
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
        self.browser_pending = false;
        self.browser_error = None;
        self.configuration_error = None;
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
            GitHubDepth::List | GitHubDepth::Detail => None,
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

    pub(crate) fn receive_list(&mut self, page: GitHubListPage) {
        self.configuration_error = None;
        self.active_list_request = None;
        self.selected_index = self.selected_index.min(page.items.len().saturating_sub(1));
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
        if self.depth == GitHubDepth::Detail {
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

    pub(crate) fn receive_detail(&mut self, detail: GitHubItemDetail) {
        self.configuration_error = None;
        self.active_detail_request = None;
        self.depth = GitHubDepth::Detail;
        self.selected = Some(detail.summary.identity.clone());
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
            GitHubDepth::Detail => {
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
        if self.depth == GitHubDepth::Detail {
            self.depth = GitHubDepth::List;
            self.detail_scroll = 0;
            self.detail = GitHubLoad::Idle;
            self.selected = None;
            self.active_detail_request = None;
            self.browser_pending = false;
            self.browser_error = None;
            self.active_browser_request = None;
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
