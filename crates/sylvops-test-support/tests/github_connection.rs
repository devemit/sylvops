use std::{path::Path, time::Duration};
use sylvops_core::github::{
    GitHubBrowseFailureKind, GitHubCheckFilter, GitHubConnectionState, GitHubCursor, GitHubHost,
    GitHubItemKind, GitHubItemNumber, GitHubListFilters, GitHubListQuery, GitHubOwner,
    GitHubPageSize, GitHubRepositoryIdentity, GitHubRepositoryName, GitHubReviewFilter,
    GitHubSearchText, GitHubViewQuery,
};
use sylvops_daemon::github::GitHubGateway;

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
