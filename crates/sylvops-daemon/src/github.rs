//! Daemon-owned, bounded access to an externally authenticated GitHub CLI.

use std::{
    collections::{BTreeSet, HashMap},
    ffi::OsStr,
    fs::File,
    io::Read as _,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use sylvops_core::github::{
    GitHubBody, GitHubBranchName, GitHubBrowseFailure, GitHubBrowseFailureKind,
    GitHubChangedFileSummary, GitHubCheckFilter, GitHubCheckState, GitHubCheckSummary,
    GitHubComment, GitHubCommentBody, GitHubCommentPage, GitHubConnection, GitHubConnectionState,
    GitHubCursor, GitHubHost, GitHubItemDetail, GitHubItemIdentity, GitHubItemKind,
    GitHubItemNumber, GitHubItemState, GitHubItemSummary, GitHubLabelName, GitHubListFilters,
    GitHubListPage, GitHubListQuery, GitHubListState, GitHubLogin, GitHubPullRequestSummary,
    GitHubRepositoryIdentity, GitHubReviewFilter, GitHubReviewState, GitHubTimestamp, GitHubTitle,
    GitHubViewQuery, MAX_GITHUB_BODY_LENGTH, MAX_GITHUB_COMMENT_BODY_LENGTH,
    MAX_GITHUB_CONNECTIONS, MAX_GITHUB_ITEM_ASSIGNEES, MAX_GITHUB_ITEM_LABELS,
    MAX_GITHUB_PAGE_SIZE, MAX_GITHUB_TITLE_LENGTH, sanitize_github_inline,
    sanitize_github_multiline,
};
use tokio::{io::AsyncReadExt as _, time::timeout};

use crate::background_process;

const GH_TIMEOUT: Duration = Duration::from_secs(5);
const GH_OUTPUT_LIMIT: u64 = 768 * 1024;
const MAX_PATH_DIRECTORIES: usize = 128;
const CACHE_LIFETIME: Duration = Duration::from_secs(60);
const MAX_CACHE_ENTRIES: usize = 64;

const LIST_QUERY: &str = r"query($search:String!,$first:Int!,$after:String){search(query:$search,type:ISSUE,first:$first,after:$after){nodes{__typename ... on Issue{number state title author{login} labels(first:50){nodes{name}} assignees(first:50){nodes{login}} createdAt updatedAt} ... on PullRequest{number state merged title author{login} labels(first:50){nodes{name}} assignees(first:50){nodes{login}} createdAt updatedAt headRefName baseRefName isDraft reviewDecision changedFiles additions deletions statusCheckRollup{state contexts(first:50){totalCount}}}} pageInfo{hasNextPage endCursor}}}";
const VIEW_QUERY: &str = r"query($owner:String!,$name:String!,$number:Int!,$commentsFirst:Int!,$commentsAfter:String){repository(owner:$owner,name:$name){issueOrPullRequest(number:$number){__typename ... on Issue{number state title body author{login} labels(first:50){nodes{name}} assignees(first:50){nodes{login}} createdAt updatedAt comments(first:$commentsFirst,after:$commentsAfter){nodes{author{login} body createdAt updatedAt} pageInfo{hasNextPage endCursor}}} ... on PullRequest{number state merged title body author{login} labels(first:50){nodes{name}} assignees(first:50){nodes{login}} createdAt updatedAt headRefName baseRefName isDraft reviewDecision changedFiles additions deletions statusCheckRollup{state contexts(first:50){totalCount}} comments(first:$commentsFirst,after:$commentsAfter){nodes{author{login} body createdAt updatedAt} pageInfo{hasNextPage endCursor}}}}}}";

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum CacheKey {
    List {
        repository: GitHubRepositoryIdentity,
        kind: GitHubItemKind,
        filters: GitHubListFilters,
        page_size: u8,
        cursor: Option<GitHubCursor>,
    },
    View {
        repository: GitHubRepositoryIdentity,
        kind: GitHubItemKind,
        number: GitHubItemNumber,
        comments_page_size: u8,
        comments_cursor: Option<GitHubCursor>,
    },
}

#[derive(Clone, Debug)]
enum CachedValue {
    List(GitHubListPage),
    Detail(Box<GitHubItemDetail>),
}

#[derive(Clone, Debug)]
struct CacheEntry {
    inserted: Instant,
    value: CachedValue,
}

#[derive(Debug, Default)]
struct BrowseCache {
    entries: HashMap<CacheKey, CacheEntry>,
}

impl BrowseCache {
    fn get(&mut self, key: &CacheKey) -> Option<CachedValue> {
        let now = Instant::now();
        self.entries
            .retain(|_, entry| now.duration_since(entry.inserted) <= CACHE_LIFETIME);
        self.entries.get(key).map(|entry| entry.value.clone())
    }

    fn insert(&mut self, key: CacheKey, value: CachedValue) {
        let now = Instant::now();
        self.entries
            .retain(|_, entry| now.duration_since(entry.inserted) <= CACHE_LIFETIME);
        if self.entries.len() >= MAX_CACHE_ENTRIES
            && !self.entries.contains_key(&key)
            && let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.inserted)
                .map(|(key, _)| key.clone())
        {
            self.entries.remove(&oldest);
        }
        self.entries.insert(
            key,
            CacheEntry {
                inserted: now,
                value,
            },
        );
    }
}

