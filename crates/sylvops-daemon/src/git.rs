//! Bounded, structured Git repository and managed-worktree operations.

use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use sha2::{Digest, Sha256};
use sylvops_core::{
    domain::{GitDiff, GitWorktreeState, Project, Worktree, WorktreeStatus},
    ids::{WorkspaceId, WorktreeId},
};
use tokio::{io::AsyncReadExt, time::timeout};

use crate::{
    DaemonError, Result, background_process,
    database::{NewManagedWorktree, RepositoryRegistration},
};

const GIT_TIMEOUT: Duration = Duration::from_secs(5);
const OUTPUT_LIMIT: u64 = 64 * 1024;
const DIFF_LIMIT: u64 = 512 * 1024;

#[derive(Debug)]
struct GitOutput {
    success: bool,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Creates and validates the private root used for managed worktrees.
///
/// # Errors
///
/// Returns an error for relative/root paths, filesystem failures, or link/reparse-point roots.
pub async fn prepare_managed_root(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() || path.parent().is_none() {
        return Err(DaemonError::Git(
            "managed worktree root must be absolute and cannot be a filesystem root".into(),
        ));
    }
    tokio::fs::create_dir_all(path)
        .await
        .map_err(|error| DaemonError::Git(format!("cannot create managed root: {error}")))?;
    reject_link(path)?;
    let canonical = tokio::fs::canonicalize(path)
        .await
        .map_err(|error| DaemonError::Git(format!("cannot canonicalize managed root: {error}")))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&canonical, fs::Permissions::from_mode(0o700))
            .await
            .map_err(|error| DaemonError::Git(format!("cannot protect managed root: {error}")))?;
    }
    let hooks = canonical.join(".disabled-hooks");
    tokio::fs::create_dir_all(&hooks).await.map_err(|error| {
        DaemonError::Git(format!("cannot create empty hooks directory: {error}"))
    })?;
    reject_link(&hooks)?;
    Ok(canonical)
}

/// Inspects a local repository without contacting a remote or mutating Git state.
///
/// # Errors
///
/// Returns an error for invalid paths, Git failures, timeouts, or oversized output.
pub async fn inspect_repository(
    workspace_id: WorkspaceId,
    supplied_path: &Path,
) -> Result<RepositoryRegistration> {
    let supplied = tokio::fs::canonicalize(supplied_path)
        .await
        .map_err(|error| {
            DaemonError::Git(format!("cannot canonicalize repository path: {error}"))
        })?;
    let root_output = run_git(&supplied, &["rev-parse", "--show-toplevel"]).await?;
    if !root_output.success {
        return Err(DaemonError::Git(format!(
            "path is not a Git repository: {}",
            safe_message(&root_output.stderr)
        )));
    }
    let root = tokio::fs::canonicalize(PathBuf::from(output_text(&root_output.stdout)?.trim()))
        .await
        .map_err(|error| DaemonError::Git(format!("cannot canonicalize Git root: {error}")))?;
    let canonical = path_text(&root)?;
    let branch = optional_git(&root, &["symbolic-ref", "--quiet", "--short", "HEAD"]).await?;
    let remote_head = optional_git(
        &root,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    )
    .await?;
    let default_branch = remote_head
        .and_then(|value| value.strip_prefix("origin/").map(str::to_owned))
        .or_else(|| branch.clone());
    let remote_url = optional_git(&root, &["config", "--get", "remote.origin.url"])
        .await?
        .map(|value| sanitize_remote_url(&value));
    let base_commit = required_git(&root, &["rev-parse", "HEAD"], "resolve HEAD").await?;
    let name = root
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("repository")
        .chars()
        .take(200)
        .collect();

    Ok(RepositoryRegistration {
        workspace_id,
        name,
        repository_path: canonical.clone(),
        canonical_repository_path: canonical,
        default_branch,
        remote_url,
        branch,
        base_commit,
    })
}

