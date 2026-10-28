#![allow(unsafe_code)]

use std::{ffi::OsString, path::Path, process::Command, time::Duration};

use sylvops_core::{
    domain::{ProviderKind, Session},
    github::{
        GitHubItemKind, GitHubItemNumber, GitHubLabelName, GitHubListFilters, GitHubListQuery,
        GitHubPageSize, GitHubSearchText, GitRemoteName, StartWorkBranch, StartWorkFailureKind,
        StartWorkInstructions, StartWorkOutcome,
    },
    protocol::{ClientRequest, DaemonResponse, SessionResumeOutcome},
    session_state::SessionLifecycle,
};
use sylvops_daemon::{client::DaemonClient, daemon, runtime::RuntimePaths};
use sylvops_test_support::{AbortOnDropTask, TemporaryRepository, git_branch_exists};

const ADDITIONAL_SENTINEL: &str = "ADDITIONAL_PRIVATE_SENTINEL";
const SEARCH_SENTINEL: &str = "SEARCH_PRIVATE_SENTINEL";
const FILTER_SENTINEL: &str = "FILTER_PRIVATE_SENTINEL";
const CREDENTIAL_SENTINEL: &str = "ghp_PRIVATE_SENTINEL";
const REMOTE_PROMPT_SENTINEL: &str = "REMOTE_PROMPT_PRIVATE_SENTINEL";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn authenticated_ipc_start_work_revalidates_and_preserves_only_provenance() {
    tokio::time::timeout(Duration::from_secs(120), scenario())
        .await
        .expect("Start Work integration scenario timeout");
}