#[derive(Clone, Debug)]
pub struct GitHubGateway {
    executable: Option<PathBuf>,
    cache: Arc<Mutex<BrowseCache>>,
    command_count: Arc<AtomicUsize>,
}

impl GitHubGateway {
    #[must_use]
    pub fn discover() -> Self {
        Self {
            executable: discover_gh_executable(),
            cache: Arc::new(Mutex::new(BrowseCache::default())),
            command_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Creates a gateway around a specific native executable for deterministic integration tests.
    #[doc(hidden)]
    pub fn with_executable(executable: &Path) -> Result<Self, String> {
        let executable = native_executable(executable)
            .ok_or_else(|| "GitHub CLI path is not a native executable".to_owned())?;
        Ok(Self {
            executable: Some(executable),
            cache: Arc::new(Mutex::new(BrowseCache::default())),
            command_count: Arc::new(AtomicUsize::new(0)),
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

    /// Returns the number of bounded GitHub CLI processes launched by this gateway.
    #[doc(hidden)]
    #[must_use]
    pub fn command_count(&self) -> usize {
        self.command_count.load(Ordering::Relaxed)
    }

    /// Lists one bounded, cursor-paginated page through the external GitHub CLI.
    ///
    /// # Errors
    ///
    /// Returns a typed failure when the CLI cannot run, GitHub refuses the request, or the
    /// response is malformed, oversized, or late.
    pub async fn list_items(
        &self,
        repository: &GitHubRepositoryIdentity,
        query: &GitHubListQuery,
    ) -> Result<GitHubListPage, GitHubBrowseFailure> {
        let key = CacheKey::List {
            repository: repository.clone(),
            kind: query.kind,
            filters: query.filters.clone(),
            page_size: query.page_size.get(),
            cursor: query.cursor.clone(),
        };
        if !query.refresh
            && let Some(CachedValue::List(page)) = self.cached(&key)
        {
            return Ok(page);
        }

        let search = build_search(repository, query.kind, &query.filters);
        let mut arguments = vec![
            "api".to_owned(),
            "--hostname".to_owned(),
            repository.host().as_str().to_owned(),
            "graphql".to_owned(),
            "-f".to_owned(),
            format!("query={LIST_QUERY}"),
            "-f".to_owned(),
            format!("search={search}"),
            "-F".to_owned(),
            format!("first={}", query.page_size.get()),
        ];
        if let Some(cursor) = &query.cursor {
            arguments.extend(["-f".to_owned(), format!("after={}", cursor.as_str())]);
        }
        let output = self.execute(&arguments).await?;
        if !output.success {
            return Err(classify_browse_failure(&output, false));
        }
        let page = parse_list_page(
            &output.stdout,
            repository,
            query.kind,
            query.page_size.get(),
        )?;
        self.store(key, CachedValue::List(page.clone()));
        Ok(page)
    }

    /// Loads one bounded Issue or Pull Request detail and one page of its comments.
    ///
    /// # Errors
    ///
    /// Returns a typed failure when the CLI cannot run, the item is unavailable, GitHub refuses
    /// the request, or the response is malformed, oversized, or late.
    pub async fn view_item(
        &self,
        repository: &GitHubRepositoryIdentity,
        query: &GitHubViewQuery,
    ) -> Result<GitHubItemDetail, GitHubBrowseFailure> {
        let key = CacheKey::View {
            repository: repository.clone(),
            kind: query.kind,
            number: query.number,
            comments_page_size: query.comments_page_size.get(),
            comments_cursor: query.comments_cursor.clone(),
        };
        if !query.refresh
            && let Some(CachedValue::Detail(detail)) = self.cached(&key)
        {
            return Ok(*detail);
        }

        let mut arguments = vec![
            "api".to_owned(),
            "--hostname".to_owned(),
            repository.host().as_str().to_owned(),
            "graphql".to_owned(),
            "-f".to_owned(),
            format!("query={VIEW_QUERY}"),
            "-f".to_owned(),
            format!("owner={}", repository.owner().as_str()),
            "-f".to_owned(),
            format!("name={}", repository.repository().as_str()),
            "-F".to_owned(),
            format!("number={}", query.number.get()),
            "-F".to_owned(),
            format!("commentsFirst={}", query.comments_page_size.get()),
        ];
        if let Some(cursor) = &query.comments_cursor {
            arguments.extend([
                "-f".to_owned(),
                format!("commentsAfter={}", cursor.as_str()),
            ]);
        }
        let output = self.execute(&arguments).await?;
        if !output.success {
            return Err(classify_browse_failure(&output, true));
        }
        let detail = parse_item_detail(
            &output.stdout,
            repository,
            query.kind,
            query.number,
            query.comments_page_size.get(),
        )?;
        self.store(key, CachedValue::Detail(Box::new(detail.clone())));
        Ok(detail)
    }

    async fn execute(&self, arguments: &[String]) -> Result<CommandOutput, GitHubBrowseFailure> {
        let executable = self
            .executable
            .as_deref()
            .ok_or_else(|| GitHubBrowseFailure::new(GitHubBrowseFailureKind::CliUnavailable))?;
        if native_executable(executable).as_deref() != Some(executable) {
            return Err(GitHubBrowseFailure::new(
                GitHubBrowseFailureKind::CliUnavailable,
            ));
        }
        self.command_count.fetch_add(1, Ordering::Relaxed);
        run_gh(executable, arguments).await.map_err(capture_failure)
    }

    fn cached(&self, key: &CacheKey) -> Option<CachedValue> {
        self.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(key)
    }

    fn store(&self, key: CacheKey, value: CachedValue) {
        self.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key, value);
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

fn build_search(
    repository: &GitHubRepositoryIdentity,
    kind: GitHubItemKind,
    filters: &GitHubListFilters,
) -> String {
    let mut terms = vec![
        format!(
            "repo:\"{}/{}\"",
            repository.owner().as_str(),
            repository.repository().as_str()
        ),
        format!("is:{}", kind.search_qualifier()),
    ];
    match filters.state {
        GitHubListState::Open => terms.push("is:open".into()),
        GitHubListState::Closed => terms.push("is:closed".into()),
        GitHubListState::All => {}
    }
    if let Some(text) = &filters.text {
        terms.push(format!("\"{}\"", escape_search(text.as_str())));
    }
    if let Some(author) = &filters.author {
        terms.push(format!("author:\"{}\"", escape_search(author.as_str())));
    }
    if let Some(assignee) = &filters.assignee {
        terms.push(format!("assignee:\"{}\"", escape_search(assignee.as_str())));
    }
    for label in &filters.labels {
        terms.push(format!("label:\"{}\"", escape_search(label.as_str())));
    }
    if let Some(draft) = filters.draft {
        terms.push(if draft { "is:draft" } else { "-is:draft" }.into());
    }
    if let Some(review) = filters.review {
        terms.push(
            match review {
                GitHubReviewFilter::Approved => "review:approved",
                GitHubReviewFilter::ChangesRequested => "review:changes_requested",
                GitHubReviewFilter::ReviewRequired => "review:required",
            }
            .into(),
        );
    }
    if let Some(check) = filters.check {
        terms.push(
            match check {
                GitHubCheckFilter::Success => "status:success",
                GitHubCheckFilter::Failure => "status:failure",
                GitHubCheckFilter::Pending => "status:pending",
            }
            .into(),
        );
    }
    terms.push("sort:updated-desc".into());
    terms.join(" ")
}

fn escape_search(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn parse_list_page(
    bytes: &[u8],
    repository: &GitHubRepositoryIdentity,
    kind: GitHubItemKind,
    requested: u8,
) -> Result<GitHubListPage, GitHubBrowseFailure> {
    let root = parse_graphql(bytes)?;
    let search = root
        .pointer("/data/search")
        .ok_or_else(malformed_response)?;
    let nodes = search
        .get("nodes")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(malformed_response)?;
    if nodes.len() > usize::from(requested) || nodes.len() > usize::from(MAX_GITHUB_PAGE_SIZE) {
        return Err(malformed_response());
    }
    let items = nodes
        .iter()
        .map(|node| parse_summary(node, repository, kind, false))
        .collect::<Result<Vec<_>, _>>()?;
    let next_cursor = parse_next_cursor(search.get("pageInfo").ok_or_else(malformed_response)?)?;
    Ok(GitHubListPage { items, next_cursor })
}

fn parse_item_detail(
    bytes: &[u8],
    repository: &GitHubRepositoryIdentity,
    kind: GitHubItemKind,
    requested_number: GitHubItemNumber,
    requested_comments: u8,
) -> Result<GitHubItemDetail, GitHubBrowseFailure> {
    let root = parse_graphql(bytes)?;
    let repository_value = root
        .pointer("/data/repository")
        .ok_or_else(unavailable_item)?;
    if repository_value.is_null() {
        return Err(unavailable_item());
    }
    let item = repository_value
        .get("issueOrPullRequest")
        .filter(|value| !value.is_null())
        .ok_or_else(unavailable_item)?;
    let summary = parse_summary(item, repository, kind, true)?;
    if summary.identity.number != requested_number {
        return Err(malformed_response());
    }
    let body = item
        .get("body")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(malformed_response)?;
    let body = GitHubBody::new(sanitize_github_multiline(body, MAX_GITHUB_BODY_LENGTH))
        .map_err(|_| malformed_response())?;
    let comments_value = item.get("comments").ok_or_else(malformed_response)?;
    let comment_nodes = comments_value
        .get("nodes")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(malformed_response)?;
    if comment_nodes.len() > usize::from(requested_comments)
        || comment_nodes.len() > usize::from(MAX_GITHUB_PAGE_SIZE)
    {
        return Err(malformed_response());
    }
    let comments = comment_nodes
        .iter()
        .map(parse_comment)
        .collect::<Result<Vec<_>, _>>()?;
    let next_cursor = parse_next_cursor(
        comments_value
            .get("pageInfo")
            .ok_or_else(malformed_response)?,
    )?;
    let identity = summary.identity.clone();
    Ok(GitHubItemDetail {
        summary,
        body,
        comments: GitHubCommentPage {
            comments,
            next_cursor,
        },
        browser_target: identity.browser_target(),
    })
}

fn parse_graphql(bytes: &[u8]) -> Result<serde_json::Value, GitHubBrowseFailure> {
    let root =
        serde_json::from_slice::<serde_json::Value>(bytes).map_err(|_| malformed_response())?;
    if let Some(errors) = root.get("errors").and_then(serde_json::Value::as_array)
        && !errors.is_empty()
    {
        let message = errors
            .first()
            .and_then(|error| error.get("message"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        return Err(classify_graphql_failure(message));
    }
    if root.get("data").is_none() {
        return Err(malformed_response());
    }
    Ok(root)
}

fn parse_summary(
    node: &serde_json::Value,
    repository: &GitHubRepositoryIdentity,
    expected_kind: GitHubItemKind,
    kind_mismatch_is_unavailable: bool,
) -> Result<GitHubItemSummary, GitHubBrowseFailure> {
    let kind = match required_string(node, "__typename")? {
        "Issue" => GitHubItemKind::Issue,
        "PullRequest" => GitHubItemKind::PullRequest,
        _ => return Err(malformed_response()),
    };
    if kind != expected_kind {
        return Err(if kind_mismatch_is_unavailable {
            unavailable_item()
        } else {
            malformed_response()
        });
    }
    let number =
        GitHubItemNumber::new(required_u64(node, "number")?).map_err(|_| malformed_response())?;
    let merged = node
        .get("merged")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let state = if merged {
        GitHubItemState::Merged
    } else {
        match required_string(node, "state")? {
            "OPEN" => GitHubItemState::Open,
            "CLOSED" => GitHubItemState::Closed,
            _ => return Err(malformed_response()),
        }
    };
    let mut title =
        sanitize_github_inline(required_string(node, "title")?, MAX_GITHUB_TITLE_LENGTH);
    if title.is_empty() {
        title = "(untitled)".into();
    }
    let title = GitHubTitle::new(title).map_err(|_| malformed_response())?;
    let author = parse_optional_login(node.pointer("/author/login"))?;
    let labels = parse_labels(node.pointer("/labels/nodes"))?;
    let assignees = parse_assignees(node.pointer("/assignees/nodes"))?;
    let created_at = parse_timestamp(required_string(node, "createdAt")?)?;
    let updated_at = parse_timestamp(required_string(node, "updatedAt")?)?;
    let pull_request = if kind == GitHubItemKind::PullRequest {
        Some(GitHubPullRequestSummary {
            head_branch: parse_branch(required_string(node, "headRefName")?)?,
            base_branch: parse_branch(required_string(node, "baseRefName")?)?,
            draft: node
                .get("isDraft")
                .and_then(serde_json::Value::as_bool)
                .ok_or_else(malformed_response)?,
            review: match node
                .get("reviewDecision")
                .and_then(serde_json::Value::as_str)
            {
                Some("APPROVED") => GitHubReviewState::Approved,
                Some("CHANGES_REQUESTED") => GitHubReviewState::ChangesRequested,
                Some("REVIEW_REQUIRED") => GitHubReviewState::ReviewRequired,
                None => GitHubReviewState::None,
                Some(_) => return Err(malformed_response()),
            },
            checks: GitHubCheckSummary {
                state: parse_check_state(
                    node.pointer("/statusCheckRollup/state")
                        .and_then(serde_json::Value::as_str),
                )?,
                total: optional_u32(node.pointer("/statusCheckRollup/contexts/totalCount"))?,
            },
            changed_files: GitHubChangedFileSummary {
                total: required_u32(node, "changedFiles")?,
                additions: required_u32(node, "additions")?,
                deletions: required_u32(node, "deletions")?,
            },
        })
    } else {
        None
    };
    Ok(GitHubItemSummary {
        identity: GitHubItemIdentity::new(repository.clone(), kind, number),
        state,
        title,
        author,
        labels,
        assignees,
        created_at,
        updated_at,
        pull_request,
    })
}

fn parse_comment(value: &serde_json::Value) -> Result<GitHubComment, GitHubBrowseFailure> {
    let body = required_string(value, "body")?;
    Ok(GitHubComment {
        author: parse_optional_login(value.pointer("/author/login"))?,
        body: GitHubCommentBody::new(sanitize_github_multiline(
            body,
            MAX_GITHUB_COMMENT_BODY_LENGTH,
        ))
        .map_err(|_| malformed_response())?,
        created_at: parse_timestamp(required_string(value, "createdAt")?)?,
        updated_at: parse_timestamp(required_string(value, "updatedAt")?)?,
    })
}

fn parse_labels(
    value: Option<&serde_json::Value>,
) -> Result<Vec<GitHubLabelName>, GitHubBrowseFailure> {
    let nodes = value
        .and_then(serde_json::Value::as_array)
        .ok_or_else(malformed_response)?;
    if nodes.len() > MAX_GITHUB_ITEM_LABELS {
        return Err(malformed_response());
    }
    nodes
        .iter()
        .map(|node| {
            let value = sanitize_github_inline(required_string(node, "name")?, 128);
            GitHubLabelName::new(if value.is_empty() {
                "(removed)".into()
            } else {
                value
            })
            .map_err(|_| malformed_response())
        })
        .collect()
}

fn parse_assignees(
    value: Option<&serde_json::Value>,
) -> Result<Vec<GitHubLogin>, GitHubBrowseFailure> {
    let nodes = value
        .and_then(serde_json::Value::as_array)
        .ok_or_else(malformed_response)?;
    if nodes.len() > MAX_GITHUB_ITEM_ASSIGNEES {
        return Err(malformed_response());
    }
    nodes
        .iter()
        .filter_map(|node| node.get("login"))
        .map(|value| parse_optional_login(Some(value)))
        .filter_map(|result| match result {
            Ok(Some(login)) => Some(Ok(login)),
            Ok(None) => None,
            Err(error) => Some(Err(error)),
        })
        .collect()
}

fn parse_optional_login(
    value: Option<&serde_json::Value>,
) -> Result<Option<GitHubLogin>, GitHubBrowseFailure> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let value = value.as_str().ok_or_else(malformed_response)?;
    let value = sanitize_github_inline(value, 128);
    if value.is_empty() {
        return Ok(None);
    }
    GitHubLogin::new(value)
        .map(Some)
        .map_err(|_| malformed_response())
}

fn parse_branch(value: &str) -> Result<GitHubBranchName, GitHubBrowseFailure> {
    let value = sanitize_github_inline(value, 255);
    GitHubBranchName::new(if value.is_empty() {
        "(unavailable)".into()
    } else {
        value
    })
    .map_err(|_| malformed_response())
}

fn parse_timestamp(value: &str) -> Result<GitHubTimestamp, GitHubBrowseFailure> {
    GitHubTimestamp::new(sanitize_github_inline(value, 64)).map_err(|_| malformed_response())
}

fn parse_check_state(value: Option<&str>) -> Result<GitHubCheckState, GitHubBrowseFailure> {
    match value {
        None => Ok(GitHubCheckState::None),
        Some("SUCCESS") => Ok(GitHubCheckState::Success),
        Some("FAILURE" | "ERROR") => Ok(GitHubCheckState::Failure),
        Some("PENDING" | "EXPECTED") => Ok(GitHubCheckState::Pending),
        Some("NEUTRAL") => Ok(GitHubCheckState::Neutral),
        Some("SKIPPED") => Ok(GitHubCheckState::Skipped),
        Some(_) => Err(malformed_response()),
    }
}

fn parse_next_cursor(
    page_info: &serde_json::Value,
) -> Result<Option<GitHubCursor>, GitHubBrowseFailure> {
    let has_next = page_info
        .get("hasNextPage")
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(malformed_response)?;
    if !has_next {
        return Ok(None);
    }
    let cursor = page_info
        .get("endCursor")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(malformed_response)?;
    GitHubCursor::new(cursor.to_owned())
        .map(Some)
        .map_err(|_| malformed_response())
}

fn required_string<'a>(
    value: &'a serde_json::Value,
    field: &str,
) -> Result<&'a str, GitHubBrowseFailure> {
    value
        .get(field)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(malformed_response)
}

fn required_u64(value: &serde_json::Value, field: &str) -> Result<u64, GitHubBrowseFailure> {
    value
        .get(field)
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(malformed_response)
}

fn required_u32(value: &serde_json::Value, field: &str) -> Result<u32, GitHubBrowseFailure> {
    required_u64(value, field)?
        .try_into()
        .map_err(|_| malformed_response())
}

fn optional_u32(value: Option<&serde_json::Value>) -> Result<u32, GitHubBrowseFailure> {
    match value {
        None => Ok(0),
        Some(value) if value.is_null() => Ok(0),
        Some(value) => value
            .as_u64()
            .ok_or_else(malformed_response)?
            .try_into()
            .map_err(|_| malformed_response()),
    }
}

fn capture_failure(error: CaptureError) -> GitHubBrowseFailure {
    GitHubBrowseFailure::new(match error {
        CaptureError::Spawn => GitHubBrowseFailureKind::CliUnavailable,
        CaptureError::TimedOut => GitHubBrowseFailureKind::TimedOut,
        CaptureError::Oversized => GitHubBrowseFailureKind::OversizedResponse,
    })
}

fn classify_browse_failure(output: &CommandOutput, item_request: bool) -> GitHubBrowseFailure {
    let mut bytes = Vec::with_capacity(output.stdout.len().saturating_add(output.stderr.len()));
    bytes.extend_from_slice(&output.stdout);
    bytes.extend_from_slice(&output.stderr);
    let text = String::from_utf8_lossy(&bytes).to_ascii_lowercase();
    let kind = if text.contains("rate limit") || text.contains("429") {
        GitHubBrowseFailureKind::RateLimited
    } else if text.contains("not logged")
        || text.contains("authentication")
        || text.contains("bad credentials")
        || text.contains("401")
    {
        GitHubBrowseFailureKind::Authentication
    } else if text.contains("403")
        || text.contains("forbidden")
        || text.contains("permission")
        || text.contains("resource not accessible")
    {
        GitHubBrowseFailureKind::Permission
    } else if item_request && (text.contains("404") || text.contains("not found")) {
        GitHubBrowseFailureKind::UnavailableItem
    } else if text.contains("network")
        || text.contains("resolve host")
        || text.contains("connection")
        || text.contains("offline")
        || text.contains("tls")
    {
        GitHubBrowseFailureKind::Network
    } else {
        GitHubBrowseFailureKind::MalformedResponse
    };
    GitHubBrowseFailure::new(kind)
}

fn classify_graphql_failure(message: &str) -> GitHubBrowseFailure {
    let lowered = message.to_ascii_lowercase();
    let kind = if lowered.contains("rate limit") {
        GitHubBrowseFailureKind::RateLimited
    } else if lowered.contains("authentication") || lowered.contains("bad credentials") {
        GitHubBrowseFailureKind::Authentication
    } else if lowered.contains("permission") || lowered.contains("resource not accessible") {
        GitHubBrowseFailureKind::Permission
    } else if lowered.contains("not found") || lowered.contains("could not resolve") {
        GitHubBrowseFailureKind::UnavailableItem
    } else {
        GitHubBrowseFailureKind::MalformedResponse
    };
    GitHubBrowseFailure::new(kind)
}

fn malformed_response() -> GitHubBrowseFailure {
    GitHubBrowseFailure::new(GitHubBrowseFailureKind::MalformedResponse)
}

fn unavailable_item() -> GitHubBrowseFailure {
    GitHubBrowseFailure::new(GitHubBrowseFailureKind::UnavailableItem)
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

async fn run_gh<S: AsRef<OsStr>>(
    executable: &Path,
    arguments: &[S],
) -> Result<CommandOutput, CaptureError> {
    let mut command = background_process::command(executable);
    command
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
    for argument in arguments {
        command.arg(argument.as_ref());
    }
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
        let gateway = GitHubGateway {
            executable: None,
            ..GitHubGateway::discover()
        };
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