/// Lists canonical worktree paths reported by Git without mutating or contacting a remote.
/// Discovers canonical worktree paths reported by Git without mutating them.
///
/// # Errors
///
/// Returns an error when Git times out, exceeds output limits, or returns malformed output.
pub async fn discover_worktree_paths(project: &Project) -> Result<Vec<String>> {
    let repository = canonical_registered_path(&project.canonical_repository_path).await?;
    let output = run_git(&repository, &["worktree", "list", "--porcelain", "-z"]).await?;
    if !output.success {
        return Err(DaemonError::Git(format!(
            "failed to discover worktrees: {}",
            safe_message(&output.stderr)
        )));
    }
    let mut paths = Vec::new();
    for field in output.stdout.split(|byte| *byte == 0 || *byte == b'\n') {
        let Some(path) = field.strip_prefix(b"worktree ") else {
            continue;
        };
        let Ok(path) = std::str::from_utf8(path) else {
            continue;
        };
        if let Ok(canonical) = tokio::fs::canonicalize(path).await
            && let Ok(text) = path_text(&canonical)
        {
            paths.push(text);
        }
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// Revalidates that a canonical path is still the root of its checkout.
///
/// # Errors
///
/// Returns an error if the path changed, Git fails, or the checkout is no longer a root.
pub async fn verify_repository_root(path: &Path) -> Result<()> {
    let canonical = tokio::fs::canonicalize(path).await.map_err(|error| {
        DaemonError::Git(format!("cannot canonicalize repository root: {error}"))
    })?;
    let output = run_git(&canonical, &["rev-parse", "--show-toplevel"]).await?;
    if !output.success {
        return Err(DaemonError::Git(
            "registered checkout is no longer a Git repository".into(),
        ));
    }
    let reported = tokio::fs::canonicalize(PathBuf::from(output_text(&output.stdout)?.trim()))
        .await
        .map_err(|error| {
            DaemonError::Git(format!("cannot canonicalize reported Git root: {error}"))
        })?;
    if reported != canonical {
        return Err(DaemonError::Git(
            "registered checkout is no longer the checkout root".into(),
        ));
    }
    Ok(())
}

/// Creates a new branch in an opaque, contained managed-worktree directory.
///
/// # Errors
///
/// Returns an error for invalid names/refs, duplicate branches, containment failures, Git
/// failures, or verification failures. Existing paths are never overwritten.
pub async fn create_managed_worktree(
    project: &Project,
    managed_root: &Path,
    id: WorktreeId,
    name: Option<String>,
    branch: &str,
    base_ref: Option<&str>,
) -> Result<NewManagedWorktree> {
    validate_branch_name(branch)?;
    let display_name = validate_display_name(name.as_deref().unwrap_or(branch))?;
    let repository = canonical_registered_path(&project.canonical_repository_path).await?;
    verify_repository_root(&repository).await?;
    validate_branch_with_git(&repository, branch).await?;
    let branch_ref = format!("refs/heads/{branch}");
    if run_git(
        &repository,
        &["show-ref", "--verify", "--quiet", &branch_ref],
    )
    .await?
    .success
    {
        return Err(DaemonError::Git(format!(
            "branch '{branch}' already exists"
        )));
    }

    let base_ref = base_ref.unwrap_or("HEAD");
    validate_base_ref(base_ref)?;
    let commit_expression = format!("{base_ref}^{{commit}}");
    let base_commit = required_git(
        &repository,
        &[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &commit_expression,
        ],
        "resolve worktree base",
    )
    .await?;

    let project_directory = managed_root.join(project.id.to_string());
    tokio::fs::create_dir_all(&project_directory)
        .await
        .map_err(|error| {
            DaemonError::Git(format!("cannot create project worktree root: {error}"))
        })?;
    reject_link(&project_directory)?;
    let project_directory = tokio::fs::canonicalize(&project_directory)
        .await
        .map_err(|error| {
            DaemonError::Git(format!("cannot verify project worktree root: {error}"))
        })?;
    ensure_direct_child(managed_root, &project_directory)?;

    let destination = project_directory.join(id.to_string());
    if tokio::fs::symlink_metadata(&destination).await.is_ok() {
        return Err(DaemonError::Git(
            "managed worktree destination already exists".into(),
        ));
    }
    let hooks = managed_root.join(".disabled-hooks");
    let arguments = vec![
        OsString::from("worktree"),
        OsString::from("add"),
        OsString::from("-b"),
        OsString::from(branch),
        git_path_argument(&destination),
        OsString::from(base_commit.as_str()),
    ];
    let output = run_git_mutation(&repository, &hooks, &arguments).await?;
    if !output.success {
        return Err(DaemonError::Git(format!(
            "failed to create worktree: {}",
            safe_message(&output.stderr)
        )));
    }

    reject_link(&destination)?;
    let canonical = tokio::fs::canonicalize(&destination)
        .await
        .map_err(|error| DaemonError::Git(format!("cannot verify new worktree: {error}")))?;
    if canonical.parent() != Some(project_directory.as_path()) {
        return Err(DaemonError::Git(
            "new worktree escaped its managed project directory".into(),
        ));
    }
    verify_repository_root(&canonical).await?;
    verify_common_repository(&repository, &canonical).await?;
    let actual_commit =
        required_git(&canonical, &["rev-parse", "HEAD"], "verify worktree HEAD").await?;
    if actual_commit != base_commit {
        return Err(DaemonError::Git(
            "new worktree HEAD does not match the resolved base commit".into(),
        ));
    }

    let canonical_text = path_text(&canonical)?;
    Ok(NewManagedWorktree {
        id,
        project_id: project.id,
        name: display_name,
        path: canonical_text.clone(),
        canonical_path: canonical_text,
        branch: branch.to_owned(),
        base_ref: base_ref.to_owned(),
        base_commit,
    })
}

/// Inspects tracked, untracked, and ignored state and returns a state-bound removal token.
///
/// # Errors
///
/// Returns an error if the checkout identity changed or Git output is unavailable/oversized.
pub async fn inspect_worktree(worktree: &Worktree) -> Result<GitWorktreeState> {
    if worktree.status != WorktreeStatus::Active {
        return Err(DaemonError::Git("worktree is not active".into()));
    }
    let path = canonical_registered_path(&worktree.canonical_path).await?;
    verify_repository_root(&path).await?;
    let head_commit = required_git(&path, &["rev-parse", "HEAD"], "resolve worktree HEAD").await?;
    let branch = optional_git(&path, &["symbolic-ref", "--quiet", "--short", "HEAD"]).await?;
    let output = run_git(
        &path,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignored=matching",
        ],
    )
    .await?;
    if !output.success {
        return Err(DaemonError::Git(format!(
            "failed to inspect worktree: {}",
            safe_message(&output.stderr)
        )));
    }
    let (tracked_changes, untracked_files, ignored_files) = count_status_entries(&output.stdout);
    let clean = output.stdout.is_empty();
    let removal_confirmation_token = clean.then(|| {
        state_token(
            worktree.id,
            &worktree.canonical_path,
            &head_commit,
            branch.as_deref(),
            &output.stdout,
        )
    });
    Ok(GitWorktreeState {
        worktree_id: worktree.id,
        head_commit,
        branch,
        tracked_changes,
        untracked_files,
        ignored_files,
        clean,
        removal_confirmation_token,
    })
}

/// Returns a bounded, no-color unified diff for an active verified worktree.
///
/// # Errors
///
/// Returns an error when worktree identity validation or the bounded Git command fails.
pub async fn worktree_diff(worktree: &Worktree) -> Result<GitDiff> {
    if worktree.status != WorktreeStatus::Active {
        return Err(DaemonError::Git("worktree is not active".into()));
    }
    let path = canonical_registered_path(&worktree.canonical_path).await?;
    verify_repository_root(&path).await?;
    let mut child = background_process::command("git");
    child
        .arg("-C")
        .arg(&path)
        .args([
            "diff",
            "HEAD",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--",
        ])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = child
        .spawn()
        .map_err(|error| DaemonError::Git(format!("failed to launch Git: {error}")))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| DaemonError::Git("Git stdout unavailable".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| DaemonError::Git("Git stderr unavailable".into()))?;
    let capture = async move {
        let stdout_task = tokio::spawn(read_capped(stdout, DIFF_LIMIT));
        let stderr_task = tokio::spawn(read_bounded(stderr));
        let status = child
            .wait()
            .await
            .map_err(|error| DaemonError::Git(error.to_string()))?;
        let (mut stdout, truncated) = stdout_task
            .await
            .map_err(|error| DaemonError::Git(error.to_string()))??;
        let stderr = stderr_task
            .await
            .map_err(|error| DaemonError::Git(error.to_string()))??;
        if !status.success() {
            return Err(DaemonError::Git(format!(
                "failed to read diff: {}",
                safe_message(&stderr)
            )));
        }
        if truncated {
            stdout.extend_from_slice(b"\n[SylvOps: diff truncated at 512 KiB]\n");
        }
        let text = String::from_utf8_lossy(&stdout).into_owned();
        Ok(GitDiff {
            worktree_id: worktree.id,
            text,
            truncated,
        })
    };
    timeout(GIT_TIMEOUT, capture)
        .await
        .map_err(|_| DaemonError::Git("Git diff timed out".into()))?
}

/// Removes an exact, clean, managed worktree without force and preserves its branch.
///
/// # Errors
///
/// Returns an error for root/unmanaged worktrees, changed confirmation state, dirty contents,
/// containment or identity failures, or any Git removal failure.
pub async fn remove_managed_worktree(
    project: &Project,
    worktree: &Worktree,
    managed_root: &Path,
    confirmation_token: &str,
) -> Result<()> {
    if worktree.is_root_checkout || worktree.status != WorktreeStatus::Active {
        return Err(DaemonError::Git(
            "only active managed worktrees can be removed".into(),
        ));
    }
    let repository = canonical_registered_path(&project.canonical_repository_path).await?;
    let path = canonical_registered_path(&worktree.canonical_path).await?;
    let project_directory = managed_root.join(project.id.to_string());
    let project_directory = tokio::fs::canonicalize(&project_directory)
        .await
        .map_err(|error| {
            DaemonError::Git(format!("cannot verify project worktree root: {error}"))
        })?;
    ensure_direct_child(managed_root, &project_directory)?;
    if path.parent() != Some(project_directory.as_path()) {
        return Err(DaemonError::Git(
            "worktree is outside its managed project directory".into(),
        ));
    }
    reject_link(&path)?;
    verify_common_repository(&repository, &path).await?;
    ensure_worktree_registered(&repository, &path).await?;

    let state = inspect_worktree(worktree).await?;
    if state.branch != worktree.branch {
        return Err(DaemonError::Git(
            "worktree branch identity changed after registration".into(),
        ));
    }
    if !state.clean {
        return Err(DaemonError::Git(format!(
            "worktree is not empty of changes (tracked {}, untracked {}, ignored {})",
            state.tracked_changes, state.untracked_files, state.ignored_files
        )));
    }
    if state.removal_confirmation_token.as_deref() != Some(confirmation_token) {
        return Err(DaemonError::Git(
            "worktree state changed after confirmation".into(),
        ));
    }

    let hooks = managed_root.join(".disabled-hooks");
    let arguments = vec![
        OsString::from("worktree"),
        OsString::from("remove"),
        OsString::from("--"),
        git_path_argument(&path),
    ];
    let output = run_git_mutation(&repository, &hooks, &arguments).await?;
    if !output.success {
        return Err(DaemonError::Git(format!(
            "failed to remove worktree without force: {}",
            safe_message(&output.stderr)
        )));
    }
    if tokio::fs::symlink_metadata(&path).await.is_ok() {
        return Err(DaemonError::Git(
            "Git reported success but the worktree path still exists".into(),
        ));
    }
    Ok(())
}

async fn validate_branch_with_git(repository: &Path, branch: &str) -> Result<()> {
    let output = run_git(repository, &["check-ref-format", "--branch", branch]).await?;
    if output.success {
        Ok(())
    } else {
        Err(DaemonError::Git(format!(
            "invalid branch name: {}",
            safe_message(&output.stderr)
        )))
    }
}

fn validate_branch_name(branch: &str) -> Result<()> {
    if branch.is_empty()
        || branch.len() > 1024
        || branch.starts_with('-')
        || branch.contains("..")
        || branch.chars().any(char::is_control)
    {
        return Err(DaemonError::Git("invalid or unsafe branch name".into()));
    }
    Ok(())
}

fn validate_base_ref(base_ref: &str) -> Result<()> {
    if base_ref.is_empty()
        || base_ref.len() > 1024
        || base_ref.starts_with('-')
        || base_ref.chars().any(char::is_control)
    {
        return Err(DaemonError::Git("invalid or unsafe base ref".into()));
    }
    Ok(())
}

fn validate_display_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 200 || name.chars().any(char::is_control) {
        return Err(DaemonError::Git(
            "worktree name must contain 1 to 200 non-control characters".into(),
        ));
    }
    Ok(name.to_owned())
}

