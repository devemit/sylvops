//! Read-only per-file review. All paths originate in a fresh bounded Git status read.

use super::{
    AsyncReadExt, DaemonError, GIT_TIMEOUT, OsString, Path, PathBuf, Result, Stdio, Worktree,
    WorktreeId, WorktreeStatus, background_process, canonical_registered_path, fs, reject_link,
    timeout, verify_repository_root,
};
use sylvops_core::changes::{
    ChangeGroup, ChangeKey, ChangeStatus, ChangedFile, CheckoutChanges, FileChange,
    MAX_CHANGE_FILES, MAX_FILE_BYTES,
};

const STATUS_LIMIT: usize = 4 * 1024 * 1024;

async fn review_root(worktree: &Worktree) -> Result<PathBuf> {
    if worktree.status != WorktreeStatus::Active {
        return Err(DaemonError::Git("checkout is not active".into()));
    }
    let path = canonical_registered_path(&worktree.canonical_path).await?;
    verify_repository_root(&path).await?;
    Ok(path)
}

/// Lists local changes, including untracked files, without refreshing the index on disk.
///
/// # Errors
/// Returns an error for invalid checkouts, malformed status, or bounded Git failures.
pub async fn list_checkout_changes(worktree: &Worktree) -> Result<CheckoutChanges> {
    let path = review_root(worktree).await?;
    let (bytes, truncated) = capture(
        &path,
        &[
            "status".into(),
            "--porcelain=v1".into(),
            "-z".into(),
            "--untracked-files=all".into(),
            "--ignore-submodules=none".into(),
        ],
        STATUS_LIMIT,
    )
    .await?;
    parse_status(worktree.id, &bytes, truncated)
}

fn parse_status(id: WorktreeId, bytes: &[u8], truncated: bool) -> Result<CheckoutChanges> {
    if !bytes.is_empty() && !bytes.ends_with(&[0]) && !truncated {
        return Err(DaemonError::Git("incomplete Git status".into()));
    }
    let mut result = CheckoutChanges {
        worktree_id: id,
        files: Vec::new(),
        truncated,
    };
    let mut records = bytes.split(|b| *b == 0);
    let mut budget = 96 * 1024_usize;
    while let Some(record) = records.next() {
        if record.is_empty() {
            continue;
        }
        if record.len() < 4 || record[2] != b' ' {
            return Err(DaemonError::Git("malformed Git status".into()));
        }
        let path = &record[3..];
        let rename = record[..2].iter().any(|b| matches!(b, b'R' | b'C'));
        let old_path = if rename {
            Some(
                records
                    .next()
                    .ok_or_else(|| DaemonError::Git("incomplete Git rename".into()))?
                    .to_vec(),
            )
        } else {
            None
        };
        if !bytes.ends_with(&[0]) && truncated && records.clone().next().is_none() {
            break;
        }
        let conflict = record[..2].contains(&b'U') || matches!(&record[..2], b"AA" | b"DD");
        for (group, code) in if &record[..2] == b"??" {
            vec![(ChangeGroup::Untracked, b'?')]
        } else if conflict {
            vec![(ChangeGroup::Unstaged, b'U')]
        } else {
            vec![
                (ChangeGroup::Unstaged, record[1]),
                (ChangeGroup::Staged, record[0]),
            ]
        } {
            if code == b' ' {
                continue;
            }
            let status = match code {
                b'M' => ChangeStatus::Modified,
                b'A' => ChangeStatus::Added,
                b'D' => ChangeStatus::Deleted,
                b'R' => ChangeStatus::Renamed,
                b'C' => ChangeStatus::Copied,
                b'T' => ChangeStatus::TypeChanged,
                b'U' => ChangeStatus::Conflicted,
                b'?' => ChangeStatus::Untracked,
                _ => return Err(DaemonError::Git("unknown Git status".into())),
            };
            let size = path.len() + old_path.as_ref().map_or(0, Vec::len);
            if result.files.len() == MAX_CHANGE_FILES || size > budget {
                result.truncated = true;
                continue;
            }
            let key = ChangeKey {
                group,
                path: path.to_vec(),
            };
            if !key.valid() {
                result.truncated = true;
                continue;
            }
            budget -= size;
            result.files.push(ChangedFile {
                key,
                status,
                old_path: if matches!(code, b'R' | b'C') {
                    old_path.clone()
                } else {
                    None
                },
            });
        }
    }
    result.files.sort_by(|a, b| {
        a.key
            .group
            .cmp(&b.key.group)
            .then_with(|| a.key.path.cmp(&b.key.path))
    });
    Ok(result)
}

