//! Deterministic fixtures shared by `SylvOps` integration tests.

use std::{
    future::Future,
    io,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

#[derive(Debug)]
pub struct TemporaryRepository {
    directory: tempfile::TempDir,
    repository: PathBuf,
}

impl TemporaryRepository {
    /// Creates a committed temporary Git repository using bounded structured commands.
    ///
    /// # Errors
    ///
    /// Returns an error when the temporary directory, fixture file, or a bounded Git command fails.
    pub async fn initialize() -> io::Result<Self> {
        let directory = tempfile::tempdir()?;
        let repository = directory.path().join("repository");
        tokio::fs::create_dir(&repository).await?;
        run_git(&repository, &["init"]).await?;
        tokio::fs::write(repository.join("README.md"), "test\n").await?;
        run_git(&repository, &["add", "README.md"]).await?;
        run_git(
            &repository,
            &[
                "-c",
                "user.name=SylvOps Test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "-m",
                "initial",
            ],
        )
        .await?;
        Ok(Self {
            directory,
            repository,
        })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        self.directory.path()
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.repository
    }
}

async fn run_git(path: &Path, arguments: &[&str]) -> io::Result<()> {
    let mut command = tokio::process::Command::new("git");
    command
        .arg("-C")
        .arg(path)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let status = bounded(command.status())
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "Git fixture command timed out"))??;
    if !status.success() {
        return Err(io::Error::other("Git fixture command failed"));
    }
    Ok(())
}

/// Apply a uniform timeout to process and IPC tests.
///
/// # Errors
///
/// Returns an elapsed error if `future` does not finish within ten seconds.
pub async fn bounded<T>(future: impl Future<Output = T>) -> Result<T, tokio::time::error::Elapsed> {
    tokio::time::timeout(Duration::from_secs(10), future).await
}