async fn canonical_registered_path(path: &str) -> Result<PathBuf> {
    let expected = PathBuf::from(path);
    let canonical = tokio::fs::canonicalize(&expected)
        .await
        .map_err(|error| DaemonError::Git(format!("registered path is unavailable: {error}")))?;
    if canonical != expected {
        return Err(DaemonError::Git(
            "registered canonical path identity changed".into(),
        ));
    }
    Ok(canonical)
}

fn ensure_direct_child(parent: &Path, child: &Path) -> Result<()> {
    if child.parent() != Some(parent) {
        return Err(DaemonError::Git(
            "managed path escaped its expected parent".into(),
        ));
    }
    Ok(())
}

async fn verify_common_repository(repository: &Path, worktree: &Path) -> Result<()> {
    let repository_common = common_git_directory(repository).await?;
    let worktree_common = common_git_directory(worktree).await?;
    if repository_common != worktree_common {
        return Err(DaemonError::Git(
            "worktree does not belong to the registered repository".into(),
        ));
    }
    Ok(())
}

async fn common_git_directory(cwd: &Path) -> Result<PathBuf> {
    let value = required_git(
        cwd,
        &["rev-parse", "--git-common-dir"],
        "resolve common Git directory",
    )
    .await?;
    let value = PathBuf::from(value);
    let value = if value.is_absolute() {
        value
    } else {
        cwd.join(value)
    };
    tokio::fs::canonicalize(value).await.map_err(|error| {
        DaemonError::Git(format!("cannot canonicalize common Git directory: {error}"))
    })
}

