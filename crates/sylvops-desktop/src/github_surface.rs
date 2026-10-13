//! Ephemeral presentation state for project-level GitHub browsing.

use sylvops_core::{
    github::{
        GitHubBrowseFailure, GitHubBrowseFailureKind, GitHubCursor, GitHubItemDetail,
        GitHubItemIdentity, GitHubItemKind, GitHubLabelName, GitHubListFilters, GitHubListPage,
        GitHubListQuery, GitHubLogin, GitHubPageSize, GitHubSearchText, GitHubViewQuery,
    },
    ids::ProjectId,
};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum GitHubSurfaceKind {
    #[default]
    Issues,
    PullRequests,
}

impl GitHubSurfaceKind {
    pub(crate) const fn item_kind(self) -> GitHubItemKind {
        match self {
            Self::Issues => GitHubItemKind::Issue,
            Self::PullRequests => GitHubItemKind::PullRequest,
        }
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Issues => "Issues",
            Self::PullRequests => "Pull requests",
        }
    }
}

impl std::fmt::Display for GitHubSurfaceKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.label())
    }
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

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct GitHubFilterInput {
    pub text: String,
    pub author: String,
    pub assignee: String,
    pub labels: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct GitHubFailureCopy {
    pub title: &'static str,
    pub action: &'static str,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct GitHubSurfaceState {
    project_id: Option<ProjectId>,
    kind: GitHubSurfaceKind,
    pub filters: GitHubListFilters,
    pub filter_input: GitHubFilterInput,
    pub filter_error: Option<String>,
    pub list: GitHubLoad<GitHubListPage>,
    pub detail: GitHubLoad<GitHubItemDetail>,
    pub selected: Option<GitHubItemIdentity>,
    current_cursor: Option<GitHubCursor>,
    previous_cursors: Vec<Option<GitHubCursor>>,
    pub browser_pending: bool,
    pub browser_error: Option<String>,
}

impl GitHubSurfaceState {
    pub(crate) const fn project_id(&self) -> Option<ProjectId> {
        self.project_id
    }

    pub(crate) const fn kind(&self) -> GitHubSurfaceKind {
        self.kind
    }

    pub(crate) fn open(
        &mut self,
        project_id: ProjectId,
        kind: GitHubSurfaceKind,
    ) -> GitHubListQuery {
        self.reset(project_id, kind);
        self.begin_list(false)
    }

    pub(crate) fn refresh_list(&mut self) -> Option<GitHubListQuery> {
        self.project_id?;
        Some(self.begin_list(true))
    }

    pub(crate) fn apply_filters(&mut self) -> Result<GitHubListQuery, String> {
        self.project_id.ok_or("Select a Repository first.")?;
        self.filters.text = optional_value(&self.filter_input.text, GitHubSearchText::new)?;
        self.filters.author = optional_value(&self.filter_input.author, GitHubLogin::new)?;
        self.filters.assignee = optional_value(&self.filter_input.assignee, GitHubLogin::new)?;
        self.filters.labels = parse_labels(&self.filter_input.labels)?;
        self.filter_error = None;
        self.current_cursor = None;
        self.previous_cursors.clear();
        self.selected = None;
        self.detail = GitHubLoad::Idle;
        Ok(self.begin_list(false))
    }

    pub(crate) fn set_filter_error(&mut self, error: String) {
        self.filter_error = Some(error);
    }

    pub(crate) fn next_page(&mut self) -> Option<GitHubListQuery> {
        let next_cursor = self.list.ready()?.next_cursor.clone()?;
        self.previous_cursors.push(self.current_cursor.clone());
        self.current_cursor = Some(next_cursor);
        Some(self.begin_list(false))
    }

    pub(crate) fn previous_page(&mut self) -> Option<GitHubListQuery> {
        let cursor = self.previous_cursors.pop()?;
        self.current_cursor = cursor;
        Some(self.begin_list(false))
    }

    pub(crate) fn can_go_back(&self) -> bool {
        !self.previous_cursors.is_empty()
    }

    pub(crate) fn receive_list(&mut self, page: GitHubListPage) {
        self.list = GitHubLoad::Ready(page);
    }

    pub(crate) fn select(
        &mut self,
        identity: GitHubItemIdentity,
        refresh: bool,
    ) -> Option<GitHubViewQuery> {
        if identity.kind != self.kind.item_kind() {
            return None;
        }
        let kind = identity.kind;
        let number = identity.number;
        self.selected = Some(identity);
        self.detail = GitHubLoad::Loading(None);
        self.browser_pending = false;
        self.browser_error = None;
        Some(GitHubViewQuery {
            kind,
            number,
            comments_page_size: GitHubPageSize::default(),
            comments_cursor: None,
            refresh,
        })
    }

    pub(crate) fn refresh_detail(&mut self) -> Option<GitHubViewQuery> {
        let identity = self.selected.clone()?;
        self.select(identity, true)
    }

    pub(crate) fn load_more_comments(&mut self) -> Option<GitHubViewQuery> {
        let identity = self.selected.as_ref()?;
        let cursor = self.detail.ready()?.comments.next_cursor.clone()?;
        let previous = self.detail.ready().cloned();
        self.detail = GitHubLoad::Loading(previous);
        Some(GitHubViewQuery {
            kind: identity.kind,
            number: identity.number,
            comments_page_size: GitHubPageSize::default(),
            comments_cursor: Some(cursor),
            refresh: false,
        })
    }

    pub(crate) fn receive_detail(&mut self, detail: GitHubItemDetail) {
        self.selected = Some(detail.summary.identity.clone());
        self.detail = GitHubLoad::Ready(detail);
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

    pub(crate) fn begin_browser_open(&mut self) -> Option<GitHubItemIdentity> {
        if self.browser_pending {
            return None;
        }
        let identity = self.detail.ready()?.summary.identity.clone();
        self.browser_pending = true;
        self.browser_error = None;
        Some(identity)
    }

    pub(crate) fn browser_opened(&mut self) {
        self.browser_pending = false;
        self.browser_error = None;
    }

    pub(crate) fn back_to_list(&mut self) {
        self.selected = None;
        self.detail = GitHubLoad::Idle;
        self.browser_pending = false;
        self.browser_error = None;
    }

    pub(crate) fn fail(
        &mut self,
        target: GitHubRequestTarget,
        mut failure: GitHubBrowseFailure,
        safe_diagnostic: String,
    ) {
        failure.diagnostic = safe_diagnostic;
        match target {
            GitHubRequestTarget::List => self.list = GitHubLoad::Failed(failure),
            GitHubRequestTarget::Detail => self.detail = GitHubLoad::Failed(failure),
            GitHubRequestTarget::Browser => {
                self.browser_pending = false;
                self.browser_error = Some(failure.diagnostic);
            }
        }
    }

    pub(crate) fn edit_filters_after_failure(&mut self) {
        if matches!(self.list, GitHubLoad::Failed(_)) {
            self.list = GitHubLoad::Idle;
        }
        self.filter_error = Some("Adjust one or more filters, then apply them again.".into());
    }

    fn reset(&mut self, project_id: ProjectId, kind: GitHubSurfaceKind) {
        self.project_id = Some(project_id);
        self.kind = kind;
        self.filters = GitHubListFilters::default();
        self.filter_input = GitHubFilterInput::default();
        self.filter_error = None;
        self.list = GitHubLoad::Idle;
        self.detail = GitHubLoad::Idle;
        self.selected = None;
        self.current_cursor = None;
        self.previous_cursors.clear();
        self.browser_pending = false;
        self.browser_error = None;
    }

    fn begin_list(&mut self, refresh: bool) -> GitHubListQuery {
        let previous = self.list.ready().cloned();
        self.list = GitHubLoad::Loading(previous);
        GitHubListQuery {
            kind: self.kind.item_kind(),
            filters: self.filters.clone(),
            page_size: GitHubPageSize::default(),
            cursor: self.current_cursor.clone(),
            refresh,
        }
    }
}

pub(crate) const fn failure_copy(failure: &GitHubBrowseFailure) -> GitHubFailureCopy {
    match failure.kind {
        GitHubBrowseFailureKind::CliUnavailable => GitHubFailureCopy {
            title: "GitHub CLI unavailable",
            action: "Open settings",
        },
        GitHubBrowseFailureKind::Authentication => GitHubFailureCopy {
            title: "GitHub authentication required",
            action: "Open settings",
        },
        GitHubBrowseFailureKind::Permission => GitHubFailureCopy {
            title: "Repository access denied",
            action: "Open settings",
        },
        GitHubBrowseFailureKind::RateLimited => GitHubFailureCopy {
            title: "GitHub rate limit reached",
            action: "Try again",
        },
        GitHubBrowseFailureKind::Network => GitHubFailureCopy {
            title: "GitHub is offline",
            action: "Try again",
        },
        GitHubBrowseFailureKind::TimedOut => GitHubFailureCopy {
            title: "GitHub request timed out",
            action: "Try again",
        },
        GitHubBrowseFailureKind::MalformedResponse => GitHubFailureCopy {
            title: "GitHub returned malformed data",
            action: "Refresh",
        },
        GitHubBrowseFailureKind::OversizedResponse => GitHubFailureCopy {
            title: "GitHub response exceeded safety limits",
            action: "Change filters",
        },
        GitHubBrowseFailureKind::UnavailableItem => GitHubFailureCopy {
            title: "This GitHub item is unavailable",
            action: "Back to list",
        },
    }
}

fn optional_value<T>(
    value: &str,
    constructor: impl FnOnce(String) -> Result<T, String>,
) -> Result<Option<T>, String> {
    let value = value.trim();
    if value.is_empty() {
        Ok(None)
    } else {
        constructor(value.to_owned()).map(Some)
    }
}

fn parse_labels(value: &str) -> Result<Vec<GitHubLabelName>, String> {
    let mut labels = Vec::new();
    for value in value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let label = GitHubLabelName::new(value)?;
        if !labels.contains(&label) {
            labels.push(label);
        }
    }
    if labels.len() > sylvops_core::github::MAX_GITHUB_FILTER_LABELS {
        return Err("Too many GitHub label filters.".into());
    }
    Ok(labels)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sylvops_core::github::{
        GitHubBody, GitHubCheckFilter, GitHubCommentPage, GitHubHost, GitHubItemNumber,
        GitHubItemState, GitHubListState, GitHubOwner, GitHubRepositoryIdentity,
        GitHubRepositoryName, GitHubReviewFilter, GitHubTimestamp, GitHubTitle,
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

    fn detail(kind: GitHubItemKind, number: u64) -> GitHubItemDetail {
        let identity = identity(kind, number);
        GitHubItemDetail {
            summary: sylvops_core::github::GitHubItemSummary {
                identity: identity.clone(),
                state: GitHubItemState::Open,
                title: GitHubTitle::new("Bounded title").unwrap(),
                author: None,
                labels: Vec::new(),
                assignees: Vec::new(),
                created_at: GitHubTimestamp::new("2026-10-13T12:00:00Z").unwrap(),
                updated_at: GitHubTimestamp::new("2026-10-13T12:00:00Z").unwrap(),
                pull_request: None,
            },
            body: GitHubBody::new("Untrusted body").unwrap(),
            comments: GitHubCommentPage {
                comments: Vec::new(),
                next_cursor: None,
            },
            browser_target: identity.browser_target(),
        }
    }

    #[test]
    fn opening_issues_builds_the_bounded_default_query_without_persisted_state() {
        let project_id = ProjectId::new();
        let mut surface = GitHubSurfaceState::default();

        let query = surface.open(project_id, GitHubSurfaceKind::Issues);

        assert_eq!(surface.project_id(), Some(project_id));
        assert_eq!(surface.kind(), GitHubSurfaceKind::Issues);
        assert_eq!(query.kind, GitHubItemKind::Issue);
        assert_eq!(query.filters.state, GitHubListState::Open);
        assert_eq!(query.page_size.get(), 30);
        assert!(query.cursor.is_none());
        assert!(!query.refresh);
    }

    #[test]
    fn filters_and_cursor_navigation_remain_ephemeral_and_bounded() {
        let mut surface = GitHubSurfaceState::default();
        surface.open(ProjectId::new(), GitHubSurfaceKind::PullRequests);
        surface.filter_input.text = "scheduler".into();
        surface.filter_input.author = "octocat".into();
        surface.filter_input.labels = "bug, urgent, bug".into();
        surface.filters.draft = Some(false);
        surface.filters.review = Some(GitHubReviewFilter::ReviewRequired);
        surface.filters.check = Some(GitHubCheckFilter::Failure);

        let query = surface.apply_filters().unwrap();
        assert_eq!(query.kind, GitHubItemKind::PullRequest);
        assert_eq!(query.filters.labels.len(), 2);
        assert_eq!(query.filters.draft, Some(false));
        assert_eq!(
            query.filters.review,
            Some(GitHubReviewFilter::ReviewRequired)
        );
        assert_eq!(query.filters.check, Some(GitHubCheckFilter::Failure));

        let next = GitHubCursor::new("next-page").unwrap();
        surface.receive_list(GitHubListPage {
            items: Vec::new(),
            next_cursor: Some(next.clone()),
        });
        assert_eq!(surface.next_page().unwrap().cursor, Some(next));
        assert!(surface.can_go_back());
        assert!(surface.previous_page().unwrap().cursor.is_none());
    }

    #[test]
    fn browser_action_is_bound_to_the_loaded_typed_identity() {
        let mut surface = GitHubSurfaceState::default();
        surface.open(ProjectId::new(), GitHubSurfaceKind::Issues);
        let identity = identity(GitHubItemKind::Issue, 125);
        let query = surface.select(identity.clone(), false).unwrap();
        assert_eq!(query.number, identity.number);
        assert!(surface.begin_browser_open().is_none());

        surface.receive_detail(detail(GitHubItemKind::Issue, 125));
        assert_eq!(surface.begin_browser_open(), Some(identity));
        assert!(surface.browser_pending);
        assert!(surface.begin_browser_open().is_none());
        surface.browser_opened();
        assert!(!surface.browser_pending);
    }

    #[test]
    fn every_remote_failure_has_distinct_actionable_copy() {
        let kinds = [
            GitHubBrowseFailureKind::CliUnavailable,
            GitHubBrowseFailureKind::Authentication,
            GitHubBrowseFailureKind::Permission,
            GitHubBrowseFailureKind::RateLimited,
            GitHubBrowseFailureKind::Network,
            GitHubBrowseFailureKind::TimedOut,
            GitHubBrowseFailureKind::MalformedResponse,
            GitHubBrowseFailureKind::OversizedResponse,
            GitHubBrowseFailureKind::UnavailableItem,
        ];
        let copies = kinds
            .map(GitHubBrowseFailure::new)
            .map(|failure| failure_copy(&failure));
        for (index, copy) in copies.iter().enumerate() {
            assert!(!copy.title.is_empty());
            assert!(!copy.action.is_empty());
            assert!(
                copies[index + 1..]
                    .iter()
                    .all(|other| other.title != copy.title)
            );
        }
    }
}
