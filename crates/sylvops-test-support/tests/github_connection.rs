use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use sylvops_core::github::{
    GitHubBrowseFailureKind, GitHubCheckFilter, GitHubConnectionState, GitHubCursor, GitHubHost,
    GitHubItemKind, GitHubItemNumber, GitHubListFilters, GitHubListQuery, GitHubOwner,
    GitHubPageSize, GitHubRepositoryIdentity, GitHubRepositoryName, GitHubReviewFilter,
    GitHubSearchText, GitHubViewQuery, MAX_GITHUB_BODY_LENGTH, MAX_GITHUB_COMMENT_BODY_LENGTH,
    MAX_GITHUB_TITLE_LENGTH,
};
use sylvops_daemon::github::GitHubGateway;
use sylvops_test_support::fake_gh_artifact;

fn repository(host: &str, owner: &str) -> GitHubRepositoryIdentity {
    GitHubRepositoryIdentity::new(
        GitHubHost::new(host).unwrap(),
        GitHubOwner::new(owner).unwrap(),
        GitHubRepositoryName::new("repository").unwrap(),
    )
}

fn list_query(kind: GitHubItemKind) -> GitHubListQuery {
    GitHubListQuery {
        kind,
        filters: GitHubListFilters {
            text: Some(GitHubSearchText::new("filter-sentinel").unwrap()),
            draft: (kind == GitHubItemKind::PullRequest).then_some(false),
            review: (kind == GitHubItemKind::PullRequest).then_some(GitHubReviewFilter::Approved),
            check: (kind == GitHubItemKind::PullRequest).then_some(GitHubCheckFilter::Success),
            ..GitHubListFilters::default()
        },
        page_size: GitHubPageSize::new(10).unwrap(),
        cursor: None,
        refresh: false,
    }
}

fn view_query(kind: GitHubItemKind, number: u64) -> GitHubViewQuery {
    GitHubViewQuery {
        kind,
        number: GitHubItemNumber::new(number).unwrap(),
        comments_page_size: GitHubPageSize::new(10).unwrap(),
        comments_cursor: None,
        refresh: false,
    }
}

#[tokio::test]
async fn native_fake_gh_covers_enterprise_and_bounded_failure_states() {
    let gateway = GitHubGateway::with_executable(Path::new(env!("CARGO_BIN_EXE_fake-gh"))).unwrap();
    let targets = [
        repository("enterprise.example.com", "allowed"),
        repository("auth.example.com", "allowed"),
        repository("network.example.com", "allowed"),
        repository("rate.example.com", "allowed"),
        repository("unavailable.example.com", "allowed"),
        repository("oversized.example.com", "allowed"),
        repository("timeout.example.com", "allowed"),
        repository("permission.example.com", "denied"),
    ];

    let connections = gateway.probe(&targets).await;
    let state = |host: &str| {
        connections
            .iter()
            .find(|connection| connection.host.as_str() == host)
            .map(|connection| connection.state)
            .unwrap()
    };

    assert_eq!(
        state("github.com"),
        GitHubConnectionState::Ready,
        "the default GitHub.com host must use the same native fake-gh seam"
    );
    assert_eq!(
        state("enterprise.example.com"),
        GitHubConnectionState::Ready
    );
    assert_eq!(
        state("auth.example.com"),
        GitHubConnectionState::AuthenticationRequired
    );
    assert_eq!(
        state("network.example.com"),
        GitHubConnectionState::NetworkFailure
    );
    assert_eq!(
        state("rate.example.com"),
        GitHubConnectionState::RateLimited
    );
    assert_eq!(
        state("unavailable.example.com"),
        GitHubConnectionState::HostUnavailable
    );
    assert_eq!(
        state("oversized.example.com"),
        GitHubConnectionState::InvalidResponse
    );
    assert_eq!(
        state("timeout.example.com"),
        GitHubConnectionState::TimedOut
    );
    assert_eq!(
        state("permission.example.com"),
        GitHubConnectionState::PermissionFailure
    );
    assert!(
        connections
            .iter()
            .all(|connection| connection.diagnostic.len() <= 512
                && !connection.diagnostic.contains("PRIVATE_SENTINEL"))
    );
}