async fn ensure_worktree_registered(repository: &Path, worktree: &Path) -> Result<()> {
    let output = run_git(repository, &["worktree", "list", "--porcelain", "-z"]).await?;
    if !output.success {
        return Err(DaemonError::Git(format!(
            "failed to refresh Git worktree list: {}",
            safe_message(&output.stderr)
        )));
    }
    for field in output.stdout.split(|byte| *byte == 0 || *byte == b'\n') {
        let Some(path) = field.strip_prefix(b"worktree ") else {
            continue;
        };
        let Ok(path) = std::str::from_utf8(path) else {
            continue;
        };
        if tokio::fs::canonicalize(path)
            .await
            .is_ok_and(|candidate| candidate == worktree)
        {
            return Ok(());
        }
    }
    Err(DaemonError::Git(
        "managed checkout is absent from Git's worktree list".into(),
    ))
}

fn count_status_entries(bytes: &[u8]) -> (u32, u32, u32) {
    let records: Vec<_> = bytes
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .collect();
    let mut tracked = 0_u32;
    let mut untracked = 0_u32;
    let mut ignored = 0_u32;
    let mut index = 0;
    while let Some(record) = records.get(index) {
        if record.starts_with(b"??") {
            untracked = untracked.saturating_add(1);
        } else if record.starts_with(b"!!") {
            ignored = ignored.saturating_add(1);
        } else {
            tracked = tracked.saturating_add(1);
            if record
                .get(..2)
                .is_some_and(|status| status.contains(&b'R') || status.contains(&b'C'))
            {
                index = index.saturating_add(1);
            }
        }
        index = index.saturating_add(1);
    }
    (tracked, untracked, ignored)
}

