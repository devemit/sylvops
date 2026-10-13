#![allow(unsafe_code)]

use std::{ffi::OsString, path::Path, process::Command, time::Duration};

use sylvops_core::{
    domain::ProviderKind,
    github::{
        GitHubItemNumber, GitRemoteName, StartWorkFailureKind, StartWorkInstructions,
        StartWorkOutcome,
    },
    protocol::{ClientRequest, DaemonResponse},
};
use sylvops_daemon::{client::DaemonClient, daemon, runtime::RuntimePaths};
use sylvops_test_support::{AbortOnDropTask, TemporaryRepository, git_branch_exists};

const ADDITIONAL_SENTINEL: &str = "ADDITIONAL_PRIVATE_SENTINEL";

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
    let _codex_home = EnvironmentGuard::set("CODEX_HOME", repository.root().join("codex-home"));
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
    assert!(matches!(
        client
            .request(&ClientRequest::SetGitHubAssociation {
                project_id,
                remote_name: GitRemoteName::new("origin").unwrap(),
            })
            .await
            .unwrap(),
        DaemonResponse::GitHubAssociationUpdated { .. }
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
    assert!(preparation.existing_work.is_empty());

    git(
        repository.path(),
        &["commit", "--allow-empty", "-m", "move base"],
    );
    let stale = client
        .request(&ClientRequest::StartWork {
            confirmation: preparation.confirmation(project_id),
            branch: preparation.suggested_branch,
            provider: ProviderKind::Codex,
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
            additional_instructions: Some(StartWorkInstructions::new(ADDITIONAL_SENTINEL).unwrap()),
            start_another_worktree: false,
            columns: 100,
            rows: 30,
        })
        .await
        .unwrap();
    let (worktree_id, session_id, expected_source) = match outcome {
        DaemonResponse::StartWorkCompleted(StartWorkOutcome::Started {
            worktree,
            source,
            session,
            ..
        }) => (worktree.id, session.id, source),
        response => panic!("unexpected Start Work response: {response:?}"),
    };
    assert!(
        git_branch_exists(repository.path(), branch.as_str())
            .await
            .unwrap()
    );
    assert_eq!(
        client
            .request(&ClientRequest::GetWorkSource { worktree_id })
            .await
            .unwrap(),
        DaemonResponse::WorkSource(Some(expected_source.clone()))
    );
    client
        .request(&ClientRequest::StopSession { session_id })
        .await
        .unwrap();
    client
        .request(&ClientRequest::ShutdownDaemon)
        .await
        .unwrap();
    daemon_task.join().await.unwrap().unwrap();

    let bytes = std::fs::read(&paths.database).unwrap();
    let logs = std::fs::read(&paths.daemon_log).unwrap_or_default();
    for private in [ADDITIONAL_SENTINEL, "safe\nbody", "looks good"] {
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

    let mut restarted = AbortOnDropTask::spawn(daemon::run(paths.clone()));
    let client = connect_eventually(&paths).await;
    assert_eq!(
        client
            .request(&ClientRequest::GetWorkSource { worktree_id })
            .await
            .unwrap(),
        DaemonResponse::WorkSource(Some(expected_source))
    );
    client
        .request(&ClientRequest::ShutdownDaemon)
        .await
        .unwrap();
    restarted.join().await.unwrap().unwrap();
}

async fn prepare(
    client: &DaemonClient,
    project_id: sylvops_core::ids::ProjectId,
) -> sylvops_core::github::StartWorkPreparation {
    match client
        .request(&ClientRequest::PrepareStartWork {
            project_id,
            issue_number: GitHubItemNumber::new(42).unwrap(),
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