/// Loads one current group/file comparison; arbitrary filesystem reads are refused.
///
/// # Errors
/// Returns an error for invalid identities, changed files, unsafe paths, or Git failures.
pub async fn load_file_change(worktree: &Worktree, key: &ChangeKey) -> Result<FileChange> {
    if !key.valid() {
        return Err(DaemonError::Git("invalid change path".into()));
    }
    let path = review_root(worktree).await?;
    let listed = list_checkout_changes(worktree).await?;
    let entry = listed
        .files
        .iter()
        .find(|entry| &entry.key == key)
        .ok_or_else(|| {
            DaemonError::Git("file changed or is no longer listed; refresh Changes".into())
        })?;
    let relative = relative_path(&key.path)?;
    let mut current = path.clone();
    for part in relative.components() {
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if reject_link(&current).is_err() || (!metadata.is_file() && !metadata.is_dir()) {
                    return Ok(FileChange::metadata(
                        worktree.id,
                        key.clone(),
                        "Symlink or special file: text preview unavailable.",
                    ));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(_) => return Err(DaemonError::Git("cannot inspect change path".into())),
        }
    }
    if current.is_dir() {
        return Ok(FileChange::metadata(
            worktree.id,
            key.clone(),
            "Submodule or directory: text preview unavailable.",
        ));
    }
    if entry.status == ChangeStatus::Conflicted {
        return Ok(FileChange::metadata(
            worktree.id,
            key.clone(),
            "Unresolved merge conflict. Resolve it in your editor before reviewing a regular diff.",
        ));
    }
    if key.group == ChangeGroup::Untracked {
        return untracked_file(&path, &relative, worktree.id, key.clone()).await;
    }
    let mut arguments: Vec<OsString> = [
        "--literal-pathspecs",
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--no-color",
        "--find-renames=50%",
        "--unified=3",
    ]
    .into_iter()
    .map(Into::into)
    .collect();
    if key.group == ChangeGroup::Staged {
        arguments.push("--cached".into());
    }
    arguments.push("--".into());
    arguments.push(relative.into_os_string());
    if let Some(old) = &entry.old_path {
        arguments.push(relative_path(old)?.into_os_string());
    }
    let (bytes, truncated) = capture(&path, &arguments, MAX_FILE_BYTES).await?;
    if bytes.contains(&0) || std::str::from_utf8(&bytes).is_err() && !truncated {
        return Ok(FileChange::metadata(
            worktree.id,
            key.clone(),
            "Binary or unsupported text encoding: preview unavailable.",
        ));
    }
    let diff_text = String::from_utf8_lossy(&bytes);
    if diff_text.lines().any(metadata_only) {
        return Ok(FileChange::metadata(
            worktree.id,
            key.clone(),
            "Binary, symlink, or submodule change: text preview unavailable.",
        ));
    }
    Ok(FileChange::from_patch(
        worktree.id,
        key.clone(),
        &diff_text,
        truncated,
    ))
}

fn metadata_only(line: &str) -> bool {
    line.starts_with("Binary files ")
        || ([
            "old mode ",
            "new mode ",
            "new file mode ",
            "deleted file mode ",
            "index ",
        ]
        .iter()
        .any(|prefix| line.starts_with(prefix))
            && (line.ends_with("120000") || line.ends_with("160000")))
}