fn state_token(
    id: WorktreeId,
    path: &str,
    head: &str,
    branch: Option<&str>,
    status: &[u8],
) -> String {
    let mut digest = Sha256::new();
    digest.update(id.to_string().as_bytes());
    digest.update([0_u8]);
    digest.update(path.as_bytes());
    digest.update([0_u8]);
    digest.update(head.as_bytes());
    digest.update([0_u8]);
    digest.update(branch.unwrap_or_default().as_bytes());
    digest.update([0_u8]);
    digest.update(status);
    let digest = digest.finalize();
    hex_digest(&digest)
}

async fn required_git(cwd: &Path, arguments: &[&str], operation: &str) -> Result<String> {
    let output = run_git(cwd, arguments).await?;
    if !output.success {
        return Err(DaemonError::Git(format!(
            "failed to {operation}: {}",
            safe_message(&output.stderr)
        )));
    }
    Ok(output_text(&output.stdout)?.trim().to_owned())
}

async fn optional_git(cwd: &Path, arguments: &[&str]) -> Result<Option<String>> {
    let output = run_git(cwd, arguments).await?;
    Ok(output
        .success
        .then(|| output_text(&output.stdout).map(|value| value.trim().to_owned()))
        .transpose()?
        .filter(|value| !value.is_empty()))
}

async fn run_git(cwd: &Path, arguments: &[&str]) -> Result<GitOutput> {
    let arguments: Vec<_> = arguments
        .iter()
        .map(|argument| OsString::from(*argument))
        .collect();
    run_git_os(cwd, &arguments).await
}

async fn run_git_mutation(cwd: &Path, hooks: &Path, arguments: &[OsString]) -> Result<GitOutput> {
    let mut hooks_configuration = OsString::from("core.hooksPath=");
    hooks_configuration.push(git_path_argument(hooks));
    let mut configured = vec![OsString::from("-c"), hooks_configuration];
    configured.extend_from_slice(arguments);
    run_git_os(cwd, &configured).await
}

#[cfg(not(windows))]
fn git_path_argument(path: &Path) -> OsString {
    path.as_os_str().to_owned()
}

#[cfg(windows)]
fn git_path_argument(path: &Path) -> OsString {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    const VERBATIM_PREFIX: &[u16] = &[92, 92, 63, 92];
    const VERBATIM_UNC_PREFIX: &[u16] = &[92, 92, 63, 92, 85, 78, 67, 92];

    let encoded: Vec<_> = path.as_os_str().encode_wide().collect();
    if let Some(remainder) = encoded.strip_prefix(VERBATIM_UNC_PREFIX) {
        let mut ordinary_unc = vec![92, 92];
        ordinary_unc.extend_from_slice(remainder);
        OsString::from_wide(&ordinary_unc)
    } else if let Some(remainder) = encoded.strip_prefix(VERBATIM_PREFIX) {
        OsString::from_wide(remainder)
    } else {
        path.as_os_str().to_owned()
    }
}