#[tokio::test]
async fn native_gh_is_discovered_from_the_os_path_without_an_explicit_override() {
    let temporary = tempfile::tempdir().unwrap();
    let executable_name = if cfg!(windows) { "gh.exe" } else { "gh" };
    let discovered = temporary.path().join(executable_name);
    std::fs::copy(env!("CARGO_BIN_EXE_fake-gh"), &discovered).unwrap();
    let expected = std::fs::canonicalize(&discovered).unwrap();
    let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "native_gh_discovery_child", "--nocapture"])
        .env("SYLVOPS_NATIVE_GH_DISCOVERY_CHILD", "1")
        .env("SYLVOPS_EXPECTED_GH_PATH", &expected)
        .env_remove("SYLVOPS_GH_CLI_PATH")
        .env("PATH", temporary.path())
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(10), command.output())
        .await
        .expect("native discovery child timed out")
        .unwrap();
    assert!(
        output.status.success(),
        "native discovery child failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("test native_gh_discovery_child ... ok"),
        "native discovery child did not run"
    );
}

#[test]
fn native_gh_discovery_child() {
    if std::env::var_os("SYLVOPS_NATIVE_GH_DISCOVERY_CHILD").is_none() {
        return;
    }
    let expected = std::fs::canonicalize(
        std::env::var_os("SYLVOPS_EXPECTED_GH_PATH").expect("expected fake gh path"),
    )
    .unwrap();
    let gateway = GitHubGateway::discover();
    assert_eq!(
        gateway.executable_path().as_deref(),
        expected.to_str(),
        "native GitHub CLI discovery did not select the PATH candidate"
    );
}

#[tokio::test]
async fn browse_lists_are_cursor_paginated_sanitized_cached_and_refreshable() {
    let gateway = GitHubGateway::with_executable(Path::new(env!("CARGO_BIN_EXE_fake-gh"))).unwrap();
    let repository = repository("enterprise.example.com", "allowed");
    let query = list_query(GitHubItemKind::Issue);

    let first = gateway.list_items(&repository, &query).await.unwrap();
    assert_eq!(first.items.len(), 1);
    assert_eq!(first.items[0].identity.repository, repository);
    assert_eq!(first.items[0].identity.number.get(), 42);
    assert!(!first.items[0].title.as_str().contains('\u{1b}'));
    assert!(!first.items[0].labels[0].as_str().contains('\n'));
    assert_eq!(first.next_cursor.as_ref().unwrap().as_str(), "cursor-one");
    assert_eq!(gateway.command_count(), 1);

    let cached = gateway.list_items(&repository, &query).await.unwrap();
    assert_eq!(cached, first);
    assert_eq!(gateway.command_count(), 1);

    let mut refreshed = query.clone();
    refreshed.refresh = true;
    gateway.list_items(&repository, &refreshed).await.unwrap();
    assert_eq!(gateway.command_count(), 2);

    let mut next = query;
    next.cursor = Some(GitHubCursor::new("cursor-one").unwrap());
    let second = gateway.list_items(&repository, &next).await.unwrap();
    assert!(second.items.is_empty());
    assert!(second.next_cursor.is_none());
}

#[tokio::test]
async fn browse_view_exposes_pull_request_detail_comments_checks_and_canonical_target() {
    let gateway = GitHubGateway::with_executable(Path::new(env!("CARGO_BIN_EXE_fake-gh"))).unwrap();
    let repository = repository("enterprise.example.com", "allowed");

    let detail = gateway
        .view_item(&repository, &view_query(GitHubItemKind::PullRequest, 7))
        .await
        .unwrap();

    assert_eq!(detail.summary.identity.kind, GitHubItemKind::PullRequest);
    let pull_request = detail.summary.pull_request.unwrap();
    assert_eq!(pull_request.changed_files.total, 3);
    assert_eq!(pull_request.checks.total, 4);
    assert_eq!(detail.comments.comments.len(), 1);
    assert!(!detail.comments.comments[0].body.as_str().contains('\u{1b}'));
    assert_eq!(
        detail.browser_target.as_str(),
        "https://enterprise.example.com/allowed/repository/pull/7"
    );
    assert_eq!(
        detail.comments.next_cursor.as_ref().unwrap().as_str(),
        "comment-cursor"
    );
}

