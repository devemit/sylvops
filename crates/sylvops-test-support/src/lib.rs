//! Deterministic fixtures shared by `SylvOps` integration tests.

use std::{future::Future, time::Duration};

/// Apply a uniform timeout to process and IPC tests.
///
/// # Errors
///
/// Returns an elapsed error if `future` does not finish within ten seconds.
pub async fn bounded<T>(future: impl Future<Output = T>) -> Result<T, tokio::time::error::Elapsed> {
    tokio::time::timeout(Duration::from_secs(10), future).await
}