async fn run_git_os(cwd: &Path, arguments: &[OsString]) -> Result<GitOutput> {
    let mut child = background_process::command("git");
    child
        .arg("-C")
        .arg(cwd)
        .args(arguments)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = child
        .spawn()
        .map_err(|error| DaemonError::Git(format!("failed to launch Git: {error}")))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| DaemonError::Git("Git stdout unavailable".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| DaemonError::Git("Git stderr unavailable".into()))?;
    let capture = async move {
        let stdout_task = tokio::spawn(read_bounded(stdout));
        let stderr_task = tokio::spawn(read_bounded(stderr));
        let status = child
            .wait()
            .await
            .map_err(|error| DaemonError::Git(error.to_string()))?;
        let stdout = stdout_task
            .await
            .map_err(|error| DaemonError::Git(error.to_string()))??;
        let stderr = stderr_task
            .await
            .map_err(|error| DaemonError::Git(error.to_string()))??;
        Ok::<_, DaemonError>(GitOutput {
            success: status.success(),
            stdout,
            stderr,
        })
    };
    timeout(GIT_TIMEOUT, capture)
        .await
        .map_err(|_| DaemonError::Git("Git command timed out".into()))?
}

async fn read_bounded<R: tokio::io::AsyncRead + Unpin + Send + 'static>(
    reader: R,
) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take(OUTPUT_LIMIT + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| DaemonError::Git(error.to_string()))?;
    if bytes.len() as u64 > OUTPUT_LIMIT {
        return Err(DaemonError::Git("Git output exceeded 64 KiB".into()));
    }
    Ok(bytes)
}

async fn read_capped<R: tokio::io::AsyncRead + Unpin + Send + 'static>(
    reader: R,
    limit: u64,
) -> Result<(Vec<u8>, bool)> {
    let mut bytes = Vec::new();
    reader
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| DaemonError::Git(error.to_string()))?;
    let truncated = bytes.len() as u64 > limit;
    if truncated {
        bytes.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    }
    Ok((bytes, truncated))
}

fn output_text(bytes: &[u8]) -> Result<&str> {
    std::str::from_utf8(bytes)
        .map_err(|_| DaemonError::Git("Git returned non-UTF-8 metadata".into()))
}

fn path_text(path: &Path) -> Result<String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| DaemonError::Git("repository path is not valid Unicode".into()))
}

fn safe_message(message: &[u8]) -> String {
    String::from_utf8_lossy(message)
        .trim()
        .chars()
        .take(512)
        .collect()
}

fn hex_digest(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut encoded, "{byte:02x}").expect("writing to a string cannot fail");
    }
    encoded
}

fn sanitize_remote_url(remote: &str) -> String {
    let remote = remote.split(['?', '#']).next().unwrap_or_default().trim();
    let Some(scheme_end) = remote.find("://") else {
        let remote = remote.rsplit_once('@').map_or(remote, |(_, host)| host);
        return remote.chars().take(4096).collect();
    };
    let authority_start = scheme_end + 3;
    let authority_end = remote[authority_start..]
        .find('/')
        .map_or(remote.len(), |offset| authority_start + offset);
    let authority = &remote[authority_start..authority_end];
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    format!(
        "{}{}{}",
        &remote[..authority_start],
        host,
        &remote[authority_end..]
    )
    .chars()
    .take(4096)
    .collect()
}

