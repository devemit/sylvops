//! Deterministic fixtures shared by `SylvOps` integration tests.

use std::{
    future::Future,
    io,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

#[derive(Debug)]
pub struct AbortOnDropTask<T> {
    task: Option<tokio::task::JoinHandle<T>>,
}

impl<T: Send + 'static> AbortOnDropTask<T> {
    /// Spawns a task that is aborted automatically unless it is explicitly joined.
    pub fn spawn(future: impl Future<Output = T> + Send + 'static) -> Self {
        Self {
            task: Some(tokio::spawn(future)),
        }
    }

    /// Waits for the task and disarms abort-on-drop cleanup.
    ///
    /// # Errors
    ///
    /// Returns the Tokio join error if the task panics or is cancelled.
    ///
    /// # Panics
    ///
    /// Panics if the same guard is joined more than once.
    pub async fn join(&mut self) -> Result<T, tokio::task::JoinError> {
        let result = self
            .task
            .as_mut()
            .expect("task can only be joined once")
            .await;
        self.task.take();
        result
    }
}

impl<T> Drop for AbortOnDropTask<T> {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

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

#[cfg(test)]
mod tests {
    use super::AbortOnDropTask;
    use std::{future::pending, time::Duration};

    struct DropSignal(Option<tokio::sync::oneshot::Sender<()>>);

    impl Drop for DropSignal {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }

    #[tokio::test]
    async fn cancelled_join_keeps_abort_on_drop_armed() {
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
        let mut task = AbortOnDropTask::spawn(async move {
            let _drop_signal = DropSignal(Some(dropped_tx));
            pending::<()>().await;
        });
        tokio::task::yield_now().await;

        assert!(
            tokio::time::timeout(Duration::from_millis(1), task.join())
                .await
                .is_err()
        );
        drop(task);
        tokio::time::timeout(Duration::from_secs(1), dropped_rx)
            .await
            .expect("aborted task did not release its guard")
            .expect("drop signal was cancelled");
    }
}
