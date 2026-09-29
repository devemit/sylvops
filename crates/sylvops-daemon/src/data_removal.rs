//! Explicit, containment-checked removal of SylvOps-owned user data.

use std::{
    collections::HashSet,
    fs::File,
    path::{Path, PathBuf},
};

use cap_std::{
    ambient_authority,
    fs::{Dir, OpenOptions as CapOpenOptions},
};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use thiserror::Error;

use crate::runtime::{OWNERSHIP_MARKER, RuntimePaths};

pub const DATA_REMOVAL_CONFIRMATION: &str = "DELETE SYLVOPS USER DATA";
pub const DATA_REMOVAL_RESERVATION_FILE: &str = ".sylvops-data-removal-pending";
const DATA_REMOVAL_AUDIT_FILE: &str = ".sylvops-data-removal-audit.json";
const MAX_DATA_REMOVAL_RESERVATION_BYTES: u64 = 64 * 1024;
const MAX_DATA_REMOVAL_AUDIT_BYTES: u64 = 1_024;
const MAX_PROTECTED_PATHS: usize = 4_096;
const MAX_OWNERSHIP_MARKER_BYTES: u64 = 256;
const DAEMON_STOP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const DAEMON_STOP_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);
const REMOVAL_COMPLETION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
const REMOVAL_OBSERVATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct DataRemovalReservation {
    application_id: String,
    handoff_token: String,
    protected_paths: Vec<PathBuf>,
    roots: Vec<ReservedRoot>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ReservedRoot {
    path: PathBuf,
    identity: FileIdentity,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct DataRemovalAudit {
    application_id: String,
    action: String,
    outcome: String,
    reason: String,
}

#[derive(Debug)]
struct DataRemovalLease {
    file: Option<File>,
    directory: Option<Dir>,
    remove_on_drop: bool,
}

impl DataRemovalLease {
    fn acquire(paths: &RuntimePaths) -> Result<Self, DataRemovalError> {
        let directory_path = coordination_directory(paths)?;
        let directory = Dir::open_ambient_dir(&directory_path, ambient_authority())
            .map_err(|_| DataRemovalError::UnsafeTarget)?;
        let directory_file = directory.into_std_file();
        let opened_identity = file_identity(&directory_file)?;
        create_coordination_directory(&directory_path)?;
        let current_handle = same_file::Handle::from_path(&directory_path)
            .map_err(|_| DataRemovalError::UnsafeTarget)?;
        let current_identity = file_identity(current_handle.as_file())?;
        if opened_identity != current_identity {
            return Err(DataRemovalError::IdentityChanged);
        }
        let directory = Dir::from_std_file(directory_file);
        let mut options = CapOpenOptions::new();
        options.create(true).read(true).write(true);
        let file = directory
            .open_with("lease.lock", &options)
            .map_err(|_| DataRemovalError::RemovalFailed)?;
        let file = file.into_std();
        fs2::FileExt::try_lock_exclusive(&file)
            .map_err(|_| DataRemovalError::OperationInProgress)?;
        Ok(Self {
            file: Some(file),
            directory: Some(directory),
            remove_on_drop: false,
        })
    }

    fn mark_complete(&mut self) {
        self.remove_on_drop = true;
    }
}

impl Drop for DataRemovalLease {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            let _ = fs2::FileExt::unlock(&file);
            drop(file);
        }
        if self.remove_on_drop
            && let Some(directory) = self.directory.take()
        {
            let _ = directory.remove_open_dir_all();
        }
    }
}

fn coordination_directory(paths: &RuntimePaths) -> Result<PathBuf, DataRemovalError> {
    let token = reserved_handoff_token(paths)?.ok_or(DataRemovalError::PreparationMissing)?;
    let directory = coordination_directory_path(paths, &token);
    create_coordination_directory(&directory)?;
    Ok(directory)
}

fn coordination_directory_path(paths: &RuntimePaths, token: &str) -> PathBuf {
    let temporary = std::env::temp_dir();
    let base = if temporary.is_absolute()
        && !owned_directories(paths)
            .iter()
            .any(|root| temporary.starts_with(root))
    {
        temporary
    } else {
        paths
            .data_directory
            .parent()
            .unwrap_or(&paths.data_directory)
            .to_path_buf()
    };
    base.join(format!("sylvops-data-removal-{token}"))
}

#[cfg(unix)]
fn create_coordination_directory(path: &Path) -> Result<(), DataRemovalError> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

    let mut builder = std::fs::DirBuilder::new();
    builder.mode(0o700);
    match builder.create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return Err(DataRemovalError::RemovalFailed),
    }
    let metadata = std::fs::symlink_metadata(path).map_err(|_| DataRemovalError::RemovalFailed)?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != nix::unistd::Uid::current().as_raw()
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(DataRemovalError::UnsafeTarget);
    }
    Ok(())
}

#[cfg(windows)]
fn create_coordination_directory(path: &Path) -> Result<(), DataRemovalError> {
    match std::fs::create_dir(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return Err(DataRemovalError::RemovalFailed),
    }
    let metadata = std::fs::symlink_metadata(path).map_err(|_| DataRemovalError::RemovalFailed)?;
    if !metadata.is_dir() || metadata_is_link_like(&metadata) {
        return Err(DataRemovalError::UnsafeTarget);
    }
    Ok(())
}

fn coordination_file(paths: &RuntimePaths, name: &str) -> Result<PathBuf, DataRemovalError> {
    Ok(coordination_directory(paths)?.join(name))
}

/// Reads and validates a durable removal reservation from the surviving owned directories.
///
/// # Errors
///
/// Fails closed when markers are linked, oversized, malformed, inconsistent, or unreadable.
pub fn reservation(paths: &RuntimePaths) -> Result<Option<Vec<PathBuf>>, DataRemovalError> {
    Ok(read_reservation(paths)?.map(|value| value.protected_paths))
}

fn read_reservation(
    paths: &RuntimePaths,
) -> Result<Option<DataRemovalReservation>, DataRemovalError> {
    let mut reservation: Option<DataRemovalReservation> = None;
    for directory in owned_directories(paths) {
        let marker = directory.join(DATA_REMOVAL_RESERVATION_FILE);
        let metadata = match std::fs::symlink_metadata(&marker) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err(DataRemovalError::RemovalFailed),
        };
        if !metadata.is_file()
            || metadata_is_link_like(&metadata)
            || !path_has_single_link(&marker, &metadata)
            || metadata.len() > MAX_DATA_REMOVAL_RESERVATION_BYTES
        {
            return Err(DataRemovalError::RemovalFailed);
        }
        let bytes = std::fs::read(marker).map_err(|_| DataRemovalError::RemovalFailed)?;
        let current: DataRemovalReservation =
            serde_json::from_slice(&bytes).map_err(|_| DataRemovalError::RemovalFailed)?;
        if current.application_id != sylvops_core::APPLICATION_ID
            || current.handoff_token.len() != 32
            || !current
                .handoff_token
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
            || current.protected_paths.len() > MAX_PROTECTED_PATHS
            || current.roots.len() > owned_directories(paths).len()
            || reservation
                .as_ref()
                .is_some_and(|existing| existing != &current)
        {
            return Err(DataRemovalError::RemovalFailed);
        }
        reservation = Some(current);
    }
    Ok(reservation)
}