#[cfg(unix)]
fn reject_link(path: &Path) -> Result<()> {
    if fs::symlink_metadata(path)
        .map_err(|error| DaemonError::Git(error.to_string()))?
        .file_type()
        .is_symlink()
    {
        return Err(DaemonError::Git(format!(
            "managed path is a symbolic link: {}",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(windows)]
fn reject_link(path: &Path) -> Result<()> {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    let attributes = fs::metadata(path)
        .map_err(|error| DaemonError::Git(error.to_string()))?
        .file_attributes();
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(DaemonError::Git(format!(
            "managed path is a reparse point: {}",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::process::Command as ProcessCommand;

    use sylvops_core::ids::ProjectId;

    use super::*;

    #[tokio::test]
    async fn registers_unicode_repository_without_an_origin() {
        let directory = tempfile::tempdir().unwrap();
        let repository = directory.path().join("répository-测试");
        initialize_repository(&repository);

        let registration = inspect_repository(WorkspaceId::new(), &repository)
            .await
            .unwrap();
        assert_eq!(
            PathBuf::from(registration.canonical_repository_path.as_str()),
            std::fs::canonicalize(&repository).unwrap()
        );
        assert!(registration.branch.is_some());
        assert!(registration.remote_url.is_none());
        assert!(!registration.base_commit.is_empty());
    }

    #[tokio::test]
    async fn creates_inspects_and_removes_a_managed_worktree() {
        let directory = tempfile::tempdir().unwrap();
        let repository = directory.path().join("repository");
        initialize_repository(&repository);
        let repository = std::fs::canonicalize(repository).unwrap();
        let managed = prepare_managed_root(&directory.path().join("managed"))
            .await
            .unwrap();
        let project = test_project(&repository);
        let id = WorktreeId::new();
        let created = create_managed_worktree(
            &project,
            &managed,
            id,
            Some("Feature".into()),
            "feature/managed",
            Some("HEAD"),
        )
        .await
        .unwrap();
        let worktree = test_worktree(&created);
        let state = inspect_worktree(&worktree).await.unwrap();
        assert!(state.clean);
        remove_managed_worktree(
            &project,
            &worktree,
            &managed,
            state.removal_confirmation_token.as_deref().unwrap(),
        )
        .await
        .unwrap();
        assert!(!Path::new(&created.canonical_path).exists());
        assert!(
            run_git(
                &repository,
                &["show-ref", "--verify", "refs/heads/feature/managed"]
            )
            .await
            .unwrap()
            .success
        );
    }

    #[tokio::test]
    async fn dirty_untracked_and_ignored_files_refuse_removal() {
        let directory = tempfile::tempdir().unwrap();
        let repository = directory.path().join("repository");
        initialize_repository(&repository);
        std::fs::write(repository.join(".gitignore"), "ignored.txt\n").unwrap();
        run_test_git(&repository, &["add", ".gitignore"]);
        commit(&repository, "ignore file");
        let repository = std::fs::canonicalize(repository).unwrap();
        let managed = prepare_managed_root(&directory.path().join("managed"))
            .await
            .unwrap();
        let project = test_project(&repository);
        let created = create_managed_worktree(
            &project,
            &managed,
            WorktreeId::new(),
            None,
            "feature/dirty",
            None,
        )
        .await
        .unwrap();
        let worktree = test_worktree(&created);
        std::fs::write(
            Path::new(&created.canonical_path).join("untracked.txt"),
            "new",
        )
        .unwrap();
        std::fs::write(
            Path::new(&created.canonical_path).join("ignored.txt"),
            "ignored",
        )
        .unwrap();
        let state = inspect_worktree(&worktree).await.unwrap();
        assert!(!state.clean);
        assert_eq!(state.untracked_files, 1);
        assert_eq!(state.ignored_files, 1);
        assert!(state.removal_confirmation_token.is_none());
        assert!(
            remove_managed_worktree(&project, &worktree, &managed, "invalid")
                .await
                .is_err()
        );
        assert!(Path::new(&created.canonical_path).exists());
    }

    #[tokio::test]
    async fn stale_clean_state_token_refuses_removal() {
        let directory = tempfile::tempdir().unwrap();
        let repository = directory.path().join("repository");
        initialize_repository(&repository);
        let repository = std::fs::canonicalize(repository).unwrap();
        let managed = prepare_managed_root(&directory.path().join("managed"))
            .await
            .unwrap();
        let project = test_project(&repository);
        let created = create_managed_worktree(
            &project,
            &managed,
            WorktreeId::new(),
            None,
            "feature/token",
            None,
        )
        .await
        .unwrap();
        let worktree = test_worktree(&created);
        let old_state = inspect_worktree(&worktree).await.unwrap();

        let checkout = Path::new(&created.canonical_path);
        run_test_git(checkout, &["checkout", "-b", "feature/replacement"]);
        let replaced_branch_state = inspect_worktree(&worktree).await.unwrap();
        assert!(
            remove_managed_worktree(
                &project,
                &worktree,
                &managed,
                replaced_branch_state
                    .removal_confirmation_token
                    .as_deref()
                    .unwrap(),
            )
            .await
            .is_err()
        );
        run_test_git(checkout, &["checkout", "feature/token"]);

        std::fs::write(checkout.join("next.txt"), "next\n").unwrap();
        run_test_git(checkout, &["add", "next.txt"]);
        commit(checkout, "advance HEAD");

        assert!(
            remove_managed_worktree(
                &project,
                &worktree,
                &managed,
                old_state.removal_confirmation_token.as_deref().unwrap(),
            )
            .await
            .is_err()
        );
        assert!(checkout.exists());
        let new_state = inspect_worktree(&worktree).await.unwrap();
        remove_managed_worktree(
            &project,
            &worktree,
            &managed,
            new_state.removal_confirmation_token.as_deref().unwrap(),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn existing_destination_is_never_overwritten() {
        let directory = tempfile::tempdir().unwrap();
        let repository = directory.path().join("repository");
        initialize_repository(&repository);
        let repository = std::fs::canonicalize(repository).unwrap();
        let managed = prepare_managed_root(&directory.path().join("managed"))
            .await
            .unwrap();
        let project = test_project(&repository);
        let id = WorktreeId::new();
        let destination = managed.join(project.id.to_string()).join(id.to_string());
        std::fs::create_dir_all(&destination).unwrap();
        let sentinel = destination.join("preserve.txt");
        std::fs::write(&sentinel, "preserve\n").unwrap();

        assert!(
            create_managed_worktree(&project, &managed, id, None, "feature/collision", None,)
                .await
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(sentinel).unwrap(), "preserve\n");
        assert!(
            !run_git(
                &repository,
                &["show-ref", "--verify", "refs/heads/feature/collision"]
            )
            .await
            .unwrap()
            .success
        );
    }

    #[tokio::test]
    async fn detached_head_is_registered_without_inventing_a_branch() {
        let directory = tempfile::tempdir().unwrap();
        let repository = directory.path().join("repository");
        initialize_repository(&repository);
        run_test_git(&repository, &["checkout", "--detach", "HEAD"]);

        let registration = inspect_repository(WorkspaceId::new(), &repository)
            .await
            .unwrap();
        assert!(registration.branch.is_none());
        assert!(registration.default_branch.is_none());
    }

    #[tokio::test]
    async fn rejects_duplicate_or_unsafe_branches() {
        let directory = tempfile::tempdir().unwrap();
        let repository = directory.path().join("repository");
        initialize_repository(&repository);
        let repository = std::fs::canonicalize(repository).unwrap();
        let managed = prepare_managed_root(&directory.path().join("managed"))
            .await
            .unwrap();
        let project = test_project(&repository);
        assert!(
            create_managed_worktree(
                &project,
                &managed,
                WorktreeId::new(),
                None,
                "../escape",
                None,
            )
            .await
            .is_err()
        );
        let branch = required_git(&repository, &["branch", "--show-current"], "read branch")
            .await
            .unwrap();
        assert!(
            create_managed_worktree(&project, &managed, WorktreeId::new(), None, &branch, None,)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn rejects_a_non_repository() {
        let directory = tempfile::tempdir().unwrap();
        assert!(
            inspect_repository(WorkspaceId::new(), directory.path())
                .await
                .is_err()
        );
    }

    #[test]
    fn remote_urls_are_sanitized_before_persistence() {
        assert_eq!(
            sanitize_remote_url("https://user:secret@example.com/org/repo?token=secret#fragment"),
            "https://example.com/org/repo"
        );
        assert_eq!(
            sanitize_remote_url("git@github.com:owner/repository.git"),
            "github.com:owner/repository.git"
        );
    }

    fn test_project(repository: &Path) -> Project {
        Project {
            id: ProjectId::new(),
            workspace_id: WorkspaceId::new(),
            name: "repository".into(),
            repository_path: path_text(repository).unwrap(),
            canonical_repository_path: path_text(repository).unwrap(),
            default_branch: None,
            remote_url: None,
            created_at: 0,
            last_activity_at: 0,
        }
    }

    fn test_worktree(created: &NewManagedWorktree) -> Worktree {
        Worktree {
            id: created.id,
            project_id: created.project_id,
            name: created.name.clone(),
            path: created.path.clone(),
            canonical_path: created.canonical_path.clone(),
            branch: Some(created.branch.clone()),
            base_ref: created.base_ref.clone(),
            base_commit: created.base_commit.clone(),
            is_root_checkout: false,
            status: WorktreeStatus::Active,
            created_at: 0,
            last_activity_at: 0,
            removed_at: None,
        }
    }

    fn initialize_repository(path: &Path) {
        std::fs::create_dir(path).unwrap();
        run_test_git(path, &["init"]);
        std::fs::write(path.join("README.md"), "test\n").unwrap();
        run_test_git(path, &["add", "README.md"]);
        commit(path, "initial");
    }

    fn commit(path: &Path, message: &str) {
        run_test_git(
            path,
            &[
                "-c",
                "user.name=SylvOps Test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "-m",
                message,
            ],
        );
    }

    fn run_test_git(path: &Path, arguments: &[&str]) {
        let status = ProcessCommand::new("git")
            .arg("-C")
            .arg(path)
            .args(arguments)
            .status()
            .unwrap();
        assert!(status.success());
    }
}