#[tokio::test]
async fn browse_failures_cover_every_typed_category_without_leaking_cli_text() {
    let gateway = GitHubGateway::with_executable(Path::new(env!("CARGO_BIN_EXE_fake-gh"))).unwrap();
    for (host, expected) in [
        ("auth.example.com", GitHubBrowseFailureKind::Authentication),
        (
            "permission.example.com",
            GitHubBrowseFailureKind::Permission,
        ),
        ("rate.example.com", GitHubBrowseFailureKind::RateLimited),
        ("network.example.com", GitHubBrowseFailureKind::Network),
        ("timeout.example.com", GitHubBrowseFailureKind::TimedOut),
        (
            "malformed.example.com",
            GitHubBrowseFailureKind::MalformedResponse,
        ),
        (
            "nested.example.com",
            GitHubBrowseFailureKind::MalformedResponse,
        ),
        (
            "oversized.example.com",
            GitHubBrowseFailureKind::OversizedResponse,
        ),
    ] {
        let failure = gateway
            .list_items(
                &repository(host, "allowed"),
                &list_query(GitHubItemKind::Issue),
            )
            .await
            .unwrap_err();
        assert_eq!(failure.kind, expected, "unexpected category for {host}");
        assert_eq!(failure.retryable, expected.retryable());
        assert!(!failure.diagnostic.contains("PRIVATE_SENTINEL"));
    }

    let failure = gateway
        .view_item(
            &repository("enterprise.example.com", "allowed"),
            &view_query(GitHubItemKind::Issue, 404),
        )
        .await
        .unwrap_err();
    assert_eq!(failure.kind, GitHubBrowseFailureKind::UnavailableItem);

    let wrong_kind = gateway
        .view_item(
            &repository("enterprise.example.com", "allowed"),
            &view_query(GitHubItemKind::Issue, 7),
        )
        .await
        .unwrap_err();
    assert_eq!(
        wrong_kind.kind,
        GitHubBrowseFailureKind::UnavailableItem,
        "an item with the same number but a different kind is unavailable"
    );
}

#[tokio::test]
async fn dropping_a_browse_future_cancels_the_owned_cli_process() {
    let gateway = GitHubGateway::with_executable(Path::new(env!("CARGO_BIN_EXE_fake-gh"))).unwrap();
    let repository = repository("timeout.example.com", "allowed");
    let query = list_query(GitHubItemKind::Issue);

    assert!(
        tokio::time::timeout(
            Duration::from_millis(100),
            gateway.list_items(&repository, &query),
        )
        .await
        .is_err()
    );
    assert_eq!(gateway.command_count(), 1);
}