/// Reports whether any owned state directory carries an active removal reservation.
///
/// # Errors
///
/// Fails closed when a reservation path cannot be inspected.
pub fn reservation_pending(paths: &RuntimePaths) -> Result<bool, DataRemovalError> {
    Ok(read_reservation(paths)?.is_some())
}

/// Reserves every owned state directory before the daemon yields to the removal client.
///
/// # Errors
///
/// Removes any partial reservation and fails when a marker cannot be written atomically.
pub fn reserve(paths: &RuntimePaths, protected_paths: &[PathBuf]) -> Result<(), DataRemovalError> {
    reserve_with_token(
        paths,
        protected_paths,
        &uuid::Uuid::new_v4().simple().to_string(),
    )
}

/// Reserves the removal and returns an unguessable detached-helper handoff token.
///
/// # Errors
///
/// Fails when the reservation cannot be validated, encoded, or written safely.
pub fn reserve_for_daemon(
    paths: &RuntimePaths,
    protected_paths: &[PathBuf],
) -> Result<String, DataRemovalError> {
    let handoff_token = uuid::Uuid::new_v4().simple().to_string();
    reserve_with_token(paths, protected_paths, &handoff_token)?;
    Ok(handoff_token)
}

fn reserve_with_token(
    paths: &RuntimePaths,
    protected_paths: &[PathBuf],
    handoff_token: &str,
) -> Result<(), DataRemovalError> {
    if handoff_token.len() != 32 || !handoff_token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(DataRemovalError::RemovalFailed);
    }
    if protected_paths.len() > MAX_PROTECTED_PATHS {
        return Err(DataRemovalError::RemovalFailed);
    }
    let plan = DataRemovalPlan::prepare(paths, DATA_REMOVAL_CONFIRMATION, protected_paths)?;
    let encoded = serde_json::to_vec(&DataRemovalReservation {
        application_id: sylvops_core::APPLICATION_ID.into(),
        handoff_token: handoff_token.into(),
        protected_paths: plan.protected_paths.clone(),
        roots: plan
            .roots
            .iter()
            .map(|root| ReservedRoot {
                path: root.path.clone(),
                identity: root.identity,
            })
            .collect(),
    })
    .map_err(|_| DataRemovalError::RemovalFailed)?;
    if u64::try_from(encoded.len()).unwrap_or(u64::MAX) > MAX_DATA_REMOVAL_RESERVATION_BYTES {
        return Err(DataRemovalError::RemovalFailed);
    }
    let mut written = Vec::new();
    for directory in owned_directories(paths) {
        let marker = directory.join(DATA_REMOVAL_RESERVATION_FILE);
        if crate::atomic_file::write(&marker, &encoded).is_err() {
            for path in written {
                let _ = std::fs::remove_file(path);
            }
            return Err(DataRemovalError::RemovalFailed);
        }
        written.push(marker);
    }
    Ok(())
}

fn reserved_handoff_token(paths: &RuntimePaths) -> Result<Option<String>, DataRemovalError> {
    Ok(read_reservation(paths)?.map(|value| value.handoff_token))
}

/// Best-effort cleanup used when the daemon cannot deliver the prepared response.
pub fn clear_reservation(paths: &RuntimePaths) {
    for directory in owned_directories(paths) {
        let _ = std::fs::remove_file(directory.join(DATA_REMOVAL_RESERVATION_FILE));
    }
}

/// Completes a daemon-prepared removal using its durable authoritative protection set.
///
/// Repeating completion is safe. If an earlier pass already consumed the reservation, the
/// traversal still preserves any Git checkout it encounters below an owned root.
///
/// # Errors
///
/// Refuses weak confirmation and any root that is no longer marker-owned or safely contained.
pub async fn complete_prepared_removal(
    paths: &RuntimePaths,
    confirmation: &str,
) -> Result<(), DataRemovalError> {
    if confirmation != DATA_REMOVAL_CONFIRMATION {
        return Err(DataRemovalError::ConfirmationRequired);
    }
    if removal_complete(paths)? {
        return Ok(());
    }
    let reservation = read_reservation(paths)?.ok_or(DataRemovalError::PreparationMissing)?;
    let plan = DataRemovalPlan::prepare(paths, confirmation, &reservation.protected_paths)?;
    if !plan.matches_reservation(&reservation.roots) {
        return Err(DataRemovalError::IdentityChanged);
    }
    plan.execute().await
}

/// Waits until the daemon removes its authentication token during orderly shutdown.
///
/// # Errors
///
/// Returns a bounded diagnostic when the daemon does not stop before the deadline.
pub async fn wait_for_daemon_stop(paths: &RuntimePaths) -> Result<(), DataRemovalError> {
    let deadline = tokio::time::Instant::now() + DAEMON_STOP_TIMEOUT;
    while paths.authentication_token.exists() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(DAEMON_STOP_POLL_INTERVAL).await;
    }
    if paths.authentication_token.exists() {
        return Err(DataRemovalError::DaemonDidNotStop);
    }
    Ok(())
}

/// Runs the daemon-prepared plan from a detached lifecycle helper after daemon shutdown.
///
/// Transient sharing violations are retried within a fixed deadline. The final outcome is emitted
/// as a categorical, redacted audit event.
///
/// # Errors
///
/// Returns a bounded removal error when shutdown or cleanup does not complete.
pub async fn run_prepared_removal(paths: &RuntimePaths) -> Result<(), DataRemovalError> {
    let mut lease = DataRemovalLease::acquire(paths)?;
    if let Err(error) = wait_for_daemon_stop(paths).await {
        record_helper_failure(paths, error);
        return Err(error);
    }
    let result = run_prepared_removal_after_shutdown(paths).await;
    if result.is_ok() {
        lease.mark_complete();
    }
    result
}

