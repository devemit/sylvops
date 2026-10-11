use std::{path::Path, process::Output, process::Stdio, time::Duration};

use sylvops_core::{
    ids::WorktreeId,
    protocol::{ClientRequest, DaemonResponse},
};
use sylvops_daemon::{client::DaemonClient, daemon, runtime::RuntimePaths};
use sylvops_test_support::{AbortOnDropTask, TemporaryRepository, bounded, git_branch_exists};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cli_removes_only_a_confirmed_clean_managed_worktree_and_preserves_its_branch() {
    tokio::time::timeout(Duration::from_secs(30), exercise_cli_worktree_removal())
        .await
        .expect("CLI Worktree removal timeout");
}

async fn exercise_cli_worktree_removal() {
    let repository = TemporaryRepository::initialize().await.unwrap();
    let state_directory = repository.root().join("state");
    let paths = RuntimePaths::discover(Some(&state_directory)).unwrap();
    let daemon_paths = paths.clone();
    let mut daemon_task = AbortOnDropTask::spawn(async move { daemon::run(daemon_paths).await });
    let client = connect_eventually(&paths).await;

    let workspace = match client
        .request(&ClientRequest::AddWorkspace {
            name: "CLI Worktree removal".into(),
        })
        .await
        .unwrap()
    {
        DaemonResponse::WorkspaceAdded { workspace, .. } => workspace,
        other => panic!("unexpected Workspace response: {other:?}"),
    };
    let project = match client
        .request(&ClientRequest::AddProject {
            workspace_id: workspace.id,
            repository_path: repository.path().to_string_lossy().into_owned(),
        })
        .await
        .unwrap()
    {
        DaemonResponse::ProjectAdded { project, .. } => project,
        other => panic!("unexpected Project response: {other:?}"),
    };
    let worktree = match client
        .request(&ClientRequest::CreateWorktree {
            project_id: project.id,
            name: Some("CLI removable".into()),
            branch: "feature/cli-removal".into(),
            base_ref: Some("HEAD".into()),
        })
        .await
        .unwrap()
    {
        DaemonResponse::WorktreeCreated { worktree, .. } => worktree,
        other => panic!("unexpected Worktree response: {other:?}"),
    };
    let worktree_path = Path::new(&worktree.canonical_path);

    let unconfirmed = run_remove(&state_directory, worktree.id, false).await;
    assert!(!unconfirmed.status.success());
    assert!(String::from_utf8_lossy(&unconfirmed.stderr).contains("--confirm"));
    assert!(worktree_path.exists());

    let sentinel = worktree_path.join("untracked-preserved.txt");
    std::fs::write(&sentinel, "preserved\n").unwrap();
    let dirty = run_remove(&state_directory, worktree.id, true).await;
    assert!(!dirty.status.success());
    assert!(String::from_utf8_lossy(&dirty.stdout).contains("Untracked files: 1"));
    assert!(String::from_utf8_lossy(&dirty.stderr).contains("refusing to remove"));
    assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "preserved\n");
    std::fs::remove_file(&sentinel).unwrap();

    let removed = run_remove(&state_directory, worktree.id, true).await;
    assert!(
        removed.status.success(),
        "CLI Worktree removal failed: {}",
        String::from_utf8_lossy(&removed.stderr)
    );
    let output = String::from_utf8_lossy(&removed.stdout);
    assert!(output.contains(&format!(
        "Removed worktree 'CLI removable' ({})",
        worktree.id
    )));
    assert!(output.contains("Branch preserved: feature/cli-removal"));
    assert!(!worktree_path.exists());
    assert!(
        git_branch_exists(repository.path(), "feature/cli-removal")
            .await
            .unwrap()
    );

    client
        .request(&ClientRequest::ShutdownDaemon)
        .await
        .unwrap();
    daemon_task.join().await.unwrap().unwrap();
}

async fn run_remove(state_directory: &Path, worktree_id: WorktreeId, confirm: bool) -> Output {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_sylvops"));
    command
        .arg("--state-dir")
        .arg(state_directory)
        .arg("worktree")
        .arg("remove")
        .arg(worktree_id.to_string());
    if confirm {
        command.arg("--confirm");
    }
    command.stdin(Stdio::null()).kill_on_drop(true);
    bounded(command.output())
        .await
        .expect("CLI Worktree removal command timeout")
        .expect("CLI Worktree removal command")
}

async fn connect_eventually(paths: &RuntimePaths) -> DaemonClient {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match DaemonClient::connect(paths, "cli-worktree-remove-test").await {
            Ok(client) => return client,
            Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(error) => panic!("daemon did not become ready: {error}"),
        }
    }
}
