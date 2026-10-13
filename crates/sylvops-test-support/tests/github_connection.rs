use std::path::Path;
use sylvops_core::github::{
    GitHubConnectionState, GitHubHost, GitHubOwner, GitHubRepositoryIdentity, GitHubRepositoryName,
};
use sylvops_daemon::github::GitHubGateway;

fn repository(host: &str, owner: &str) -> GitHubRepositoryIdentity {
    GitHubRepositoryIdentity::new(
        GitHubHost::new(host).unwrap(),
        GitHubOwner::new(owner).unwrap(),
        GitHubRepositoryName::new("repository").unwrap(),
    )
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