fn relative_path(bytes: &[u8]) -> Result<PathBuf> {
    let key = ChangeKey {
        group: ChangeGroup::Untracked,
        path: bytes.to_vec(),
    };
    if !key.valid() {
        return Err(DaemonError::Git("invalid relative path".into()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Ok(OsString::from_vec(bytes.to_vec()).into())
    }
    #[cfg(not(unix))]
    {
        Ok(PathBuf::from(std::str::from_utf8(bytes).map_err(|_| {
            DaemonError::Git("unsupported path encoding".into())
        })?))
    }
}

async fn untracked_file(
    root: &Path,
    relative: &Path,
    id: WorktreeId,
    key: ChangeKey,
) -> Result<FileChange> {
    let root = root.to_owned();
    let relative = relative.to_owned();
    let bytes = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<u8>> {
        use std::io::Read;
        let dir = cap_std::fs::Dir::open_ambient_dir(root, cap_std::ambient_authority())?;
        let mut options = cap_std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt;
            options.custom_flags(nix::libc::O_NONBLOCK | nix::libc::O_NOFOLLOW);
        }
        #[cfg(windows)]
        {
            use cap_std::fs::OpenOptionsExt;
            options.custom_flags(
                windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT,
            );
        }
        let file = dir.open_with(relative, &options)?;
        if !file.metadata()?.is_file() {
            return Err(std::io::Error::other("not a regular file"));
        }
        let mut bytes = Vec::new();
        file.take((MAX_FILE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        Ok(bytes)
    })
    .await
    .map_err(|_| DaemonError::Git("file reader failed".into()))?
    .map_err(|_| DaemonError::Git("file unavailable; refresh Changes".into()))?;
    let truncated = bytes.len() > MAX_FILE_BYTES;
    let bytes = &bytes[..bytes.len().min(MAX_FILE_BYTES)];
    if bytes.contains(&0) || std::str::from_utf8(bytes).is_err() && !truncated {
        return Ok(FileChange::metadata(
            id,
            key,
            "Binary or unsupported text encoding: preview unavailable.",
        ));
    }
    let text = String::from_utf8_lossy(bytes);
    let mut patch = format!("@@ -0,0 +1,{} @@\n", text.lines().count());
    for line in text.split_terminator('\n') {
        patch.push('+');
        patch.push_str(line);
        patch.push('\n');
    }
    if !text.ends_with('\n') && !text.is_empty() {
        patch.push_str("\\ No newline at end of file\n");
    }
    Ok(FileChange::from_patch(id, key, &patch, truncated))
}

async fn capture(root: &Path, arguments: &[OsString], limit: usize) -> Result<(Vec<u8>, bool)> {
    let mut command = background_process::command("git");
    for variable in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_CONFIG_PARAMETERS",
    ] {
        command.env_remove(variable);
    }
    command
        .env("GIT_CONFIG_COUNT", "3")
        .env("GIT_CONFIG_KEY_0", "core.fsmonitor")
        .env("GIT_CONFIG_VALUE_0", "false")
        .env("GIT_CONFIG_KEY_1", "core.untrackedCache")
        .env("GIT_CONFIG_VALUE_1", "false")
        .env("GIT_CONFIG_KEY_2", "status.renames")
        .env("GIT_CONFIG_VALUE_2", "true");
    command
        .arg("-C")
        .arg(root)
        .args(arguments)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|_| DaemonError::Git("cannot launch Git review".into()))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| DaemonError::Git("Git output unavailable".into()))?;
    timeout(GIT_TIMEOUT, async move {
        let mut output = Vec::new();
        let mut buffer = [0; 8192];
        let mut truncated = false;
        loop {
            let count = stdout
                .read(&mut buffer)
                .await
                .map_err(|_| DaemonError::Git("cannot read Git review".into()))?;
            if count == 0 {
                break;
            }
            let retained = count.min(limit.saturating_sub(output.len()));
            output.extend_from_slice(&buffer[..retained]);
            truncated |= retained < count;
        }
        if !child
            .wait()
            .await
            .map_err(|_| DaemonError::Git("Git review failed".into()))?
            .success()
        {
            return Err(DaemonError::Git(
                "cannot read checkout changes; refresh or check repository health".into(),
            ));
        }
        Ok((output, truncated))
    })
    .await
    .map_err(|_| DaemonError::Git("Git review timed out".into()))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::run_git;
    use sylvops_test_support::TemporaryRepository;

    fn checkout(path: &Path) -> Worktree {
        let canonical = fs::canonicalize(path).unwrap().to_str().unwrap().to_owned();
        Worktree {
            id: WorktreeId::new(),
            project_id: sylvops_core::ids::ProjectId::new(),
            name: "Review fixture".into(),
            path: canonical.clone(),
            canonical_path: canonical,
            branch: None,
            base_ref: "HEAD".into(),
            base_commit: String::new(),
            is_root_checkout: true,
            status: WorktreeStatus::Active,
            created_at: 1,
            last_activity_at: 1,
            removed_at: None,
        }
    }

    #[tokio::test]
    async fn review_distinguishes_staged_unstaged_and_untracked_without_writes() {
        let repo = TemporaryRepository::initialize().await.unwrap();
        fs::write(repo.path().join("README.md"), "staged\n").unwrap();
        assert!(
            run_git(repo.path(), &["add", "README.md"])
                .await
                .unwrap()
                .success
        );
        fs::write(repo.path().join("README.md"), "unstaged\n").unwrap();
        fs::write(repo.path().join("-new file.json"), "{\"new\":true}\n").unwrap();
        fs::write(repo.path().join("binary.bin"), [0, 1, 2]).unwrap();
        let index_before = fs::read(repo.path().join(".git/index")).unwrap();
        let head_before = fs::read(repo.path().join(".git/HEAD")).unwrap();
        let worktree = checkout(repo.path());
        let list = list_checkout_changes(&worktree).await.unwrap();
        assert_eq!(list.files.len(), 4);
        for entry in list.files {
            let file = load_file_change(&worktree, &entry.key).await.unwrap();
            let text = file
                .lines
                .iter()
                .map(|l| l.text.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            match entry.key.group {
                ChangeGroup::Staged => {
                    assert!(text.contains("+staged"));
                    assert!(!text.contains("+unstaged"));
                }
                ChangeGroup::Unstaged => {
                    assert!(text.contains("-staged"));
                    assert!(text.contains("+unstaged"));
                }
                ChangeGroup::Untracked if entry.key.path == b"binary.bin" => {
                    assert!(file.notice.is_some());
                }
                ChangeGroup::Untracked => assert!(text.contains("+{\"new\":true}")),
            }
        }
        assert_eq!(
            fs::read(repo.path().join(".git/index")).unwrap(),
            index_before
        );
        assert_eq!(
            fs::read(repo.path().join(".git/HEAD")).unwrap(),
            head_before
        );
        assert_eq!(
            fs::read_to_string(repo.path().join("README.md")).unwrap(),
            "unstaged\n"
        );
    }

    #[tokio::test]
    async fn unborn_checkout_lists_new_files_and_staged_additions() {
        let temp = tempfile::tempdir().unwrap();
        assert!(run_git(temp.path(), &["init"]).await.unwrap().success);
        fs::write(temp.path().join("staged.rs"), "fn main() {}\n").unwrap();
        fs::write(temp.path().join("untracked.rs"), "// untracked\n").unwrap();
        assert!(
            run_git(temp.path(), &["add", "staged.rs"])
                .await
                .unwrap()
                .success
        );
        let worktree = checkout(temp.path());
        let list = list_checkout_changes(&worktree).await.unwrap();
        assert_eq!(list.files.len(), 2);
        for entry in list.files {
            assert!(
                load_file_change(&worktree, &entry.key)
                    .await
                    .unwrap()
                    .additions
                    > 0
            );
        }
        let key = ChangeKey {
            group: ChangeGroup::Untracked,
            path: b"../outside".to_vec(),
        };
        assert!(load_file_change(&worktree, &key).await.is_err());
    }

    #[tokio::test]
    async fn oversized_file_does_not_hide_other_files_or_exceed_ipc_frame() {
        let repo = TemporaryRepository::initialize().await.unwrap();
        fs::write(repo.path().join("huge.txt"), "line\n".repeat(100_000)).unwrap();
        fs::write(repo.path().join("small.txt"), "small\n").unwrap();
        let worktree = checkout(repo.path());
        let list = list_checkout_changes(&worktree).await.unwrap();
        assert_eq!(list.files.len(), 2);
        let huge = list
            .files
            .iter()
            .find(|f| f.key.path == b"huge.txt")
            .unwrap();
        let file = load_file_change(&worktree, &huge.key).await.unwrap();
        assert!(file.truncated);
        assert!(file.lines.len() <= sylvops_core::changes::MAX_DIFF_LINES);
        let frame = sylvops_core::protocol::Frame::message(
            sylvops_core::protocol::MessageClass::Response,
            10,
            &sylvops_core::protocol::DaemonResponse::FileChange(file),
        )
        .unwrap();
        assert!(frame.encode().unwrap().len() < sylvops_core::protocol::MAX_FRAME_SIZE);
    }

    #[test]
    fn status_keeps_partial_staging_untracked_and_renames_separate() {
        let id = WorktreeId::new();
        let list = parse_status(
            id,
            b"MM a.rs\0?? new file\0R  renamed\0old\0UU conflict\0",
            false,
        )
        .unwrap();
        assert_eq!(list.files.len(), 5);
        assert_eq!(
            list.files.iter().filter(|f| f.key.path == b"a.rs").count(),
            2
        );
        assert!(
            list.files
                .iter()
                .any(|f| f.old_path.as_deref() == Some(b"old"))
        );
        assert!(
            list.files
                .iter()
                .any(|f| f.key.group == ChangeGroup::Untracked)
        );
    }
    #[test]
    fn status_is_bounded_and_malformed_data_cannot_look_clean() {
        assert!(parse_status(WorktreeId::new(), b"invalid\0", false).is_err());
        let data = b"?? file\0".repeat(MAX_CHANGE_FILES + 1);
        let list = parse_status(WorktreeId::new(), &data, false).unwrap();
        assert!(list.truncated);
        assert_eq!(list.files.len(), MAX_CHANGE_FILES);
    }

    #[tokio::test]
    async fn renames_and_deletions_keep_identity_and_source_text_is_not_metadata() {
        let repo = TemporaryRepository::initialize().await.unwrap();
        assert!(
            run_git(repo.path(), &["mv", "README.md", "renamed file.md"])
                .await
                .unwrap()
                .success
        );
        let worktree = checkout(repo.path());
        let list = list_checkout_changes(&worktree).await.unwrap();
        assert_eq!(list.files.len(), 1);
        assert_eq!(list.files[0].status, ChangeStatus::Renamed);
        assert_eq!(
            list.files[0].old_path.as_deref(),
            Some(b"README.md".as_slice())
        );
        let file = load_file_change(&worktree, &list.files[0].key)
            .await
            .unwrap();
        assert!(file.notice.is_none());
        assert_eq!((file.additions, file.deletions), (0, 0));
        fs::write(
            repo.path().join("renamed file.md"),
            "new file mode 120000\nindex 160000\n",
        )
        .unwrap();
        let key = ChangeKey {
            group: ChangeGroup::Unstaged,
            path: b"renamed file.md".to_vec(),
        };
        let file = load_file_change(&worktree, &key).await.unwrap();
        assert!(file.notice.is_none());
        assert_eq!(file.additions, 2);
        fs::remove_file(repo.path().join("renamed file.md")).unwrap();
        let file = load_file_change(&worktree, &key).await.unwrap();
        assert!(file.deletions > 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_and_fifo_never_read_targets_or_block_the_reader() {
        use std::os::unix::fs::symlink;
        use std::time::Duration;
        let repo = TemporaryRepository::initialize().await.unwrap();
        let outside = repo.root().join("outside.txt");
        fs::write(&outside, "outside secret fixture").unwrap();
        symlink(&outside, repo.path().join("link")).unwrap();
        let worktree = checkout(repo.path());
        let key = ChangeKey {
            group: ChangeGroup::Untracked,
            path: b"link".to_vec(),
        };
        let file = load_file_change(&worktree, &key).await.unwrap();
        assert!(file.notice.is_some());
        assert!(file.lines.is_empty());
        let fifo = repo.path().join("pipe");
        nix::unistd::mkfifo(
            &fifo,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .unwrap();
        let key = ChangeKey {
            path: b"pipe".to_vec(),
            ..key
        };
        let result = timeout(Duration::from_secs(2), load_file_change(&worktree, &key))
            .await
            .unwrap();
        assert!(
            result.is_err()
                || result.is_ok_and(|file| file.notice.is_some() && file.lines.is_empty())
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn junction_cannot_expose_files_outside_the_checkout() {
        let repo = TemporaryRepository::initialize().await.unwrap();
        let outside = repo.root().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("secret.txt"), "outside secret fixture").unwrap();
        junction::create(&outside, repo.path().join("linked")).unwrap();
        let worktree = checkout(repo.path());
        let key = ChangeKey {
            group: ChangeGroup::Untracked,
            path: b"linked/secret.txt".to_vec(),
        };
        let result = load_file_change(&worktree, &key).await;
        assert!(result.is_err() || result.is_ok_and(|f| f.lines.is_empty() && f.notice.is_some()));
    }
}