#[tokio::test]
async fn connection_state_changes_and_offline_browsing_recover_explicitly() {
    let host = format!("transition-{}.example.com", uuid::Uuid::new_v4().simple());
    let repository = repository(&host, "allowed");
    let state = FakeGhState::new(&host);
    let gateway = GitHubGateway::with_executable(Path::new(env!("CARGO_BIN_EXE_fake-gh"))).unwrap();

    state.set("authentication");
    assert_eq!(
        connection_state(&gateway, &repository).await,
        GitHubConnectionState::AuthenticationRequired
    );
    state.set("ready");
    assert_eq!(
        connection_state(&gateway, &repository).await,
        GitHubConnectionState::Ready
    );
    state.set("permission");
    assert_eq!(
        connection_state(&gateway, &repository).await,
        GitHubConnectionState::PermissionFailure
    );
    state.set("offline");
    assert_eq!(
        connection_state(&gateway, &repository).await,
        GitHubConnectionState::NetworkFailure
    );
    state.set("ready");
    assert_eq!(
        connection_state(&gateway, &repository).await,
        GitHubConnectionState::Ready
    );

    let query = list_query(GitHubItemKind::Issue);
    gateway.list_items(&repository, &query).await.unwrap();
    state.set("offline");
    let mut refresh = query.clone();
    refresh.refresh = true;
    assert_eq!(
        gateway
            .list_items(&repository, &refresh)
            .await
            .unwrap_err()
            .kind,
        GitHubBrowseFailureKind::Network
    );
    state.set("ready");
    assert_eq!(
        gateway
            .list_items(&repository, &refresh)
            .await
            .unwrap()
            .items
            .len(),
        1
    );
}

#[tokio::test]
async fn oversized_fields_pages_and_control_characters_stay_bounded() {
    let gateway = GitHubGateway::with_executable(Path::new(env!("CARGO_BIN_EXE_fake-gh"))).unwrap();
    let fields = repository("oversized-fields.example.com", "allowed");

    let page = gateway
        .list_items(&fields, &list_query(GitHubItemKind::Issue))
        .await
        .unwrap();
    assert_eq!(
        page.items[0].title.as_str().chars().count(),
        MAX_GITHUB_TITLE_LENGTH
    );
    assert!(!page.items[0].title.as_str().chars().any(char::is_control));

    let detail = gateway
        .view_item(&fields, &view_query(GitHubItemKind::Issue, 42))
        .await
        .unwrap();
    assert_eq!(detail.body.as_str().chars().count(), MAX_GITHUB_BODY_LENGTH);
    assert_eq!(
        detail.comments.comments[0].body.as_str().chars().count(),
        MAX_GITHUB_COMMENT_BODY_LENGTH
    );
    assert!(
        !detail
            .body
            .as_str()
            .chars()
            .any(|character| character == '\u{1b}')
    );
    assert!(
        !detail.comments.comments[0]
            .body
            .as_str()
            .chars()
            .any(|character| character == '\u{1b}')
    );

    let failure = gateway
        .list_items(
            &repository("oversized-page.example.com", "allowed"),
            &list_query(GitHubItemKind::Issue),
        )
        .await
        .unwrap_err();
    assert_eq!(failure.kind, GitHubBrowseFailureKind::MalformedResponse);
}

#[tokio::test]
async fn cancelling_browse_terminates_the_native_cli_before_it_can_complete() {
    let host = format!("cancel-{}.example.com", uuid::Uuid::new_v4().simple());
    let state = FakeGhState::new(&host);
    let gateway = GitHubGateway::with_executable(Path::new(env!("CARGO_BIN_EXE_fake-gh"))).unwrap();
    let query = list_query(GitHubItemKind::Issue);

    assert!(
        tokio::time::timeout(
            Duration::from_millis(100),
            gateway.list_items(&repository(&host, "allowed"), &query),
        )
        .await
        .is_err()
    );
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(
        !state.completed().exists(),
        "cancelled fake gh process completed"
    );
}

async fn connection_state(
    gateway: &GitHubGateway,
    repository: &GitHubRepositoryIdentity,
) -> GitHubConnectionState {
    gateway
        .probe(std::slice::from_ref(repository))
        .await
        .into_iter()
        .find(|connection| connection.host == *repository.host())
        .unwrap()
        .state
}

struct FakeGhState {
    path: PathBuf,
}

impl FakeGhState {
    fn new(host: &str) -> Self {
        let path = fake_gh_artifact(host, "state");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(fake_gh_artifact(host, "completed"));
        Self { path }
    }

    fn set(&self, state: &str) {
        std::fs::write(&self.path, state).unwrap();
    }

    fn completed(&self) -> PathBuf {
        self.path.with_extension("completed")
    }
}

impl Drop for FakeGhState {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(self.completed());
    }
}