/// Runs a prepared removal only after the originating daemon process has fully exited.
///
/// # Errors
///
/// Returns a bounded removal error when the daemon remains live or cleanup does not complete.
pub async fn run_prepared_removal_after_process(
    paths: &RuntimePaths,
    daemon_process_id: u32,
    handoff_token: &str,
) -> Result<(), DataRemovalError> {
    let reserved_token =
        reserved_handoff_token(paths)?.ok_or(DataRemovalError::PreparationMissing)?;
    if handoff_token.len() != reserved_token.len()
        || !bool::from(handoff_token.as_bytes().ct_eq(reserved_token.as_bytes()))
    {
        record_helper_failure(paths, DataRemovalError::IdentityChanged);
        return Err(DataRemovalError::IdentityChanged);
    }
    let mut lease = DataRemovalLease::acquire(paths)?;
    if let Err(error) = wait_for_daemon_stop(paths).await {
        record_helper_failure(paths, error);
        return Err(error);
    }
    if let Err(error) = wait_for_process_exit(daemon_process_id).await {
        record_helper_failure(paths, error);
        return Err(error);
    }
    let result = run_prepared_removal_after_shutdown(paths).await;
    if result.is_ok() {
        lease.mark_complete();
    }
    result
}

async fn wait_for_process_exit(process_id: u32) -> Result<(), DataRemovalError> {
    let deadline = tokio::time::Instant::now() + DAEMON_STOP_TIMEOUT;
    loop {
        let running = crate::native_upgrade::process_is_running(process_id)
            .map_err(|_| DataRemovalError::DaemonDidNotStop)?;
        if !running {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(DataRemovalError::DaemonDidNotStop);
        }
        tokio::time::sleep(DAEMON_STOP_POLL_INTERVAL).await;
    }
}

/// Recovers a previously prepared removal only after claiming its exclusive helper lease.
///
/// # Errors
///
/// Refuses recovery while another helper still owns the operation.
pub async fn recover_prepared_removal(paths: &RuntimePaths) -> Result<(), DataRemovalError> {
    reserved_handoff_token(paths)?.ok_or(DataRemovalError::PreparationMissing)?;
    let mut lease = DataRemovalLease::acquire(paths)?;
    if let Err(error) = wait_for_daemon_stop(paths).await {
        record_helper_failure(paths, error);
        return Err(error);
    }
    let result = run_prepared_removal_after_shutdown(paths).await;
    if result.is_ok() {
        lease.mark_complete();
    }
    result
}

async fn run_prepared_removal_after_shutdown(paths: &RuntimePaths) -> Result<(), DataRemovalError> {
    let deadline = tokio::time::Instant::now() + REMOVAL_COMPLETION_TIMEOUT;
    loop {
        match complete_prepared_removal(paths, DATA_REMOVAL_CONFIRMATION).await {
            Ok(()) => {
                if let Ok(path) = coordination_file(paths, "audit.json") {
                    let _ = std::fs::remove_file(path);
                }
                tracing::info!(
                    target: "sylvops_audit",
                    action = "user_data_removal_completed",
                    outcome = "succeeded",
                    details = "{}",
                );
                return Ok(());
            }
            Err(DataRemovalError::RemovalFailed) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(DAEMON_STOP_POLL_INTERVAL).await;
            }
            Err(error) => {
                record_helper_failure(paths, error);
                return Err(error);
            }
        }
    }
}

fn record_helper_failure(paths: &RuntimePaths, error: DataRemovalError) {
    record_failure_audit(paths, error);
    tracing::error!(
        target: "sylvops_audit",
        action = "user_data_removal_completed",
        outcome = "failed",
        reason = error.audit_reason(),
    );
}

/// Waits for the detached daemon-owned helper to finish removal without mutating filesystem state.
///
/// # Errors
///
/// Returns a bounded error when completion is not observable before the deadline.
pub async fn wait_for_removal_completion(paths: &RuntimePaths) -> Result<(), DataRemovalError> {
    let deadline = tokio::time::Instant::now() + REMOVAL_OBSERVATION_TIMEOUT;
    let external_audit = reserved_handoff_token(paths)?
        .map(|token| coordination_directory_path(paths, &token).join("audit.json"));
    loop {
        let external_failure = if let Some(path) = external_audit.as_deref() {
            read_audit_path(path)?.is_some()
        } else {
            false
        };
        if external_failure || failure_audit(paths)?.is_some() {
            return Err(DataRemovalError::RemovalFailed);
        }
        match removal_complete(paths) {
            Ok(true) => return Ok(()),
            Ok(false) | Err(DataRemovalError::UnownedTarget)
                if tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(DAEMON_STOP_POLL_INTERVAL).await;
            }
            Ok(false) | Err(DataRemovalError::UnownedTarget) => {
                return Err(DataRemovalError::RemovalFailed);
            }
            Err(error) => return Err(error),
        }
    }
}

fn record_failure_audit(paths: &RuntimePaths, error: DataRemovalError) {
    let encoded = serde_json::to_vec(&DataRemovalAudit {
        application_id: sylvops_core::APPLICATION_ID.into(),
        action: "user_data_removal_completed".into(),
        outcome: "failed".into(),
        reason: error.audit_reason().into(),
    });
    let Ok(encoded) = encoded else {
        return;
    };
    if u64::try_from(encoded.len()).unwrap_or(u64::MAX) > MAX_DATA_REMOVAL_AUDIT_BYTES {
        return;
    }
    if let Ok(path) = coordination_file(paths, "audit.json") {
        let _ = crate::atomic_file::write(&path, &encoded);
    }
    for directory in owned_directories(paths) {
        if validate_owned_root(directory, &[]).ok().flatten().is_some() {
            let _ = crate::atomic_file::write(&directory.join(DATA_REMOVAL_AUDIT_FILE), &encoded);
        }
    }
}

fn failure_audit(paths: &RuntimePaths) -> Result<Option<DataRemovalAudit>, DataRemovalError> {
    let mut audit = match reserved_handoff_token(paths)? {
        Some(token) => {
            read_audit_path(&coordination_directory_path(paths, &token).join("audit.json"))?
        }
        None => None,
    };
    for directory in owned_directories(paths) {
        let path = directory.join(DATA_REMOVAL_AUDIT_FILE);
        if let Some(current) = read_audit_path(&path)? {
            if audit.as_ref().is_some_and(|existing| existing != &current) {
                return Err(DataRemovalError::RemovalFailed);
            }
            audit = Some(current);
        }
    }
    Ok(audit)
}

fn read_audit_path(path: &Path) -> Result<Option<DataRemovalAudit>, DataRemovalError> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(DataRemovalError::RemovalFailed),
    };
    if !metadata.is_file()
        || metadata_is_link_like(&metadata)
        || !path_has_single_link(path, &metadata)
        || metadata.len() > MAX_DATA_REMOVAL_AUDIT_BYTES
    {
        return Err(DataRemovalError::RemovalFailed);
    }
    let current: DataRemovalAudit =
        serde_json::from_slice(&std::fs::read(path).map_err(|_| DataRemovalError::RemovalFailed)?)
            .map_err(|_| DataRemovalError::RemovalFailed)?;
    if current.application_id != sylvops_core::APPLICATION_ID
        || current.action != "user_data_removal_completed"
        || current.outcome != "failed"
    {
        return Err(DataRemovalError::RemovalFailed);
    }
    Ok(Some(current))
}