#[allow(clippy::too_many_lines)]
async fn scenario() {
    let repository = TemporaryRepository::initialize().await.unwrap();
    git(
        repository.path(),
        &[
            "remote",
            "add",
            "origin",
            "git@enterprise.example.com:allowed/repository.git",
        ],
    );
    git(
        repository.path(),
        &[
            "remote",
            "add",
            "authentication-check",
            "git@auth.example.com:allowed/repository.git",
        ],
    );
    git(
        repository.path(),
        &[
            "remote",
            "add",
            "upstream",
            "git@enterprise.example.com:different/repository.git",
        ],
    );
    let _gh = EnvironmentGuard::set(
        "SYLVOPS_GH_CLI_PATH",
        OsString::from(env!("CARGO_BIN_EXE_fake-gh")),
    );
    let codex_path = repository.root().join(if cfg!(windows) {
        "fake-codex.exe"
    } else {
        "fake-codex"
    });
    std::fs::copy(env!("CARGO_BIN_EXE_fake-codex"), &codex_path).unwrap();
    let _codex = EnvironmentGuard::set("CODEX_CLI_PATH", codex_path.clone().into_os_string());
    let codex_home = repository.root().join("codex-home");
    let _codex_home = EnvironmentGuard::set("CODEX_HOME", codex_home.clone());
    let paths = RuntimePaths::discover(Some(&repository.root().join("state"))).unwrap();
    let mut daemon_task = AbortOnDropTask::spawn(daemon::run(paths.clone()));
    let client = connect_eventually(&paths).await;

    let workspace_id = match client
        .request(&ClientRequest::AddWorkspace {
            name: "Start Work".into(),
        })
        .await
        .unwrap()
    {
        DaemonResponse::WorkspaceAdded { workspace, .. } => workspace.id,
        response => panic!("unexpected workspace response: {response:?}"),
    };
    let project_id = match client
        .request(&ClientRequest::AddProject {
            workspace_id,
            repository_path: repository.path().to_string_lossy().into_owned(),
        })
        .await
        .unwrap()
    {
        DaemonResponse::ProjectAdded { project, .. } => project.id,
        response => panic!("unexpected Project response: {response:?}"),
    };
    let github_status = client
        .request(&ClientRequest::GetGitHubStatus {
            project_id: Some(project_id),
        })
        .await
        .unwrap();
    let DaemonResponse::GitHubStatus(github_status) = github_status else {
        panic!("unexpected GitHub status response: {github_status:?}");
    };
    let authentication = github_status
        .connections
        .iter()
        .find(|connection| connection.host.as_str() == "auth.example.com")
        .expect("authentication host status");
    assert_eq!(
        authentication.state,
        sylvops_core::github::GitHubConnectionState::AuthenticationRequired
    );
    assert!(!authentication.diagnostic.contains(CREDENTIAL_SENTINEL));
    assert!(
        matches!(
            client
                .request(&ClientRequest::ListGitHubItems {
                    project_id,
                    query: GitHubListQuery {
                        kind: GitHubItemKind::Issue,
                        filters: GitHubListFilters::default(),
                        page_size: GitHubPageSize::new(10).unwrap(),
                        cursor: None,
                        refresh: true,
                    },
                })
                .await
                .unwrap(),
            DaemonResponse::GitHubItems(_)
        ),
        "read-only Issue browsing must use origin without requiring saved setup"
    );
    assert!(matches!(
        client
            .request(&ClientRequest::ViewGitHubItem {
                project_id,
                query: sylvops_core::github::GitHubViewQuery {
                    kind: GitHubItemKind::Issue,
                    number: GitHubItemNumber::new(42).unwrap(),
                    comments_page_size: GitHubPageSize::new(10).unwrap(),
                    comments_cursor: None,
                    refresh: true,
                },
            })
            .await
            .unwrap(),
        DaemonResponse::GitHubItem(_)
    ));
    let DaemonResponse::GitHubStatus(after_browsing) = client
        .request(&ClientRequest::GetGitHubStatus {
            project_id: Some(project_id),
        })
        .await
        .unwrap()
    else {
        panic!("GitHub status");
    };
    assert!(
        after_browsing.association.is_none(),
        "browsing must not persist a repository choice"
    );
    assert!(matches!(
        client
            .request(&ClientRequest::SetGitHubAssociation {
                project_id,
                remote_name: GitRemoteName::new("origin").unwrap(),
                authorization_token: None,
            })
            .await
            .unwrap(),
        DaemonResponse::GitHubAssociationUpdated { .. }
    ));

    assert!(matches!(
        client
            .request(&ClientRequest::ListGitHubItems {
                project_id,
                query: GitHubListQuery {
                    kind: GitHubItemKind::Issue,
                    filters: GitHubListFilters {
                        text: Some(GitHubSearchText::new(SEARCH_SENTINEL).unwrap()),
                        labels: vec![GitHubLabelName::new(FILTER_SENTINEL).unwrap()],
                        ..GitHubListFilters::default()
                    },
                    page_size: GitHubPageSize::new(10).unwrap(),
                    cursor: None,
                    refresh: true,
                },
            })
            .await
            .unwrap(),
        DaemonResponse::GitHubItems(_)
    ));

    let preparation = prepare(&client, project_id).await;
    assert!(
        preparation
            .eligible_providers
            .contains(&ProviderKind::Codex)
    );
    assert!(
        !preparation
            .eligible_providers
            .contains(&ProviderKind::Shell)
    );
    assert!(!preparation.suggested_branch_conflict);
    assert_eq!(preparation.existing_work, []);

    let missing_retry = client
        .request(&ClientRequest::RetryStartWorkSession {
            worktree_id: sylvops_core::ids::WorktreeId::new(),
            provider: ProviderKind::Codex,
            display_name: None,
            columns: 100,
            rows: 30,
        })
        .await
        .unwrap();
    assert!(matches!(
        missing_retry,
        DaemonResponse::StartWorkFailure(ref failure)
            if failure.kind == StartWorkFailureKind::WorktreeUnavailable
    ));

    git(
        repository.path(),
        &["commit", "--allow-empty", "-m", "move base"],
    );
    let stale = client
        .request(&ClientRequest::StartWork {
            confirmation: preparation.confirmation(project_id),
            branch: preparation.suggested_branch,
            provider: ProviderKind::Codex,
            display_name: None,
            additional_instructions: None,
            start_another_worktree: false,
            columns: 100,
            rows: 30,
        })
        .await
        .unwrap();
    assert!(matches!(
        stale,
        DaemonResponse::StartWorkFailure(ref failure)
            if failure.kind == StartWorkFailureKind::BaseMoved
    ));

    let preparation = prepare(&client, project_id).await;
    let branch = preparation.suggested_branch.clone();
    let unavailable_codex = codex_path.with_extension("disabled");
    std::fs::rename(&codex_path, &unavailable_codex).unwrap();
    let capability_changed = client
        .request(&ClientRequest::StartWork {
            confirmation: preparation.confirmation(project_id),
            branch: branch.clone(),
            provider: ProviderKind::Codex,
            display_name: None,
            additional_instructions: None,
            start_another_worktree: false,
            columns: 100,
            rows: 30,
        })
        .await
        .unwrap();
    assert!(matches!(
        capability_changed,
        DaemonResponse::StartWorkFailure(ref failure)
            if failure.kind == StartWorkFailureKind::UnsupportedProvider
    ));
    std::fs::rename(&unavailable_codex, &codex_path).unwrap();

    git(repository.path(), &["branch", branch.as_str()]);
    let collision = client
        .request(&ClientRequest::StartWork {
            confirmation: preparation.confirmation(project_id),
            branch: branch.clone(),
            provider: ProviderKind::Codex,
            display_name: None,
            additional_instructions: None,
            start_another_worktree: false,
            columns: 100,
            rows: 30,
        })
        .await
        .unwrap();
    assert!(matches!(
        collision,
        DaemonResponse::StartWorkFailure(ref failure)
            if failure.kind == StartWorkFailureKind::BranchConflict
    ));
    git(repository.path(), &["branch", "-D", branch.as_str()]);

    let unsupported = client
        .request(&ClientRequest::StartWork {
            confirmation: preparation.confirmation(project_id),
            branch: branch.clone(),
            provider: ProviderKind::Shell,
            display_name: None,
            additional_instructions: None,
            start_another_worktree: false,
            columns: 100,
            rows: 30,
        })
        .await
        .unwrap();
    assert!(matches!(
        unsupported,
        DaemonResponse::StartWorkFailure(ref failure)
            if failure.kind == StartWorkFailureKind::UnsupportedProvider
    ));

    let outcome = client
        .request(&ClientRequest::StartWork {
            confirmation: preparation.confirmation(project_id),
            branch: branch.clone(),
            provider: ProviderKind::Codex,
            display_name: Some("Issue 42 custom".into()),
            additional_instructions: Some(StartWorkInstructions::new(ADDITIONAL_SENTINEL).unwrap()),
            start_another_worktree: false,
            columns: 100,
            rows: 30,
        })
        .await
        .unwrap();
    let (expected_worktree, session_id, session_name, expected_source) = match outcome {
        DaemonResponse::StartWorkCompleted(StartWorkOutcome::Started {
            worktree,
            source,
            session,
            ..
        }) => (worktree, session.id, session.display_name.clone(), source),
        response => panic!("unexpected Start Work response: {response:?}"),
    };
    let worktree_id = expected_worktree.id;
    assert_eq!(session_name, "Issue 42 custom");
    assert!(
        git_branch_exists(repository.path(), branch.as_str())
            .await
            .unwrap()
    );
    assert!(
        !git_branch_exists(repository.path(), "provider-policy-overridden")
            .await
            .unwrap()
    );
    assert!(!repository.path().join("remote-command-ran").exists());
    assert_eq!(
        client
            .request(&ClientRequest::GetWorkSource { worktree_id })
            .await
            .unwrap(),
        DaemonResponse::WorkSource(Some(expected_source.clone()))
    );
    let snapshot = match client.request(&ClientRequest::GetSnapshot).await.unwrap() {
        DaemonResponse::Snapshot(snapshot) => snapshot,
        response => panic!("unexpected snapshot response: {response:?}"),
    };
    assert_eq!(snapshot.work_sources, vec![expected_source.clone()]);
    let source_session = wait_for_session(&client, session_id, |session| {
        session.status.lifecycle == SessionLifecycle::Running
            && session.external_session_id.is_some()
    })
    .await;
    client
        .request(&ClientRequest::StopSession { session_id })
        .await
        .unwrap();
    wait_for_session(&client, session_id, |session| {
        session.status.lifecycle == SessionLifecycle::Terminated && session.process_id.is_none()
    })
    .await;
    let resumed_session = match client
        .request(&ClientRequest::ResumeSession {
            session_id,
            columns: 100,
            rows: 30,
        })
        .await
        .unwrap()
    {
        DaemonResponse::SessionResumed {
            predecessor_session_id: Some(predecessor_session_id),
            session,
            outcome: SessionResumeOutcome::CreatedSuccessor,
            ..
        } => {
            assert_eq!(predecessor_session_id, session_id);
            session
        }
        response => panic!("stopped Work Source Session did not resume: {response:?}"),
    };
    assert_eq!(resumed_session.worktree_id, source_session.worktree_id);
    assert_eq!(resumed_session.provider_kind, source_session.provider_kind);
    assert_eq!(
        resumed_session.provider_profile_id,
        source_session.provider_profile_id
    );
    assert_eq!(resumed_session.display_name, source_session.display_name);
    assert_eq!(
        resumed_session.external_session_id,
        source_session.external_session_id
    );
    assert_eq!(
        client
            .request(&ClientRequest::GetWorkSource { worktree_id })
            .await
            .unwrap(),
        DaemonResponse::WorkSource(Some(expected_source.clone()))
    );
    let resumed_session_id = resumed_session.id;
    let parallel_preparation = prepare(&client, project_id).await;
    assert_eq!(
        parallel_preparation.existing_work,
        vec![expected_source.clone()]
    );
    let parallel_branch = StartWorkBranch::new("issue/42-explicit-parallel").unwrap();
    let duplicate = client
        .request(&ClientRequest::StartWork {
            confirmation: parallel_preparation.confirmation(project_id),
            branch: parallel_branch.clone(),
            provider: ProviderKind::Codex,
            display_name: None,
            additional_instructions: None,
            start_another_worktree: false,
            columns: 100,
            rows: 30,
        })
        .await
        .unwrap();
    assert!(matches!(
        duplicate,
        DaemonResponse::StartWorkFailure(ref failure)
            if failure.kind == StartWorkFailureKind::ExistingWork
    ));
    let parallel = client
        .request(&ClientRequest::StartWork {
            confirmation: parallel_preparation.confirmation(project_id),
            branch: parallel_branch,
            provider: ProviderKind::Codex,
            display_name: None,
            additional_instructions: None,
            start_another_worktree: true,
            columns: 100,
            rows: 30,
        })
        .await
        .unwrap();
    let (parallel_worktree_id, parallel_session_id) = match parallel {
        DaemonResponse::StartWorkCompleted(StartWorkOutcome::Started {
            worktree,
            source,
            session,
            ..
        }) => {
            assert_ne!(worktree.id, worktree_id);
            assert_eq!(source.item, expected_source.item);
            (worktree.id, session.id)
        }
        response => panic!("unexpected parallel Start Work response: {response:?}"),
    };
    client
        .request(&ClientRequest::StopSession {
            session_id: parallel_session_id,
        })
        .await
        .unwrap();
    let partial_preparation = prepare(&client, project_id).await;
    let partial_branch = StartWorkBranch::new("issue/42-partial-recovery").unwrap();
    std::fs::write(
        codex_home.join(".fake-codex-invalidate-home"),
        b"invalidate",
    )
    .unwrap();
    let partial = client
        .request(&ClientRequest::StartWork {
            confirmation: partial_preparation.confirmation(project_id),
            branch: partial_branch.clone(),
            provider: ProviderKind::Codex,
            display_name: None,
            additional_instructions: None,
            start_another_worktree: true,
            columns: 100,
            rows: 30,
        })
        .await
        .unwrap();
    let (partial_worktree, partial_source) = match partial {
        DaemonResponse::StartWorkCompleted(StartWorkOutcome::WorktreeCreated {
            worktree,
            source,
            failure,
            ..
        }) => {
            assert_eq!(failure.kind, StartWorkFailureKind::ProviderLaunchFailed);
            assert_eq!(worktree.branch.as_deref(), Some(partial_branch.as_str()));
            (worktree, source)
        }
        response => panic!("unexpected partial Start Work response: {response:?}"),
    };
    assert!(
        git_branch_exists(repository.path(), partial_branch.as_str())
            .await
            .unwrap()
    );
    std::fs::write(
        Path::new(&partial_worktree.canonical_path).join("dirty.txt"),
        b"preserve me",
    )
    .unwrap();
    let dirty = client
        .request(&ClientRequest::GetWorktreeStatus {
            worktree_id: partial_worktree.id,
        })
        .await
        .unwrap();
    assert!(matches!(
        dirty,
        DaemonResponse::WorktreeStatus(ref checkout)
            if !checkout.clean && checkout.removal_confirmation_token.is_none()
    ));
    let snapshot = match client.request(&ClientRequest::GetSnapshot).await.unwrap() {
        DaemonResponse::Snapshot(snapshot) => snapshot,
        response => panic!("unexpected snapshot response: {response:?}"),
    };
    assert!(
        snapshot
            .work_sources
            .iter()
            .any(|source| source.worktree_id == worktree_id)
    );
    assert!(
        snapshot
            .work_sources
            .iter()
            .any(|source| source.worktree_id == parallel_worktree_id)
    );
    assert!(
        snapshot
            .work_sources
            .iter()
            .any(|source| source.worktree_id == partial_worktree.id)
    );
    let changed_remote = GitRemoteName::new("upstream").unwrap();
    let association_change = match client
        .request(&ClientRequest::SetGitHubAssociation {
            project_id,
            remote_name: changed_remote.clone(),
            authorization_token: None,
        })
        .await
        .unwrap()
    {
        DaemonResponse::GitHubAssociationChangeRequiresConfirmation(inspection) => inspection,
        response => panic!("unexpected association confirmation response: {response:?}"),
    };
    assert_eq!(
        association_change.remote_name.as_ref(),
        Some(&changed_remote)
    );
    assert_eq!(association_change.work_source_count, 3);
    let association_change = match client
        .request(&ClientRequest::SetGitHubAssociation {
            project_id,
            remote_name: changed_remote.clone(),
            authorization_token: Some(
                sylvops_core::protocol::GitHubAssociationChangeAuthorization::try_from(
                    "b".repeat(64),
                )
                .unwrap(),
            ),
        })
        .await
        .unwrap()
    {
        DaemonResponse::GitHubAssociationChangeRequiresConfirmation(inspection) => inspection,
        response => panic!("unexpected forged authorization response: {response:?}"),
    };
    assert!(matches!(
        client
            .request(&ClientRequest::SetGitHubAssociation {
                project_id,
                remote_name: changed_remote,
                authorization_token: Some(association_change.authorization_token),
            })
            .await
            .unwrap(),
        DaemonResponse::GitHubAssociationUpdated {
            association: Some(_),
            ..
        }
    ));
    let association_clear = match client
        .request(&ClientRequest::ClearGitHubAssociation {
            project_id,
            authorization_token: None,
        })
        .await
        .unwrap()
    {
        DaemonResponse::GitHubAssociationChangeRequiresConfirmation(inspection) => inspection,
        response => panic!("unexpected association confirmation response: {response:?}"),
    };
    assert_eq!(association_clear.project_id, project_id);
    assert_eq!(association_clear.work_source_count, 3);
    assert!(matches!(
        client
            .request(&ClientRequest::ClearGitHubAssociation {
                project_id,
                authorization_token: Some(association_clear.authorization_token),
            })
            .await
            .unwrap(),
        DaemonResponse::GitHubAssociationUpdated {
            association: None,
            ..
        }
    ));
    assert_eq!(
        client
            .request(&ClientRequest::GetWorkSource { worktree_id })
            .await
            .unwrap(),
        DaemonResponse::WorkSource(Some(expected_source.clone()))
    );
    let restored_association_inspection = match client
        .request(&ClientRequest::SetGitHubAssociation {
            project_id,
            remote_name: GitRemoteName::new("origin").unwrap(),
            authorization_token: None,
        })
        .await
        .unwrap()
    {
        DaemonResponse::GitHubAssociationChangeRequiresConfirmation(inspection) => inspection,
        response => panic!("unexpected restored association inspection: {response:?}"),
    };
    let restored_association = match client
        .request(&ClientRequest::SetGitHubAssociation {
            project_id,
            remote_name: GitRemoteName::new("origin").unwrap(),
            authorization_token: Some(restored_association_inspection.authorization_token),
        })
        .await
        .unwrap()
    {
        DaemonResponse::GitHubAssociationUpdated {
            association: Some(association),
            ..
        } => association,
        response => panic!("unexpected restored association response: {response:?}"),
    };
    client
        .request(&ClientRequest::StopSession {
            session_id: resumed_session_id,
        })
        .await
        .unwrap();
    client
        .request(&ClientRequest::ShutdownDaemon)
        .await
        .unwrap();
    daemon_task.join().await.unwrap().unwrap();

    let bytes = std::fs::read(&paths.database).unwrap();
    let logs = std::fs::read(&paths.daemon_log).unwrap_or_default();
    for private in [
        ADDITIONAL_SENTINEL,
        SEARCH_SENTINEL,
        FILTER_SENTINEL,
        CREDENTIAL_SENTINEL,
        REMOTE_PROMPT_SENTINEL,
        "</untrusted_github_issue>",
        "looks good",
        "filter-sentinel",
        "unsafe issue",
    ] {
        assert!(
            !bytes
                .windows(private.len())
                .any(|window| window == private.as_bytes()),
            "persistent state leaked {private:?}"
        );
        assert!(
            !logs
                .windows(private.len())
                .any(|window| window == private.as_bytes()),
            "daemon logs leaked {private:?}"
        );
    }
    let persisted = rusqlite::Connection::open(&paths.database).unwrap();
    let association_identity = persisted
        .query_row(
            "SELECT remote_name, host, owner, repository FROM project_github_associations WHERE project_id = ?1",
            [project_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(
        association_identity,
        (
            restored_association.remote_name.as_str().to_owned(),
            restored_association.repository.host().as_str().to_owned(),
            restored_association.repository.owner().as_str().to_owned(),
            restored_association
                .repository
                .repository()
                .as_str()
                .to_owned(),
        )
    );
    let persisted_sources: i64 = persisted
        .query_row("SELECT COUNT(*) FROM work_sources", [], |row| row.get(0))
        .unwrap();
    assert_eq!(persisted_sources, 3);
    drop(persisted);

    let mut restarted = AbortOnDropTask::spawn(daemon::run(paths.clone()));
    let client = connect_eventually(&paths).await;
    let status = client
        .request(&ClientRequest::GetGitHubStatus {
            project_id: Some(project_id),
        })
        .await
        .unwrap();
    assert!(matches!(
        status,
        DaemonResponse::GitHubStatus(ref status)
            if status.association.as_ref() == Some(&restored_association)
    ));
    assert_eq!(
        client
            .request(&ClientRequest::GetWorkSource {
                worktree_id: partial_worktree.id,
            })
            .await
            .unwrap(),
        DaemonResponse::WorkSource(Some(partial_source.clone()))
    );
    git(
        Path::new(&partial_worktree.canonical_path),
        &["switch", "--detach"],
    );
    let changed_identity = client
        .request(&ClientRequest::RetryStartWorkSession {
            worktree_id: partial_worktree.id,
            provider: ProviderKind::Codex,
            display_name: None,
            columns: 100,
            rows: 30,
        })
        .await
        .unwrap();
    assert!(matches!(
        changed_identity,
        DaemonResponse::StartWorkFailure(ref failure)
            if failure.kind == StartWorkFailureKind::WorktreeUnavailable
    ));
    git(
        Path::new(&partial_worktree.canonical_path),
        &["switch", partial_branch.as_str()],
    );
    let retried = client
        .request(&ClientRequest::RetryStartWorkSession {
            worktree_id: partial_worktree.id,
            provider: ProviderKind::Codex,
            display_name: Some("Issue 42 recovered".into()),
            columns: 100,
            rows: 30,
        })
        .await
        .unwrap();
    let retried_session_id = match retried {
        DaemonResponse::StartWorkCompleted(StartWorkOutcome::Started {
            worktree,
            source,
            session,
            ..
        }) => {
            assert_eq!(worktree.id, partial_worktree.id);
            assert_eq!(worktree.project_id, partial_worktree.project_id);
            assert_eq!(worktree.canonical_path, partial_worktree.canonical_path);
            assert_eq!(worktree.branch, partial_worktree.branch);
            assert_eq!(worktree.base_ref, partial_worktree.base_ref);
            assert_eq!(worktree.base_commit, partial_worktree.base_commit);
            assert_eq!(source, partial_source);
            assert_ne!(session.id, session_id);
            assert_eq!(session.display_name, "Issue 42 recovered");
            session.id
        }
        response => panic!("unexpected Retry Session response: {response:?}"),
    };
    client
        .request(&ClientRequest::StopSession {
            session_id: retried_session_id,
        })
        .await
        .unwrap();
    client
        .request(&ClientRequest::ShutdownDaemon)
        .await
        .unwrap();
    restarted.join().await.unwrap().unwrap();
}

async fn wait_for_session(
    client: &DaemonClient,
    session_id: sylvops_core::ids::SessionId,
    predicate: impl Fn(&Session) -> bool,
) -> Session {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut last_observed = None;
    loop {
        if let DaemonResponse::Snapshot(snapshot) = client
            .request(&ClientRequest::GetSnapshot)
            .await
            .expect("snapshot response")
            && let Some(session) = snapshot
                .sessions
                .into_iter()
                .find(|session| session.id == session_id)
        {
            if predicate(&session) {
                return session;
            }
            last_observed = Some(session);
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "Session status timeout; last observed: {last_observed:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn prepare(
    client: &DaemonClient,
    project_id: sylvops_core::ids::ProjectId,
) -> sylvops_core::github::StartWorkPreparation {
    match client
        .request(&ClientRequest::PrepareStartWork {
            project_id,
            issue_number: GitHubItemNumber::new(42).unwrap(),
            base: Some("HEAD".into()),
        })
        .await
        .unwrap()
    {
        DaemonResponse::StartWorkPrepared(preparation) => preparation,
        response => panic!("unexpected preparation response: {response:?}"),
    }
}

fn git(repository: &Path, arguments: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args([
            "-c",
            "user.name=SylvOps Test",
            "-c",
            "user.email=test@example.invalid",
        ])
        .args(arguments)
        .status()
        .unwrap();
    assert!(
        status.success(),
        "Git fixture command failed: {arguments:?}"
    );
}

async fn connect_eventually(paths: &RuntimePaths) -> DaemonClient {
    for _ in 0..100 {
        if let Ok(client) = DaemonClient::connect(paths, "start-work-integration").await {
            return client;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("daemon did not accept connections");
}

struct EnvironmentGuard {
    name: &'static str,
    previous: Option<OsString>,
}

impl EnvironmentGuard {
    fn set(name: &'static str, value: impl Into<OsString>) -> Self {
        let previous = std::env::var_os(name);
        unsafe { std::env::set_var(name, value.into()) };
        Self { name, previous }
    }
}

impl Drop for EnvironmentGuard {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take() {
            unsafe { std::env::set_var(self.name, previous) };
        } else {
            unsafe { std::env::remove_var(self.name) };
        }
    }
}