/// Reports whether removal is complete: roots are absent, or only preserved Git checkouts remain.
///
/// # Errors
///
/// Fails closed when any configured path cannot be inspected.
pub fn removal_complete(paths: &RuntimePaths) -> Result<bool, DataRemovalError> {
    for directory in owned_directories(paths) {
        match std::fs::symlink_metadata(directory) {
            Ok(_) => {
                let Some(canonical) = validate_owned_root(directory, &[])? else {
                    continue;
                };
                let (only_preserved, found_checkout) = scan_preserved_content(&canonical)?;
                if !only_preserved || !found_checkout {
                    return Ok(false);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(DataRemovalError::RemovalFailed),
        }
    }
    Ok(true)
}

fn scan_preserved_content(path: &Path) -> Result<(bool, bool), DataRemovalError> {
    let mut found_checkout = false;
    for entry in std::fs::read_dir(path).map_err(|_| DataRemovalError::RemovalFailed)? {
        let entry = entry.map_err(|_| DataRemovalError::RemovalFailed)?;
        if entry.file_name() == OWNERSHIP_MARKER {
            continue;
        }
        if entry.file_name() == DATA_REMOVAL_RESERVATION_FILE {
            return Ok((false, false));
        }
        let child = entry.path();
        let metadata =
            std::fs::symlink_metadata(&child).map_err(|_| DataRemovalError::RemovalFailed)?;
        if metadata_is_link_like(&metadata) || !metadata.is_dir() {
            return Ok((false, false));
        }
        if is_git_checkout(&child) {
            found_checkout = true;
            continue;
        }
        let (only_preserved, nested_checkout) = scan_preserved_content(&child)?;
        if !only_preserved {
            return Ok((false, false));
        }
        found_checkout |= nested_checkout;
    }
    Ok((true, found_checkout))
}

fn owned_directories(paths: &RuntimePaths) -> [&Path; 3] {
    [
        &paths.data_directory,
        &paths.config_directory,
        &paths.runtime_directory,
    ]
}

#[derive(Debug)]
pub struct DataRemovalPlan {
    roots: Vec<OwnedRoot>,
    protected_paths: Vec<PathBuf>,
}

#[derive(Debug)]
struct OwnedRoot {
    path: PathBuf,
    identity: FileIdentity,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct FileIdentity {
    volume: u64,
    file: u64,
}

impl DataRemovalPlan {
    /// Validates a one-shot removal plan against authoritative runtime and protected-content paths.
    ///
    /// # Errors
    ///
    /// Refuses weak confirmation, broad/unowned targets, links, and any target overlapping user content.
    pub fn prepare(
        paths: &RuntimePaths,
        confirmation: &str,
        protected_paths: &[PathBuf],
    ) -> Result<Self, DataRemovalError> {
        if confirmation != DATA_REMOVAL_CONFIRMATION {
            return Err(DataRemovalError::ConfirmationRequired);
        }
        let protected_paths = protected_paths
            .iter()
            .map(|path| canonical_if_present(path))
            .collect::<Result<Vec<_>, _>>()?;
        let mut roots = Vec::new();
        let mut seen = HashSet::new();
        for path in [
            &paths.data_directory,
            &paths.config_directory,
            &paths.runtime_directory,
        ] {
            let canonical = validate_owned_root(path, &protected_paths)?;
            if let Some(canonical) = canonical
                && seen.insert(canonical.clone())
            {
                let identity_handle = same_file::Handle::from_path(&canonical)
                    .map_err(|_| DataRemovalError::UnsafeTarget)?;
                let identity = file_identity(identity_handle.as_file())
                    .map_err(|_| DataRemovalError::UnsafeTarget)?;
                roots.push(OwnedRoot {
                    path: canonical,
                    identity,
                });
            }
        }
        roots.sort_by_key(|root| root.path.components().count());
        let mut collapsed: Vec<OwnedRoot> = Vec::new();
        for root in roots {
            if !collapsed
                .iter()
                .any(|parent| root.path.starts_with(&parent.path))
            {
                collapsed.push(root);
            }
        }
        if collapsed.is_empty() {
            return Err(DataRemovalError::UnownedTarget);
        }
        Ok(Self {
            roots: collapsed,
            protected_paths,
        })
    }

    /// Revalidates and removes only the owned roots in this plan. Repeating a completed plan is safe.
    ///
    /// # Errors
    ///
    /// Stops before a target whose identity, marker, or containment changed.
    pub async fn execute(&self) -> Result<(), DataRemovalError> {
        for root in &self.roots {
            if !root.path.exists() {
                continue;
            }
            let Some(current) = validate_owned_root(&root.path, &self.protected_paths)? else {
                continue;
            };
            if current != root.path {
                return Err(DataRemovalError::IdentityChanged);
            }
            let directory = open_owned_root(&current, &root.identity)?;
            let protected = self.protected_paths.clone();
            tokio::task::spawn_blocking(move || {
                remove_open_directory(directory, &current, &protected)
            })
            .await
            .map_err(|_| DataRemovalError::RemovalFailed)??;
        }
        Ok(())
    }

    fn matches_reservation(&self, roots: &[ReservedRoot]) -> bool {
        self.roots.iter().all(|root| {
            roots
                .iter()
                .any(|reserved| root.path == reserved.path && root.identity == reserved.identity)
        }) && roots.iter().all(|reserved| {
            self.roots.iter().any(|root| root.path == reserved.path)
                || std::fs::symlink_metadata(&reserved.path)
                    .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        })
    }
}

fn validate_owned_root(
    path: &Path,
    protected_paths: &[PathBuf],
) -> Result<Option<PathBuf>, DataRemovalError> {
    if !path.is_absolute() || path.parent().is_none() {
        return Err(DataRemovalError::UnsafeTarget);
    }
    if !path.exists() {
        return Ok(None);
    }
    let metadata = std::fs::symlink_metadata(path).map_err(|_| DataRemovalError::UnsafeTarget)?;
    if !metadata.is_dir() || metadata_is_link_like(&metadata) {
        return Err(DataRemovalError::UnsafeTarget);
    }
    let canonical = std::fs::canonicalize(path).map_err(|_| DataRemovalError::UnsafeTarget)?;
    if canonical.parent().is_none() || is_home_directory(&canonical) {
        return Err(DataRemovalError::UnsafeTarget);
    }
    let marker_path = canonical.join(OWNERSHIP_MARKER);
    let marker_metadata =
        std::fs::symlink_metadata(&marker_path).map_err(|_| DataRemovalError::UnownedTarget)?;
    if !marker_metadata.is_file()
        || metadata_is_link_like(&marker_metadata)
        || !path_has_single_link(&marker_path, &marker_metadata)
        || marker_metadata.len() > MAX_OWNERSHIP_MARKER_BYTES
    {
        return Err(DataRemovalError::UnownedTarget);
    }
    let marker =
        std::fs::read_to_string(marker_path).map_err(|_| DataRemovalError::UnownedTarget)?;
    if marker != sylvops_core::APPLICATION_ID {
        return Err(DataRemovalError::UnownedTarget);
    }
    if protected_paths
        .iter()
        .any(|protected| canonical.starts_with(protected))
    {
        return Err(DataRemovalError::ProtectedContent);
    }
    if is_git_checkout(&canonical) {
        return Err(DataRemovalError::ProtectedContent);
    }
    Ok(Some(canonical))
}

fn open_owned_root(path: &Path, expected_identity: &FileIdentity) -> Result<Dir, DataRemovalError> {
    let directory = Dir::open_ambient_dir(path, ambient_authority())
        .map_err(|_| DataRemovalError::IdentityChanged)?;
    let file = directory.into_std_file();
    let identity = file_identity(&file).map_err(|_| DataRemovalError::IdentityChanged)?;
    if &identity != expected_identity {
        return Err(DataRemovalError::IdentityChanged);
    }
    let directory = Dir::from_std_file(file);
    validate_open_owned_root(&directory)?;
    Ok(directory)
}

fn remove_open_directory(
    directory: Dir,
    logical_path: &Path,
    protected_paths: &[PathBuf],
) -> Result<bool, DataRemovalError> {
    if protected_paths
        .iter()
        .any(|protected| protected == logical_path)
        || open_directory_is_git_checkout(&directory)
    {
        return Ok(true);
    }
    let mut preserved_content = false;
    for entry in directory
        .entries()
        .map_err(|_| DataRemovalError::RemovalFailed)?
    {
        let entry = entry.map_err(|_| DataRemovalError::RemovalFailed)?;
        let name = entry.file_name();
        if name == OWNERSHIP_MARKER || name == DATA_REMOVAL_RESERVATION_FILE {
            continue;
        }
        let child_path = logical_path.join(&name);
        if protected_paths
            .iter()
            .any(|protected| protected == &child_path)
        {
            preserved_content = true;
            continue;
        }
        let file_type = entry
            .file_type()
            .map_err(|_| DataRemovalError::RemovalFailed)?;
        if file_type.is_dir() && !file_type.is_symlink() {
            let child = entry
                .open_dir()
                .map_err(|_| DataRemovalError::IdentityChanged)?;
            preserved_content |= remove_open_directory(child, &child_path, protected_paths)?;
        } else {
            entry
                .remove_file()
                .or_else(|_| entry.remove_dir())
                .map_err(|_| DataRemovalError::RemovalFailed)?;
        }
    }
    if preserved_content {
        remove_relative_file_if_present(&directory, DATA_REMOVAL_RESERVATION_FILE)?;
        return Ok(true);
    }
    finalize_empty_directory(directory, logical_path)
}

fn finalize_empty_directory(directory: Dir, logical_path: &Path) -> Result<bool, DataRemovalError> {
    let ownership = directory.read(OWNERSHIP_MARKER).ok();
    let reservation = directory.read(DATA_REMOVAL_RESERVATION_FILE).ok();
    let file = directory.into_std_file();
    let identity = file_identity(&file)?;
    let directory = Dir::from_std_file(file);
    remove_relative_file_if_present(&directory, DATA_REMOVAL_RESERVATION_FILE)?;
    remove_relative_file_if_present(&directory, OWNERSHIP_MARKER)?;
    if directory.remove_open_dir().is_ok() {
        return Ok(false);
    }
    let recovery = Dir::open_ambient_dir(logical_path, ambient_authority())
        .map_err(|_| DataRemovalError::RemovalFailed)?;
    let recovery_file = recovery.into_std_file();
    let recovery_identity = file_identity(&recovery_file)?;
    if recovery_identity != identity {
        return Err(DataRemovalError::IdentityChanged);
    }
    let recovery = Dir::from_std_file(recovery_file);
    if let Some(ownership) = ownership {
        let _ = recovery.remove_file(OWNERSHIP_MARKER);
        recovery
            .write(OWNERSHIP_MARKER, ownership)
            .map_err(|_| DataRemovalError::RemovalFailed)?;
    }
    if let Some(reservation) = reservation {
        let _ = recovery.remove_file(DATA_REMOVAL_RESERVATION_FILE);
        recovery
            .write(DATA_REMOVAL_RESERVATION_FILE, reservation)
            .map_err(|_| DataRemovalError::RemovalFailed)?;
    }
    Err(DataRemovalError::RemovalFailed)
}

fn validate_open_owned_root(directory: &Dir) -> Result<(), DataRemovalError> {
    let metadata = directory
        .symlink_metadata(OWNERSHIP_MARKER)
        .map_err(|_| DataRemovalError::UnownedTarget)?;
    if !metadata.is_file() || metadata.len() > MAX_OWNERSHIP_MARKER_BYTES {
        return Err(DataRemovalError::UnownedTarget);
    }
    let marker = directory
        .read_to_string(OWNERSHIP_MARKER)
        .map_err(|_| DataRemovalError::UnownedTarget)?;
    if marker != sylvops_core::APPLICATION_ID {
        return Err(DataRemovalError::UnownedTarget);
    }
    Ok(())
}

fn open_directory_is_git_checkout(directory: &Dir) -> bool {
    directory.symlink_metadata(".git").is_ok()
}

fn remove_relative_file_if_present(directory: &Dir, name: &str) -> Result<(), DataRemovalError> {
    match directory.remove_file(name) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(DataRemovalError::RemovalFailed),
    }
}

fn is_git_checkout(path: &Path) -> bool {
    std::fs::symlink_metadata(path.join(".git")).is_ok()
}

#[cfg(unix)]
fn metadata_is_link_like(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
fn metadata_is_link_like(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(unix)]
fn path_has_single_link(_path: &Path, metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;

    metadata.nlink() == 1
}

#[cfg(unix)]
fn file_identity(file: &std::fs::File) -> Result<FileIdentity, DataRemovalError> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file
        .metadata()
        .map_err(|_| DataRemovalError::IdentityChanged)?;
    Ok(FileIdentity {
        volume: metadata.dev(),
        file: metadata.ino(),
    })
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn path_has_single_link(path: &Path, _metadata: &std::fs::Metadata) -> bool {
    use std::{fs::File, os::windows::io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    let Ok(file) = File::open(path) else {
        return false;
    };
    // SAFETY: `information` is a writable value of the exact Win32 output type and the file
    // handle remains open for the duration of the call.
    let mut information: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: both arguments are valid for the duration of this synchronous Win32 call.
    let succeeded = unsafe {
        GetFileInformationByHandle(file.as_raw_handle(), std::ptr::addr_of_mut!(information))
    };
    succeeded != 0 && information.nNumberOfLinks == 1
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn file_identity(file: &std::fs::File) -> Result<FileIdentity, DataRemovalError> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    // SAFETY: `information` is a writable value of the exact Win32 output type.
    let mut information: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: the file handle and output pointer remain valid for this synchronous call.
    if unsafe {
        GetFileInformationByHandle(file.as_raw_handle(), std::ptr::addr_of_mut!(information))
    } == 0
    {
        return Err(DataRemovalError::IdentityChanged);
    }
    Ok(FileIdentity {
        volume: u64::from(information.dwVolumeSerialNumber),
        file: (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow),
    })
}

fn canonical_if_present(path: &Path) -> Result<PathBuf, DataRemovalError> {
    if path.exists() {
        std::fs::canonicalize(path).map_err(|_| DataRemovalError::ProtectedContent)
    } else if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Err(DataRemovalError::ProtectedContent)
    }
}

fn is_home_directory(path: &Path) -> bool {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(PathBuf::from)
        .and_then(|home| std::fs::canonicalize(home).ok())
        .is_some_and(|home| home == path)
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum DataRemovalError {
    #[error("exact user-data removal confirmation is required")]
    ConfirmationRequired,
    #[error("user-data removal target is unsafe")]
    UnsafeTarget,
    #[error("user-data removal target is not SylvOps-owned")]
    UnownedTarget,
    #[error("user-data removal would overlap a repository or worktree")]
    ProtectedContent,
    #[error("user-data removal target changed after validation")]
    IdentityChanged,
    #[error("user-data removal did not complete")]
    RemovalFailed,
    #[error("user-data removal was not prepared by the daemon")]
    PreparationMissing,
    #[error("daemon did not stop before user-data removal")]
    DaemonDidNotStop,
    #[error("user-data removal is already in progress")]
    OperationInProgress,
}

impl DataRemovalError {
    const fn audit_reason(self) -> &'static str {
        match self {
            Self::ConfirmationRequired => "confirmation_required",
            Self::UnsafeTarget => "unsafe_target",
            Self::UnownedTarget => "unowned_target",
            Self::ProtectedContent => "protected_content",
            Self::IdentityChanged => "identity_changed",
            Self::RemovalFailed => "removal_failed",
            Self::PreparationMissing => "preparation_missing",
            Self::DaemonDidNotStop => "daemon_did_not_stop",
            Self::OperationInProgress => "operation_in_progress",
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::runtime::RuntimePaths;

    use super::{
        DATA_REMOVAL_CONFIRMATION, DataRemovalPlan, complete_prepared_removal, reserve,
        wait_for_daemon_stop,
    };

    #[tokio::test]
    async fn removal_requires_strong_confirmation_and_preserves_user_content() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::discover(Some(&temporary.path().join("state"))).unwrap();
        paths.prepare().unwrap();
        let repository = temporary.path().join("repository");
        let worktree = temporary.path().join("worktree");
        tokio::fs::create_dir_all(repository.join(".git"))
            .await
            .unwrap();
        tokio::fs::create_dir_all(&worktree).await.unwrap();
        tokio::fs::write(worktree.join("keep.txt"), b"keep")
            .await
            .unwrap();

        assert!(
            DataRemovalPlan::prepare(&paths, "wrong", &[repository.clone(), worktree.clone()])
                .is_err()
        );
        let plan = DataRemovalPlan::prepare(
            &paths,
            DATA_REMOVAL_CONFIRMATION,
            &[repository.clone(), worktree.clone()],
        )
        .unwrap();
        plan.execute().await.unwrap();
        plan.execute().await.unwrap();
        plan.execute().await.unwrap();

        assert!(!paths.data_directory.exists());
        assert!(!paths.config_directory.exists());
        assert!(repository.join(".git").exists());
        assert_eq!(
            tokio::fs::read(worktree.join("keep.txt")).await.unwrap(),
            b"keep"
        );
    }

    #[tokio::test]
    async fn removal_preserves_a_managed_worktree_inside_an_owned_root() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::discover(Some(&temporary.path().join("state"))).unwrap();
        paths.prepare().unwrap();
        let managed_worktree = paths.data_directory.join("worktrees/project/task");
        std::fs::create_dir_all(managed_worktree.join(".git")).unwrap();
        std::fs::write(managed_worktree.join("keep.txt"), b"keep").unwrap();
        std::fs::write(paths.data_directory.join("sylvops.db"), b"metadata").unwrap();

        let plan = DataRemovalPlan::prepare(
            &paths,
            DATA_REMOVAL_CONFIRMATION,
            std::slice::from_ref(&managed_worktree),
        )
        .unwrap();
        plan.execute().await.unwrap();

        assert_eq!(
            std::fs::read(managed_worktree.join("keep.txt")).unwrap(),
            b"keep"
        );
        assert!(!paths.data_directory.join("sylvops.db").exists());
        assert!(!paths.config_directory.exists());
        assert!(!paths.runtime_directory.exists());
    }

    #[tokio::test]
    async fn prepared_removal_uses_authoritative_protection_and_is_repeatable() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::discover(Some(&temporary.path().join("state"))).unwrap();
        paths.prepare().unwrap();
        let managed_worktree = paths.data_directory.join("worktrees/project/task");
        std::fs::create_dir_all(managed_worktree.join(".git")).unwrap();
        std::fs::write(managed_worktree.join("keep.txt"), b"keep").unwrap();
        std::fs::write(paths.data_directory.join("sylvops.db"), b"metadata").unwrap();
        reserve(&paths, std::slice::from_ref(&managed_worktree)).unwrap();

        complete_prepared_removal(&paths, DATA_REMOVAL_CONFIRMATION)
            .await
            .unwrap();
        complete_prepared_removal(&paths, DATA_REMOVAL_CONFIRMATION)
            .await
            .unwrap();

        assert_eq!(
            std::fs::read(managed_worktree.join("keep.txt")).unwrap(),
            b"keep"
        );
        assert!(!paths.data_directory.join("sylvops.db").exists());
        assert!(!paths.config_directory.exists());
        assert!(!paths.runtime_directory.exists());
    }

    #[tokio::test]
    async fn prepared_removal_is_idempotent_after_all_owned_roots_are_gone() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::discover(Some(&temporary.path().join("state"))).unwrap();
        paths.prepare().unwrap();
        reserve(&paths, &[]).unwrap();

        complete_prepared_removal(&paths, DATA_REMOVAL_CONFIRMATION)
            .await
            .unwrap();
        complete_prepared_removal(&paths, DATA_REMOVAL_CONFIRMATION)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn completion_refuses_missing_reservation_while_owned_data_remains() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::discover(Some(&temporary.path().join("state"))).unwrap();
        paths.prepare().unwrap();
        let sentinel = paths.data_directory.join("must-not-delete");
        std::fs::write(&sentinel, b"state").unwrap();

        let error = complete_prepared_removal(&paths, DATA_REMOVAL_CONFIRMATION)
            .await
            .unwrap_err();

        assert_eq!(error, super::DataRemovalError::PreparationMissing);
        assert!(sentinel.exists());
    }

    #[tokio::test]
    async fn removal_refuses_a_root_identity_swap_after_preparation() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::discover(Some(&temporary.path().join("state"))).unwrap();
        paths.prepare().unwrap();
        let plan = DataRemovalPlan::prepare(&paths, DATA_REMOVAL_CONFIRMATION, &[]).unwrap();
        let original_data = temporary.path().join("original-data");
        if std::fs::rename(&paths.data_directory, &original_data).is_err() {
            assert!(paths.data_directory.exists());
            return;
        }
        std::fs::create_dir(&paths.data_directory).unwrap();
        std::fs::write(
            paths.data_directory.join(crate::runtime::OWNERSHIP_MARKER),
            sylvops_core::APPLICATION_ID,
        )
        .unwrap();
        let replacement_sentinel = paths.data_directory.join("replacement");
        std::fs::write(&replacement_sentinel, b"keep").unwrap();

        let error = plan.execute().await.unwrap_err();

        assert_eq!(error, super::DataRemovalError::IdentityChanged);
        assert!(replacement_sentinel.exists());
        assert!(original_data.exists());
    }

    #[tokio::test]
    async fn helper_refuses_a_root_replaced_after_daemon_reservation() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::discover(Some(&temporary.path().join("state"))).unwrap();
        paths.prepare().unwrap();
        reserve(&paths, &[]).unwrap();
        let original_data = temporary.path().join("reserved-data");
        std::fs::rename(&paths.data_directory, &original_data).unwrap();
        std::fs::create_dir(&paths.data_directory).unwrap();
        std::fs::write(
            paths.data_directory.join(crate::runtime::OWNERSHIP_MARKER),
            sylvops_core::APPLICATION_ID,
        )
        .unwrap();
        std::fs::copy(
            original_data.join(super::DATA_REMOVAL_RESERVATION_FILE),
            paths
                .data_directory
                .join(super::DATA_REMOVAL_RESERVATION_FILE),
        )
        .unwrap();
        let replacement_sentinel = paths.data_directory.join("replacement");
        std::fs::write(&replacement_sentinel, b"keep").unwrap();

        let error = complete_prepared_removal(&paths, DATA_REMOVAL_CONFIRMATION)
            .await
            .unwrap_err();

        assert_eq!(error, super::DataRemovalError::IdentityChanged);
        assert!(replacement_sentinel.exists());
        assert!(original_data.exists());
    }

    #[tokio::test]
    async fn helper_refuses_a_handoff_token_not_authorized_by_the_daemon() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::discover(Some(&temporary.path().join("state"))).unwrap();
        paths.prepare().unwrap();
        let token = super::reserve_for_daemon(&paths, &[]).unwrap();
        let replacement = if token.starts_with('0') { '1' } else { '0' };
        let mut wrong_token = token;
        wrong_token.replace_range(..1, &replacement.to_string());

        let error = super::run_prepared_removal_after_process(&paths, 42, &wrong_token)
            .await
            .unwrap_err();

        assert_eq!(error, super::DataRemovalError::IdentityChanged);
        assert!(super::failure_audit(&paths).unwrap().is_some());
        assert!(paths.data_directory.exists());
    }

    #[tokio::test]
    async fn recovery_refuses_to_overlap_an_active_helper_lease() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::discover(Some(&temporary.path().join("state"))).unwrap();
        paths.prepare().unwrap();
        reserve(&paths, &[]).unwrap();
        let lease = super::DataRemovalLease::acquire(&paths).unwrap();

        let error = super::recover_prepared_removal(&paths).await.unwrap_err();

        assert_eq!(error, super::DataRemovalError::OperationInProgress);
        assert!(paths.data_directory.exists());
        drop(lease);
        super::recover_prepared_removal(&paths).await.unwrap();
        assert!(!paths.data_directory.exists());
    }

    #[tokio::test]
    async fn helper_refuses_a_linked_coordination_directory() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::discover(Some(&temporary.path().join("state"))).unwrap();
        paths.prepare().unwrap();
        reserve(&paths, &[]).unwrap();
        let token = super::reserved_handoff_token(&paths).unwrap().unwrap();
        let coordination = super::coordination_directory_path(&paths, &token);
        let external = temporary.path().join("external-coordination");
        std::fs::create_dir(&external).unwrap();
        create_directory_link(&external, &coordination).unwrap();

        let error = super::run_prepared_removal(&paths).await.unwrap_err();

        assert_eq!(error, super::DataRemovalError::UnsafeTarget);
        assert!(paths.data_directory.exists());
        remove_directory_link(&coordination).unwrap();
    }

    #[tokio::test]
    async fn partial_failure_is_redacted_and_a_later_pass_finishes_cleanup() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::discover(Some(&temporary.path().join("state"))).unwrap();
        paths.prepare().unwrap();
        std::fs::write(paths.data_directory.join("remove-me"), b"data").unwrap();
        let plan = DataRemovalPlan::prepare(&paths, DATA_REMOVAL_CONFIRMATION, &[]).unwrap();
        std::fs::write(
            paths
                .config_directory
                .join(crate::runtime::OWNERSHIP_MARKER),
            b"tampered",
        )
        .unwrap();

        let error = plan.execute().await.unwrap_err();
        assert_eq!(error, super::DataRemovalError::UnownedTarget);
        assert!(
            !error
                .to_string()
                .contains(&temporary.path().display().to_string())
        );
        assert!(!paths.data_directory.exists());

        std::fs::write(
            paths
                .config_directory
                .join(crate::runtime::OWNERSHIP_MARKER),
            sylvops_core::APPLICATION_ID,
        )
        .unwrap();
        plan.execute().await.unwrap();
        assert!(!paths.config_directory.exists());
        assert!(!paths.runtime_directory.exists());
    }

    #[tokio::test]
    async fn durable_reservation_recovers_after_an_earlier_root_was_removed() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::discover(Some(&temporary.path().join("state"))).unwrap();
        paths.prepare().unwrap();
        reserve(&paths, &[]).unwrap();
        std::fs::remove_dir_all(&paths.data_directory).unwrap();
        assert!(!paths.data_directory.exists());

        complete_prepared_removal(&paths, DATA_REMOVAL_CONFIRMATION)
            .await
            .unwrap();
        assert!(!paths.config_directory.exists());
    }

    #[tokio::test]
    async fn helper_failure_retains_a_categorical_redacted_audit_record() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::discover(Some(&temporary.path().join("state"))).unwrap();
        paths.prepare().unwrap();
        reserve(&paths, &[]).unwrap();
        for directory in super::owned_directories(&paths) {
            std::fs::write(
                directory.join(crate::runtime::OWNERSHIP_MARKER),
                b"tampered",
            )
            .unwrap();
        }

        let error = super::run_prepared_removal(&paths).await.unwrap_err();
        let audit = super::failure_audit(&paths)
            .unwrap()
            .expect("failure audit");

        assert_eq!(error, super::DataRemovalError::UnownedTarget);
        assert_eq!(audit.action, "user_data_removal_completed");
        assert_eq!(audit.outcome, "failed");
        assert_eq!(audit.reason, "unowned_target");
        assert!(
            super::owned_directories(&paths)
                .iter()
                .all(|directory| !directory.join(super::DATA_REMOVAL_AUDIT_FILE).exists())
        );
        assert_eq!(
            super::wait_for_removal_completion(&paths)
                .await
                .unwrap_err(),
            super::DataRemovalError::RemovalFailed
        );
        assert!(
            !serde_json::to_string(&audit)
                .unwrap()
                .contains(&temporary.path().display().to_string())
        );
    }

    #[test]
    fn final_directory_failure_restores_ownership_and_reservation_markers() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::discover(Some(&temporary.path().join("state"))).unwrap();
        paths.prepare().unwrap();
        reserve(&paths, &[]).unwrap();
        std::fs::write(paths.data_directory.join("concurrent-content"), b"keep").unwrap();
        let directory =
            cap_std::fs::Dir::open_ambient_dir(&paths.data_directory, cap_std::ambient_authority())
                .unwrap();

        assert_eq!(
            super::finalize_empty_directory(directory, &paths.data_directory).unwrap_err(),
            super::DataRemovalError::RemovalFailed
        );
        assert_eq!(
            std::fs::read_to_string(paths.data_directory.join(crate::runtime::OWNERSHIP_MARKER))
                .unwrap(),
            sylvops_core::APPLICATION_ID
        );
        assert!(super::reservation_pending(&paths).unwrap());
    }

    #[tokio::test]
    async fn removal_never_follows_a_directory_link_escape() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::discover(Some(&temporary.path().join("state"))).unwrap();
        paths.prepare().unwrap();
        let external = temporary.path().join("external");
        std::fs::create_dir(&external).unwrap();
        std::fs::write(external.join("keep.txt"), b"keep").unwrap();
        create_directory_link(&external, &paths.data_directory.join("escape")).unwrap();

        DataRemovalPlan::prepare(&paths, DATA_REMOVAL_CONFIRMATION, &[])
            .unwrap()
            .execute()
            .await
            .unwrap();

        assert_eq!(std::fs::read(external.join("keep.txt")).unwrap(), b"keep");
    }

    #[test]
    fn removal_refuses_a_link_or_reparse_point_root() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::discover(Some(&temporary.path().join("state"))).unwrap();
        paths.prepare().unwrap();
        let real_data = temporary.path().join("real-data");
        std::fs::rename(&paths.data_directory, &real_data).unwrap();
        create_directory_link(&real_data, &paths.data_directory).unwrap();

        assert!(DataRemovalPlan::prepare(&paths, DATA_REMOVAL_CONFIRMATION, &[]).is_err());
        assert!(real_data.join(crate::runtime::OWNERSHIP_MARKER).exists());
    }

    #[tokio::test]
    async fn completion_waits_for_the_daemon_to_release_its_runtime_token() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::discover(Some(&temporary.path().join("state"))).unwrap();
        paths.prepare().unwrap();
        std::fs::write(&paths.authentication_token, b"daemon-running").unwrap();
        let token = paths.authentication_token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            std::fs::remove_file(token).unwrap();
        });

        wait_for_daemon_stop(&paths).await.unwrap();
    }

    #[test]
    fn observer_deadline_exceeds_the_helpers_complete_bounded_runtime() {
        assert!(
            super::REMOVAL_OBSERVATION_TIMEOUT.as_secs()
                > super::DAEMON_STOP_TIMEOUT.as_secs() * 2
                    + super::REMOVAL_COMPLETION_TIMEOUT.as_secs()
        );
    }

    #[test]
    fn removal_refuses_broad_or_unowned_paths() {
        let temporary = tempfile::tempdir().unwrap();
        let mut paths = RuntimePaths::discover(Some(&temporary.path().join("state"))).unwrap();
        paths.prepare().unwrap();
        paths.data_directory = if cfg!(windows) {
            PathBuf::from(r"C:\")
        } else {
            PathBuf::from("/")
        };

        assert!(DataRemovalPlan::prepare(&paths, DATA_REMOVAL_CONFIRMATION, &[]).is_err());
    }

    #[test]
    fn removal_refuses_a_hard_linked_ownership_marker() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::discover(Some(&temporary.path().join("state"))).unwrap();
        paths.prepare().unwrap();
        let external_marker = temporary.path().join("external-marker");
        std::fs::write(&external_marker, sylvops_core::APPLICATION_ID).unwrap();
        std::fs::remove_file(paths.data_directory.join(crate::runtime::OWNERSHIP_MARKER)).unwrap();
        std::fs::hard_link(
            &external_marker,
            paths.data_directory.join(crate::runtime::OWNERSHIP_MARKER),
        )
        .unwrap();

        assert!(DataRemovalPlan::prepare(&paths, DATA_REMOVAL_CONFIRMATION, &[]).is_err());
        assert_eq!(
            std::fs::read_to_string(external_marker).unwrap(),
            sylvops_core::APPLICATION_ID
        );
    }

    #[cfg(unix)]
    fn create_directory_link(
        target: &std::path::Path,
        link: &std::path::Path,
    ) -> std::io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }

    #[cfg(windows)]
    fn create_directory_link(
        target: &std::path::Path,
        link: &std::path::Path,
    ) -> std::io::Result<()> {
        junction::create(target, link)
    }

    #[cfg(unix)]
    fn remove_directory_link(link: &std::path::Path) -> std::io::Result<()> {
        std::fs::remove_file(link)
    }

    #[cfg(windows)]
    fn remove_directory_link(link: &std::path::Path) -> std::io::Result<()> {
        junction::delete(link)
    }

    #[cfg(unix)]
    #[test]
    fn removal_refuses_a_linked_ownership_marker() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::discover(Some(&temporary.path().join("state"))).unwrap();
        paths.prepare().unwrap();
        let external_marker = temporary.path().join("external-marker");
        std::fs::write(&external_marker, sylvops_core::APPLICATION_ID).unwrap();
        std::fs::remove_file(paths.data_directory.join(crate::runtime::OWNERSHIP_MARKER)).unwrap();
        symlink(
            &external_marker,
            paths.data_directory.join(crate::runtime::OWNERSHIP_MARKER),
        )
        .unwrap();

        assert!(DataRemovalPlan::prepare(&paths, DATA_REMOVAL_CONFIRMATION, &[]).is_err());
        assert_eq!(
            std::fs::read_to_string(external_marker).unwrap(),
            sylvops_core::APPLICATION_ID
        );
    }
}
