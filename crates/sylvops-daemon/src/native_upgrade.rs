//! Detached native package application, health verification, and one-shot rollback.

use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sylvops_core::{
    protocol::{ClientRequest, DaemonResponse, PROTOCOL_MAJOR},
    upgrade::{
        InstallerKind, MAX_UPGRADE_CLIENT_PROCESSES, NativeUpgradeHandoff, NativeUpgradeOutcome,
        ReleaseValidationContext, SignedReleaseMetadata,
    },
};

use crate::{
    DaemonError, Result,
    client::DaemonClient,
    runtime::RuntimePaths,
    upgrade::{embedded_verifying_key, read_bounded_file, verify_payload},
};

const MAX_HANDOFF_BYTES: u64 = 64 * 1024;
const MAX_ATTEMPT_BYTES: u64 = 16 * 1024;
const STOP_TIMEOUT: Duration = Duration::from_secs(20);
const CLIENT_EXIT_TIMEOUT: Duration = Duration::from_secs(20);
const HEALTH_TIMEOUT: Duration = Duration::from_secs(20);
const DESKTOP_START_TIMEOUT: Duration = Duration::from_secs(2);
const IPC_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(2);
const ROLLBACK_RETRY_TIMEOUT: Duration = Duration::from_secs(20);
const NATIVE_COMMAND_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const WATCHDOG_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const MAX_VERSION_OUTPUT_BYTES: u64 = 4 * 1024;
#[cfg(target_os = "macos")]
const MAX_MACOS_SIGNATURE_OUTPUT_BYTES: u64 = 16 * 1024;
const MAX_PACKAGE_TREE_DEPTH: usize = 32;
const MAX_PACKAGE_TREE_ENTRIES: u64 = 8_192;
const MAX_PACKAGE_TREE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const ATTEMPT_FILE: &str = "native-upgrade-attempt.json";
const DATABASE_ROLLBACK_FILES: [&str; 4] = [
    "sylvops.db",
    "sylvops.db-wal",
    "sylvops.db-shm",
    "sylvops.db-journal",
];

#[cfg(target_os = "macos")]
#[link(name = "proc")]
#[allow(unsafe_code)]
unsafe extern "C" {
    fn proc_pidpath(process_id: i32, buffer: *mut std::ffi::c_void, buffer_size: u32) -> i32;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum NativeUpgradePhase {
    Preparing,
    BackupReady,
    Applying,
    Healthy,
    RollbackStarted,
    RolledBack,
    Completed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum NativeUpgradeDiagnostic {
    HelperInterrupted,
    PreparationFailed,
    InstallerFailed,
    DaemonRelaunchFailed,
    HealthCheckFailed,
    DesktopRelaunchFailed,
}

impl NativeUpgradeDiagnostic {
    const fn message(self) -> &'static str {
        match self {
            Self::HelperInterrupted => "upgrade helper was interrupted",
            Self::PreparationFailed => "upgrade preparation failed",
            Self::InstallerFailed => "native installer failed",
            Self::DaemonRelaunchFailed => "updated daemon did not relaunch",
            Self::HealthCheckFailed => "post-install health check failed",
            Self::DesktopRelaunchFailed => "updated desktop did not relaunch",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeUpgradeAttempt {
    schema_version: u16,
    target_version: String,
    phase: NativeUpgradePhase,
    rollback_attempts: u8,
    diagnostic: Option<NativeUpgradeDiagnostic>,
    backup_sha256: Option<String>,
}

impl NativeUpgradeAttempt {
    fn new(target_version: String) -> Self {
        Self {
            schema_version: 1,
            target_version,
            phase: NativeUpgradePhase::Preparing,
            rollback_attempts: 0,
            diagnostic: None,
            backup_sha256: None,
        }
    }
}

/// Applies one daemon-prepared update from a detached helper process.
///
/// # Errors
///
/// Refuses unowned paths, modified metadata or payloads, failed native installers, and unhealthy
/// replacements. An unhealthy replacement is rolled back exactly once before an error is returned.
pub async fn run(handoff_path: &Path) -> Result<()> {
    let handoff = read_handoff(handoff_path)?;
    let paths = RuntimePaths::from_explicit_directories(
        handoff.data_directory.clone(),
        handoff.config_directory.clone(),
        handoff.runtime_directory.clone(),
    )?;
    validate_handoff_path(handoff_path, &handoff)?;
    refuse_repeated_attempt(&handoff)?;
    wait_for_daemon_stop(&paths).await?;
    if let Err(error) = wait_for_client_processes_exit(&handoff.client_process_ids).await {
        start_installed_daemon(&handoff)?;
        report_outcome(
            &paths,
            &handoff.release.target_version,
            NativeUpgradeOutcome::RolledBack,
        )
        .await?;
        return Err(DaemonError::Lifecycle(format!(
            "application upgrade was cancelled before package replacement: {error}"
        )));
    }
    let mut attempt = NativeUpgradeAttempt::new(handoff.release.target_version.clone());
    write_attempt(&handoff, &attempt)?;
    let (payload, backup) = match prepare_application_replacement(&handoff).await {
        Ok(prepared) => prepared,
        Err(_error) => {
            cancel_before_replacement(
                &paths,
                &handoff,
                &mut attempt,
                NativeUpgradeDiagnostic::PreparationFailed,
            )
            .await?;
            return Err(DaemonError::Lifecycle(format!(
                "application upgrade preparation failed after quiescing: {}",
                NativeUpgradeDiagnostic::PreparationFailed.message()
            )));
        }
    };
    attempt.phase = NativeUpgradePhase::BackupReady;
    attempt.backup_sha256 = Some(backup.sha256.clone());
    write_attempt(&handoff, &attempt)?;
    if let Err(error) = spawn_watchdog(handoff_path, &backup.sha256) {
        cancel_before_replacement(
            &paths,
            &handoff,
            &mut attempt,
            NativeUpgradeDiagnostic::PreparationFailed,
        )
        .await?;
        return Err(DaemonError::Lifecycle(format!(
            "application upgrade watchdog did not start: {error}"
        )));
    }
    apply_and_verify_replacement(&paths, &handoff, &payload, &backup, &mut attempt).await
}

async fn apply_and_verify_replacement(
    paths: &RuntimePaths,
    handoff: &NativeUpgradeHandoff,
    payload: &Path,
    backup: &Backup,
    attempt: &mut NativeUpgradeAttempt,
) -> Result<()> {
    attempt.phase = NativeUpgradePhase::Applying;
    write_attempt(handoff, attempt)?;
    if apply_package(handoff, payload).is_err() {
        tracing::warn!(
            diagnostic = NativeUpgradeDiagnostic::InstallerFailed.message(),
            "native application installer failed"
        );
        return rollback_and_restart(
            paths,
            handoff,
            backup,
            NativeUpgradeDiagnostic::InstallerFailed,
        )
        .await;
    }
    if start_installed_daemon(handoff).is_err() {
        tracing::warn!(
            diagnostic = NativeUpgradeDiagnostic::DaemonRelaunchFailed.message(),
            "updated application daemon did not relaunch"
        );
        return rollback_and_restart(
            paths,
            handoff,
            backup,
            NativeUpgradeDiagnostic::DaemonRelaunchFailed,
        )
        .await;
    }
    if verify_health(paths, handoff).await.is_err() {
        tracing::warn!(
            diagnostic = NativeUpgradeDiagnostic::HealthCheckFailed.message(),
            "updated application failed its health check"
        );
        return rollback_and_restart(
            paths,
            handoff,
            backup,
            NativeUpgradeDiagnostic::HealthCheckFailed,
        )
        .await;
    }
    attempt.phase = NativeUpgradePhase::Healthy;
    write_attempt(handoff, attempt)?;
    if handoff.relaunch_desktop && relaunch_desktop(handoff).is_err() {
        tracing::warn!(
            diagnostic = NativeUpgradeDiagnostic::DesktopRelaunchFailed.message(),
            "updated application desktop did not relaunch"
        );
        return rollback_and_restart(
            paths,
            handoff,
            backup,
            NativeUpgradeDiagnostic::DesktopRelaunchFailed,
        )
        .await;
    }
    report_outcome(
        paths,
        &handoff.release.target_version,
        NativeUpgradeOutcome::Installed,
    )
    .await?;
    attempt.phase = NativeUpgradePhase::Completed;
    write_attempt(handoff, attempt)?;
    remove_backup(handoff)?;
    Ok(())
}

async fn prepare_application_replacement(
    handoff: &NativeUpgradeHandoff,
) -> Result<(PathBuf, Backup)> {
    let payload = validate_staged_release(handoff).await?;
    verify_staged_package_signature(&payload, handoff.release.target.installer)?;
    let backup = create_backup(handoff)?;
    Ok((payload, backup))
}

async fn cancel_before_replacement(
    paths: &RuntimePaths,
    handoff: &NativeUpgradeHandoff,
    attempt: &mut NativeUpgradeAttempt,
    diagnostic: NativeUpgradeDiagnostic,
) -> Result<()> {
    start_installed_daemon(handoff)?;
    report_outcome(
        paths,
        &handoff.release.target_version,
        NativeUpgradeOutcome::RolledBack,
    )
    .await?;
    if handoff.relaunch_desktop {
        relaunch_desktop(handoff)?;
    }
    attempt.phase = NativeUpgradePhase::RolledBack;
    attempt.diagnostic = Some(diagnostic);
    write_attempt(handoff, attempt)
}

/// Watches one detached native helper and restores N-1 if that helper is interrupted.
///
/// # Errors
///
/// Returns an error when the handoff or attempt journal is invalid, the supervisor does not exit
/// within the bounded deadline, or the one allowed recovery attempt cannot complete.
pub async fn watch(
    handoff_path: &Path,
    supervisor_process_id: u32,
    expected_backup_sha256: &str,
) -> Result<()> {
    if supervisor_process_id == 0
        || expected_backup_sha256.len() != 64
        || !expected_backup_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(DaemonError::Lifecycle(
            "upgrade watchdog arguments are invalid".into(),
        ));
    }
    let deadline = tokio::time::Instant::now() + WATCHDOG_TIMEOUT;
    while process_is_running(supervisor_process_id)? && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if process_is_running(supervisor_process_id)? {
        return Err(DaemonError::Lifecycle(
            "upgrade helper watchdog timed out".into(),
        ));
    }
    if !handoff_path.exists() {
        return Ok(());
    }
    let handoff = read_handoff(handoff_path)?;
    let paths = RuntimePaths::from_explicit_directories(
        handoff.data_directory.clone(),
        handoff.config_directory.clone(),
        handoff.runtime_directory.clone(),
    )?;
    validate_handoff_path(handoff_path, &handoff)?;
    let attempt = read_attempt(&handoff)?;
    validate_attempt(&handoff, &attempt)?;
    if attempt.backup_sha256.as_deref() != Some(expected_backup_sha256) {
        return Err(DaemonError::Lifecycle(
            "upgrade watchdog backup identity does not match the attempt".into(),
        ));
    }
    match attempt.phase {
        NativeUpgradePhase::Completed | NativeUpgradePhase::RolledBack => Ok(()),
        NativeUpgradePhase::Preparing => Err(DaemonError::Lifecycle(
            "upgrade watchdog observed an incomplete backup".into(),
        )),
        NativeUpgradePhase::Healthy => {
            if verify_health(&paths, &handoff).await.is_ok() {
                if handoff.relaunch_desktop && relaunch_desktop(&handoff).is_err() {
                    let backup = existing_backup(&handoff, expected_backup_sha256)?;
                    return perform_rollback(
                        &paths,
                        &handoff,
                        &backup,
                        NativeUpgradeDiagnostic::DesktopRelaunchFailed,
                    )
                    .await;
                }
                report_outcome(
                    &paths,
                    &handoff.release.target_version,
                    NativeUpgradeOutcome::Installed,
                )
                .await?;
                let mut completed = attempt;
                completed.phase = NativeUpgradePhase::Completed;
                write_attempt(&handoff, &completed)?;
                remove_backup(&handoff)?;
                Ok(())
            } else {
                let backup = existing_backup(&handoff, expected_backup_sha256)?;
                perform_rollback(
                    &paths,
                    &handoff,
                    &backup,
                    NativeUpgradeDiagnostic::HelperInterrupted,
                )
                .await
            }
        }
        NativeUpgradePhase::BackupReady | NativeUpgradePhase::Applying => {
            let backup = existing_backup(&handoff, expected_backup_sha256)?;
            perform_rollback(
                &paths,
                &handoff,
                &backup,
                NativeUpgradeDiagnostic::HelperInterrupted,
            )
            .await
        }
        NativeUpgradePhase::RollbackStarted => {
            let backup = existing_backup(&handoff, expected_backup_sha256)?;
            complete_rollback(&paths, &handoff, &backup, attempt).await
        }
    }
}

async fn rollback_and_restart(
    paths: &RuntimePaths,
    handoff: &NativeUpgradeHandoff,
    backup: &Backup,
    diagnostic: NativeUpgradeDiagnostic,
) -> Result<()> {
    perform_rollback(paths, handoff, backup, diagnostic).await?;
    Err(DaemonError::Lifecycle(format!(
        "application upgrade failed and the previous version was restored: {}",
        diagnostic.message()
    )))
}

async fn perform_rollback(
    paths: &RuntimePaths,
    handoff: &NativeUpgradeHandoff,
    backup: &Backup,
    diagnostic: NativeUpgradeDiagnostic,
) -> Result<()> {
    verify_backup_before_restore(handoff, backup)?;
    let mut attempt = read_attempt(handoff)?;
    validate_attempt(handoff, &attempt)?;
    if attempt.rollback_attempts != 0
        || matches!(
            attempt.phase,
            NativeUpgradePhase::RollbackStarted | NativeUpgradePhase::RolledBack
        )
    {
        return Err(DaemonError::Lifecycle(
            "automatic rollback was already attempted".into(),
        ));
    }
    attempt.phase = NativeUpgradePhase::RollbackStarted;
    attempt.rollback_attempts = 1;
    attempt.diagnostic = Some(diagnostic);
    write_attempt(handoff, &attempt)?;
    complete_rollback(paths, handoff, backup, attempt).await
}

async fn complete_rollback(
    paths: &RuntimePaths,
    handoff: &NativeUpgradeHandoff,
    backup: &Backup,
    mut attempt: NativeUpgradeAttempt,
) -> Result<()> {
    verify_backup_before_restore(handoff, backup)?;
    if attempt.phase != NativeUpgradePhase::RollbackStarted || attempt.rollback_attempts != 1 {
        return Err(DaemonError::Lifecycle(
            "rollback resumption state is invalid".into(),
        ));
    }
    stop_daemon_if_running(paths).await;
    wait_for_daemon_stop(paths).await?;
    restore_backup_before_deadline(handoff, backup).await?;
    verify_restored_package(handoff)?;
    start_installed_daemon(handoff)?;
    if handoff.relaunch_desktop {
        relaunch_desktop(handoff)?;
    }
    report_outcome(
        paths,
        &handoff.release.target_version,
        NativeUpgradeOutcome::RolledBack,
    )
    .await?;
    attempt.phase = NativeUpgradePhase::RolledBack;
    write_attempt(handoff, &attempt)?;
    Ok(())
}

fn verify_backup_before_restore(handoff: &NativeUpgradeHandoff, backup: &Backup) -> Result<()> {
    if backup_tree_sha256(handoff)? != backup.sha256 {
        return Err(DaemonError::Lifecycle(
            "rollback package integrity check failed".into(),
        ));
    }
    verify_backup_signature(handoff, backup)
}

fn verify_restored_package(handoff: &NativeUpgradeHandoff) -> Result<()> {
    verify_application_signature(
        &handoff.installed_executable,
        handoff.release.target.installer,
    )?;
    #[cfg(windows)]
    if handoff.release.target.installer == InstallerKind::WindowsNsis {
        verify_application_signature(
            &windows_install_path(handoff)?.join("uninstall.exe"),
            InstallerKind::WindowsNsis,
        )?;
    }
    Ok(())
}

async fn restore_backup_before_deadline(
    handoff: &NativeUpgradeHandoff,
    backup: &Backup,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + ROLLBACK_RETRY_TIMEOUT;
    loop {
        match restore_backup(handoff, backup) {
            Ok(()) => return Ok(()),
            Err(_error) if tokio::time::Instant::now() < deadline => {
                tracing::warn!(
                    diagnostic = "rollback_target_still_locked",
                    "waiting to restore the previous application package"
                );
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Err(error) => return Err(error),
        }
    }
}

fn spawn_watchdog(handoff_path: &Path, backup_sha256: &str) -> Result<()> {
    let executable = std::env::current_exe().map_err(|error| {
        DaemonError::Lifecycle(format!("upgrade helper executable is unavailable: {error}"))
    })?;
    crate::background_process::command(executable)
        .arg("update-watchdog")
        .arg("--handoff")
        .arg(handoff_path)
        .arg("--supervisor-process-id")
        .arg(std::process::id().to_string())
        .arg("--backup-sha256")
        .arg(backup_sha256)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| {
            DaemonError::Lifecycle(format!("upgrade watchdog did not start: {error}"))
        })?;
    Ok(())
}

fn attempt_path(handoff: &NativeUpgradeHandoff) -> PathBuf {
    handoff.staging_root.join(ATTEMPT_FILE)
}

fn write_attempt(handoff: &NativeUpgradeHandoff, attempt: &NativeUpgradeAttempt) -> Result<()> {
    validate_attempt(handoff, attempt)?;
    let encoded = serde_json::to_vec(attempt)
        .map_err(|_| DaemonError::Lifecycle("upgrade attempt could not be encoded".into()))?;
    if encoded.len() > usize::try_from(MAX_ATTEMPT_BYTES).unwrap_or(usize::MAX) {
        return Err(DaemonError::Lifecycle(
            "upgrade attempt exceeded its byte limit".into(),
        ));
    }
    crate::atomic_file::write(&attempt_path(handoff), &encoded)
}

fn read_attempt(handoff: &NativeUpgradeHandoff) -> Result<NativeUpgradeAttempt> {
    let path = attempt_path(handoff);
    let metadata = fs::symlink_metadata(&path)?;
    if metadata.file_type().is_symlink() || metadata.len() > MAX_ATTEMPT_BYTES {
        return Err(DaemonError::Lifecycle(
            "upgrade attempt journal is invalid".into(),
        ));
    }
    let mut encoded = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    fs::File::open(path)?
        .take(MAX_ATTEMPT_BYTES.saturating_add(1))
        .read_to_end(&mut encoded)?;
    if u64::try_from(encoded.len()).unwrap_or(u64::MAX) > MAX_ATTEMPT_BYTES {
        return Err(DaemonError::Lifecycle(
            "upgrade attempt journal is oversized".into(),
        ));
    }
    serde_json::from_slice(&encoded)
        .map_err(|_| DaemonError::Lifecycle("upgrade attempt journal is malformed".into()))
}

fn validate_attempt(handoff: &NativeUpgradeHandoff, attempt: &NativeUpgradeAttempt) -> Result<()> {
    if attempt.schema_version != 1 || attempt.target_version != handoff.release.target_version {
        return Err(DaemonError::Lifecycle(
            "upgrade attempt journal does not match the handoff".into(),
        ));
    }
    if attempt.rollback_attempts > 1 {
        return Err(DaemonError::Lifecycle(
            "upgrade attempt journal has an invalid rollback count".into(),
        ));
    }
    if let Some(sha256) = &attempt.backup_sha256
        && (sha256.len() != 64 || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        return Err(DaemonError::Lifecycle(
            "upgrade attempt journal has an invalid backup identity".into(),
        ));
    }
    Ok(())
}

fn refuse_repeated_attempt(handoff: &NativeUpgradeHandoff) -> Result<()> {
    let path = attempt_path(handoff);
    if !path.exists() {
        return Ok(());
    }
    let attempt = read_attempt(handoff)?;
    if attempt.target_version != handoff.release.target_version {
        return Ok(());
    }
    validate_attempt(handoff, &attempt)?;
    if attempt.rollback_attempts != 0
        || matches!(
            attempt.phase,
            NativeUpgradePhase::RollbackStarted | NativeUpgradePhase::RolledBack
        )
    {
        return Err(DaemonError::Lifecycle(
            "automatic rollback was already attempted for this upgrade".into(),
        ));
    }
    Err(DaemonError::Lifecycle(
        "this native upgrade attempt is already in progress or complete".into(),
    ))
}

fn read_handoff(path: &Path) -> Result<NativeUpgradeHandoff> {
    let metadata = fs::metadata(path)?;
    if metadata.len() > MAX_HANDOFF_BYTES || !path.is_absolute() {
        return Err(DaemonError::Lifecycle("upgrade handoff is invalid".into()));
    }
    if fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(DaemonError::Lifecycle("upgrade handoff is a link".into()));
    }
    let mut encoded = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    fs::File::open(path)?
        .take(MAX_HANDOFF_BYTES.saturating_add(1))
        .read_to_end(&mut encoded)?;
    if u64::try_from(encoded.len()).unwrap_or(u64::MAX) > MAX_HANDOFF_BYTES {
        return Err(DaemonError::Lifecycle(
            "upgrade handoff is oversized".into(),
        ));
    }
    serde_json::from_slice(&encoded)
        .map_err(|_| DaemonError::Lifecycle("upgrade handoff is malformed".into()))
}

fn validate_handoff_path(path: &Path, handoff: &NativeUpgradeHandoff) -> Result<()> {
    let root = fs::canonicalize(&handoff.staging_root)?;
    let parent = fs::canonicalize(
        path.parent()
            .ok_or_else(|| DaemonError::Lifecycle("upgrade handoff has no parent".into()))?,
    )?;
    if root != parent || root != fs::canonicalize(handoff.data_directory.join("upgrades"))? {
        return Err(DaemonError::Lifecycle(
            "upgrade handoff escaped the owned staging directory".into(),
        ));
    }
    Ok(())
}

async fn validate_staged_release(handoff: &NativeUpgradeHandoff) -> Result<PathBuf> {
    let manifest = read_bounded_file(
        &handoff.staging_root.join("release.json"),
        sylvops_core::upgrade::MAX_RELEASE_METADATA_BYTES,
    )
    .await?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| DaemonError::Lifecycle("system clock is invalid".into()))?
        .as_secs()
        .try_into()
        .map_err(|_| DaemonError::Lifecycle("system clock is out of range".into()))?;
    let release = SignedReleaseMetadata::decode_and_validate(
        &manifest,
        &embedded_verifying_key()?,
        &ReleaseValidationContext {
            current_version: env!("CARGO_PKG_VERSION").into(),
            expected_target: handoff.release.target,
            now_unix_seconds: now,
            oldest_allowed_publication: now.saturating_sub(180 * 24 * 60 * 60),
            newest_seen_publication: None,
        },
    )
    .map_err(crate::upgrade::UpgradeError::from)?;
    if release != handoff.release {
        return Err(DaemonError::Lifecycle(
            "upgrade handoff does not match signed metadata".into(),
        ));
    }
    let payload = handoff.staging_root.join("payload.staged");
    verify_payload(&payload, &release).await?;
    Ok(payload)
}

async fn wait_for_daemon_stop(paths: &RuntimePaths) -> Result<()> {
    let deadline = tokio::time::Instant::now() + STOP_TIMEOUT;
    while tokio::fs::try_exists(&paths.authentication_token).await?
        && tokio::time::Instant::now() < deadline
    {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if tokio::fs::try_exists(&paths.authentication_token).await? {
        return Err(DaemonError::Lifecycle(
            "daemon did not stop before application replacement".into(),
        ));
    }
    Ok(())
}

async fn wait_for_client_processes_exit(process_ids: &[u32]) -> Result<()> {
    if process_ids.is_empty()
        || process_ids.len() > MAX_UPGRADE_CLIENT_PROCESSES
        || process_ids.contains(&0)
    {
        return Err(DaemonError::Lifecycle(
            "upgrade client process list is invalid".into(),
        ));
    }
    let deadline = tokio::time::Instant::now() + CLIENT_EXIT_TIMEOUT;
    while client_process_is_running(process_ids)? && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if client_process_is_running(process_ids)? {
        return Err(DaemonError::Lifecycle(
            "connected applications did not exit before package replacement".into(),
        ));
    }
    Ok(())
}

fn client_process_is_running(process_ids: &[u32]) -> Result<bool> {
    for process_id in process_ids {
        if process_is_running(*process_id)? {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(unix)]
fn process_is_running(process_id: u32) -> Result<bool> {
    let process_id = i32::try_from(process_id)
        .map_err(|_| DaemonError::Lifecycle("requesting process ID is invalid".into()))?;
    match nix::sys::signal::kill(nix::unistd::Pid::from_raw(process_id), None) {
        Ok(()) | Err(nix::errno::Errno::EPERM) => Ok(true),
        Err(nix::errno::Errno::ESRCH) => Ok(false),
        Err(error) => Err(DaemonError::Lifecycle(format!(
            "requesting process could not be inspected: {error}"
        ))),
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn process_is_running(process_id: u32) -> Result<bool> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

    use windows_sys::Win32::{
        Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT},
        System::Threading::{OpenProcess, WaitForSingleObject},
    };

    const PROCESS_SYNCHRONIZE: u32 = 0x0010_0000;
    // SAFETY: the returned handle is checked and transferred into `OwnedHandle` exactly once.
    let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, process_id) };
    if process.is_null() {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(87) {
            return Ok(false);
        }
        return Err(DaemonError::Lifecycle(format!(
            "requesting process could not be inspected: {error}"
        )));
    }
    // SAFETY: `process` is a valid owned handle and is transferred exactly once.
    let process = unsafe { OwnedHandle::from_raw_handle(process) };
    // SAFETY: the process handle grants SYNCHRONIZE and the zero timeout is valid.
    match unsafe { WaitForSingleObject(process.as_raw_handle(), 0) } {
        WAIT_OBJECT_0 => Ok(false),
        WAIT_TIMEOUT => Ok(true),
        _ => Err(DaemonError::Lifecycle(
            "requesting process wait failed".into(),
        )),
    }
}

#[derive(Debug)]
struct Backup {
    payload: PathBuf,
    sha256: String,
}

fn create_backup(handoff: &NativeUpgradeHandoff) -> Result<Backup> {
    let rollback = handoff.staging_root.join("rollback");
    if fs::symlink_metadata(&rollback).is_ok() {
        let existing = canonical_rollback_root(handoff)?;
        fs::remove_dir_all(existing)?;
    }
    fs::create_dir(&rollback)?;
    let payload = match handoff.release.target.installer {
        InstallerKind::WindowsNsis => create_windows_backup(handoff, &rollback)?,
        InstallerKind::LinuxAppImage => {
            let target = installed_payload_path(handoff)?;
            let backup = rollback.join(target.file_name().ok_or_else(|| {
                DaemonError::Lifecycle("installed executable has no name".into())
            })?);
            fs::copy(target, &backup)?;
            backup
        }
        InstallerKind::MacosDmg => {
            let app = macos_app_root(&handoff.installed_executable)?;
            let backup = rollback.join("SylvOps.app");
            copy_macos_bundle(&app, &backup)?;
            backup
        }
        InstallerKind::LinuxDeb => create_debian_backup(&rollback)?,
    };
    create_database_backup(handoff, &rollback)?;
    let sha256 = backup_tree_sha256(handoff)?;
    Ok(Backup { payload, sha256 })
}

fn create_database_backup(handoff: &NativeUpgradeHandoff, rollback: &Path) -> Result<()> {
    let data_directory = canonical_data_directory(handoff)?;
    let mut budget = CopyBudget {
        entries: 0,
        bytes: 0,
    };
    let mut entries = Vec::new();
    collect_backup_entries(rollback, rollback, 0, &mut budget, &mut entries)?;
    budget.entries = budget.entries.saturating_add(1);
    if budget.entries > MAX_PACKAGE_TREE_ENTRIES {
        return Err(DaemonError::Lifecycle(
            "rollback package exceeds the entry limit".into(),
        ));
    }
    let database_backup = rollback.join("database");
    fs::create_dir(&database_backup)?;
    for (index, name) in DATABASE_ROLLBACK_FILES.iter().enumerate() {
        let source = data_directory.join(name);
        let Some(metadata) = symlink_metadata_if_present(&source)? else {
            if index == 0 {
                return Err(DaemonError::Lifecycle(
                    "database rollback source is unavailable".into(),
                ));
            }
            continue;
        };
        if metadata_is_link(&metadata) || !metadata.is_file() {
            return Err(DaemonError::Lifecycle(
                "database rollback source is unsafe".into(),
            ));
        }
        if fs::canonicalize(&source)?.parent() != Some(data_directory.as_path()) {
            return Err(DaemonError::Lifecycle(
                "database rollback source escaped the data directory".into(),
            ));
        }
        budget.entries = budget.entries.saturating_add(1);
        budget.bytes = budget.bytes.saturating_add(metadata.len());
        if budget.entries > MAX_PACKAGE_TREE_ENTRIES || budget.bytes > MAX_PACKAGE_TREE_BYTES {
            return Err(DaemonError::Lifecycle(
                "rollback package exceeds its copy budget".into(),
            ));
        }
        let copied = fs::copy(&source, database_backup.join(name))?;
        if copied != metadata.len() {
            return Err(DaemonError::Lifecycle(
                "database rollback source changed during backup".into(),
            ));
        }
    }
    Ok(())
}

fn restore_database_backup(handoff: &NativeUpgradeHandoff) -> Result<()> {
    let rollback = canonical_rollback_root(handoff)?;
    let requested = rollback.join("database");
    let metadata = fs::symlink_metadata(&requested)?;
    if metadata_is_link(&metadata) || !metadata.is_dir() {
        return Err(DaemonError::Lifecycle(
            "database rollback package is unsafe".into(),
        ));
    }
    let database_backup = fs::canonicalize(requested)?;
    if database_backup.parent() != Some(rollback.as_path()) {
        return Err(DaemonError::Lifecycle(
            "database rollback package escaped staging".into(),
        ));
    }
    let main_database = database_backup.join(DATABASE_ROLLBACK_FILES[0]);
    if !matches!(
        symlink_metadata_if_present(&main_database)?,
        Some(metadata) if metadata.is_file() && !metadata_is_link(&metadata)
    ) {
        return Err(DaemonError::Lifecycle(
            "database rollback package is incomplete".into(),
        ));
    }
    let data_directory = canonical_data_directory(handoff)?;
    for name in DATABASE_ROLLBACK_FILES {
        let destination = data_directory.join(name);
        if let Some(metadata) = symlink_metadata_if_present(&destination)? {
            if metadata_is_link(&metadata) || !metadata.is_file() {
                return Err(DaemonError::Lifecycle(
                    "database restore target is unsafe".into(),
                ));
            }
            fs::remove_file(&destination)?;
        }
    }
    for name in DATABASE_ROLLBACK_FILES {
        let source = database_backup.join(name);
        let Some(metadata) = symlink_metadata_if_present(&source)? else {
            continue;
        };
        if metadata_is_link(&metadata)
            || !metadata.is_file()
            || fs::canonicalize(&source)?.parent() != Some(database_backup.as_path())
        {
            return Err(DaemonError::Lifecycle(
                "database rollback entry is unsafe".into(),
            ));
        }
        let copied = fs::copy(&source, data_directory.join(name))?;
        if copied != metadata.len() {
            return Err(DaemonError::Lifecycle(
                "database rollback entry changed during restore".into(),
            ));
        }
    }
    Ok(())
}

fn canonical_data_directory(handoff: &NativeUpgradeHandoff) -> Result<PathBuf> {
    let metadata = fs::symlink_metadata(&handoff.data_directory)?;
    if metadata_is_link(&metadata) || !metadata.is_dir() {
        return Err(DaemonError::Lifecycle(
            "upgrade data directory is unsafe".into(),
        ));
    }
    fs::canonicalize(&handoff.data_directory).map_err(Into::into)
}

fn symlink_metadata_if_present(path: &Path) -> Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn existing_backup(handoff: &NativeUpgradeHandoff, expected_sha256: &str) -> Result<Backup> {
    let rollback = canonical_rollback_root(handoff)?;
    let payload = match handoff.release.target.installer {
        InstallerKind::WindowsNsis => rollback.join("windows-install"),
        InstallerKind::LinuxAppImage => rollback.join(
            handoff
                .installed_executable
                .file_name()
                .ok_or_else(|| DaemonError::Lifecycle("installed executable has no name".into()))?,
        ),
        InstallerKind::MacosDmg => rollback.join("SylvOps.app"),
        InstallerKind::LinuxDeb => fs::read_dir(&rollback)?
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path())
            .find(|path| path.extension().is_some_and(|extension| extension == "deb"))
            .ok_or_else(|| {
                DaemonError::Lifecycle("Debian rollback package is unavailable".into())
            })?,
    };
    let canonical = fs::canonicalize(&payload)?;
    if !canonical.starts_with(&rollback) {
        return Err(DaemonError::Lifecycle(
            "rollback package escaped the owned staging directory".into(),
        ));
    }
    let actual_sha256 = backup_tree_sha256(handoff)?;
    if actual_sha256 != expected_sha256 {
        return Err(DaemonError::Lifecycle(
            "rollback package integrity check failed".into(),
        ));
    }
    let backup = Backup {
        payload: canonical,
        sha256: actual_sha256,
    };
    verify_backup_signature(handoff, &backup)?;
    Ok(backup)
}

fn remove_backup(handoff: &NativeUpgradeHandoff) -> Result<()> {
    #[cfg(target_os = "macos")]
    cleanup_macos_staging(handoff)?;
    if fs::symlink_metadata(handoff.staging_root.join("rollback")).is_ok() {
        let rollback = canonical_rollback_root(handoff)?;
        fs::remove_dir_all(rollback)?;
    }
    Ok(())
}

fn canonical_rollback_root(handoff: &NativeUpgradeHandoff) -> Result<PathBuf> {
    let staging = fs::canonicalize(&handoff.staging_root)?;
    let requested = handoff.staging_root.join("rollback");
    let metadata = fs::symlink_metadata(&requested)?;
    if metadata_is_link(&metadata) {
        return Err(DaemonError::Lifecycle(
            "rollback package path is a link".into(),
        ));
    }
    let rollback = fs::canonicalize(requested)?;
    if rollback.parent() != Some(staging.as_path()) {
        return Err(DaemonError::Lifecycle(
            "rollback package escaped the owned staging directory".into(),
        ));
    }
    Ok(rollback)
}

fn backup_tree_sha256(handoff: &NativeUpgradeHandoff) -> Result<String> {
    let root = canonical_rollback_root(handoff)?;
    let mut entries = Vec::new();
    let mut budget = CopyBudget {
        entries: 0,
        bytes: 0,
    };
    collect_backup_entries(&root, &root, 0, &mut budget, &mut entries)?;
    entries.sort();
    let mut hasher = Sha256::new();
    for path in entries {
        let metadata = fs::symlink_metadata(&path)?;
        if metadata_is_link(&metadata) {
            return Err(DaemonError::Lifecycle(
                "rollback package contains a link".into(),
            ));
        }
        let relative = path.strip_prefix(&root).map_err(|_| {
            DaemonError::Lifecycle("rollback package entry escaped its root".into())
        })?;
        let relative = path_identity_bytes(relative.as_os_str());
        hasher.update(
            u64::try_from(relative.len())
                .unwrap_or(u64::MAX)
                .to_le_bytes(),
        );
        hasher.update(relative);
        if metadata.is_dir() {
            hasher.update([0]);
        } else if metadata.is_file() {
            hasher.update([1]);
            hasher.update(metadata.len().to_le_bytes());
            let mut file = fs::File::open(path)?;
            let mut buffer = [0_u8; 8 * 1024];
            let mut observed = 0_u64;
            loop {
                let read = file.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                observed = observed.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
                if observed > metadata.len() {
                    return Err(DaemonError::Lifecycle(
                        "rollback package changed during integrity validation".into(),
                    ));
                }
                hasher.update(&buffer[..read]);
            }
            if observed != metadata.len() {
                return Err(DaemonError::Lifecycle(
                    "rollback package changed during integrity validation".into(),
                ));
            }
        } else {
            return Err(DaemonError::Lifecycle(
                "rollback package contains an unsupported entry".into(),
            ));
        }
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn collect_backup_entries(
    root: &Path,
    directory: &Path,
    depth: usize,
    budget: &mut CopyBudget,
    entries: &mut Vec<PathBuf>,
) -> Result<()> {
    if depth > MAX_PACKAGE_TREE_DEPTH {
        return Err(DaemonError::Lifecycle(
            "rollback package exceeds the depth limit".into(),
        ));
    }
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata_is_link(&metadata) {
            return Err(DaemonError::Lifecycle(
                "rollback package contains a link".into(),
            ));
        }
        if !path.starts_with(root) {
            return Err(DaemonError::Lifecycle(
                "rollback package entry escaped its root".into(),
            ));
        }
        budget.entries = budget.entries.saturating_add(1);
        if budget.entries > MAX_PACKAGE_TREE_ENTRIES {
            return Err(DaemonError::Lifecycle(
                "rollback package exceeds the entry limit".into(),
            ));
        }
        if metadata.is_file() {
            budget.bytes = budget.bytes.saturating_add(metadata.len());
            if budget.bytes > MAX_PACKAGE_TREE_BYTES {
                return Err(DaemonError::Lifecycle(
                    "rollback package exceeds the byte limit".into(),
                ));
            }
        }
        entries.push(path.clone());
        if metadata.is_dir() {
            collect_backup_entries(root, &path, depth.saturating_add(1), budget, entries)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn path_identity_bytes(path: &std::ffi::OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_bytes().to_vec()
}

#[cfg(windows)]
fn path_identity_bytes(path: &std::ffi::OsStr) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    path.encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>()
}

#[cfg(not(windows))]
fn metadata_is_link(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
fn metadata_is_link(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

fn verify_backup_signature(handoff: &NativeUpgradeHandoff, backup: &Backup) -> Result<()> {
    match handoff.release.target.installer {
        InstallerKind::WindowsNsis => {
            verify_application_signature(
                &backup.payload.join("sylvops.exe"),
                InstallerKind::WindowsNsis,
            )?;
            verify_application_signature(
                &backup.payload.join("uninstall.exe"),
                InstallerKind::WindowsNsis,
            )
        }
        InstallerKind::MacosDmg => {
            verify_application_signature(&backup.payload, InstallerKind::MacosDmg)
        }
        InstallerKind::LinuxAppImage | InstallerKind::LinuxDeb => Ok(()),
    }
}

fn installed_payload_path(handoff: &NativeUpgradeHandoff) -> Result<&Path> {
    if !handoff.installed_executable.is_absolute()
        || fs::symlink_metadata(&handoff.installed_executable)?
            .file_type()
            .is_symlink()
    {
        return Err(DaemonError::Lifecycle(
            "installed executable path is unsafe".into(),
        ));
    }
    Ok(&handoff.installed_executable)
}

#[cfg(windows)]
fn apply_package(handoff: &NativeUpgradeHandoff, payload: &Path) -> Result<()> {
    if handoff.release.target.installer != InstallerKind::WindowsNsis {
        return Err(DaemonError::Lifecycle(
            "Windows installer kind is invalid".into(),
        ));
    }
    successful(Command::new(payload).arg("/S"), "Windows installer")
}

#[cfg(target_os = "linux")]
fn apply_package(handoff: &NativeUpgradeHandoff, payload: &Path) -> Result<()> {
    match handoff.release.target.installer {
        InstallerKind::LinuxAppImage => {
            let target = installed_payload_path(handoff)?;
            let replacement = target.with_extension("AppImage.new");
            fs::copy(payload, &replacement)?;
            fs::set_permissions(&replacement, fs::metadata(target)?.permissions())?;
            fs::rename(replacement, target)?;
            Ok(())
        }
        InstallerKind::LinuxDeb => successful(
            Command::new("pkexec")
                .args(["dpkg", "--install"])
                .arg(payload),
            "Debian installer",
        ),
        _ => Err(DaemonError::Lifecycle(
            "Linux installer kind is invalid".into(),
        )),
    }
}

#[cfg(target_os = "macos")]
fn apply_package(handoff: &NativeUpgradeHandoff, payload: &Path) -> Result<()> {
    if handoff.release.target.installer != InstallerKind::MacosDmg {
        return Err(DaemonError::Lifecycle(
            "macOS installer kind is invalid".into(),
        ));
    }
    cleanup_macos_staging(handoff)?;
    let mount = handoff.staging_root.join("mounted-update");
    fs::create_dir(&mount)?;
    successful(
        Command::new("hdiutil")
            .args(["attach", "-nobrowse", "-readonly", "-mountpoint"])
            .arg(&mount)
            .arg(payload),
        "macOS disk image mount",
    )?;
    let result = (|| {
        let applications = fs::read_dir(&mount)?
            .filter_map(std::result::Result::ok)
            .filter_map(|entry| {
                let path = entry.path();
                (path.extension().is_some_and(|extension| extension == "app")).then_some(path)
            })
            .collect::<Vec<_>>();
        let [source] = applications.as_slice() else {
            return Err(DaemonError::Lifecycle(
                "mounted update must contain exactly one application bundle".into(),
            ));
        };
        verify_application_signature(source, InstallerKind::MacosDmg)?;
        verify_macos_bundle_identity(handoff, source)?;

        let target = macos_app_root(&handoff.installed_executable)?;
        let pending = handoff.staging_root.join("pending.app");
        copy_macos_bundle(source, &pending)?;
        verify_application_signature(&pending, InstallerKind::MacosDmg)?;
        verify_macos_bundle_identity(handoff, &pending)?;
        if symlink_metadata_if_present(&target)?.is_some() {
            fs::remove_dir_all(&target)?;
        }
        if fs::rename(&pending, &target).is_err() {
            copy_macos_bundle(&pending, &target)?;
            remove_macos_staging_path(handoff, &pending)?;
        }
        Ok(())
    })();
    let detach = successful(
        Command::new("hdiutil").args(["detach"]).arg(&mount),
        "macOS disk image detach",
    );
    if symlink_metadata_if_present(&mount)?.is_some() {
        remove_macos_staging_path(handoff, &mount)?;
    }
    result?;
    detach
}

fn restore_backup(handoff: &NativeUpgradeHandoff, backup: &Backup) -> Result<()> {
    match handoff.release.target.installer {
        InstallerKind::WindowsNsis => restore_windows_backup(handoff, &backup.payload),
        InstallerKind::LinuxAppImage => {
            fs::copy(&backup.payload, installed_payload_path(handoff)?)?;
            Ok(())
        }
        InstallerKind::MacosDmg => {
            cleanup_macos_staging(handoff)?;
            let target = macos_app_root(&handoff.installed_executable)?;
            if symlink_metadata_if_present(&target)?.is_some() {
                fs::remove_dir_all(&target)?;
            }
            copy_macos_bundle(&backup.payload, &target)
        }
        InstallerKind::LinuxDeb => successful(
            Command::new("pkexec")
                .args(["dpkg", "--install"])
                .arg(&backup.payload),
            "Debian rollback installer",
        ),
    }?;
    restore_database_backup(handoff)
}

#[cfg(windows)]
fn create_windows_backup(handoff: &NativeUpgradeHandoff, rollback: &Path) -> Result<PathBuf> {
    let install_root = windows_install_root(handoff)?;
    let uninstaller = install_root.join("uninstall.exe");
    if !uninstaller.is_file() || fs::symlink_metadata(&uninstaller)?.file_type().is_symlink() {
        return Err(DaemonError::Lifecycle(
            "installed Windows package has no safe uninstaller".into(),
        ));
    }
    let package = rollback.join("windows-install");
    copy_tree(&install_root, &package)?;
    let start_menu = windows_start_menu_root()?;
    copy_tree(&start_menu, &rollback.join("windows-start-menu"))?;
    let registry = rollback.join("windows-registry");
    export_windows_registry(&registry)?;
    Ok(package)
}

#[cfg(not(windows))]
fn create_windows_backup(_handoff: &NativeUpgradeHandoff, _rollback: &Path) -> Result<PathBuf> {
    Err(DaemonError::Lifecycle(
        "Windows rollback is unavailable on this platform".into(),
    ))
}

#[cfg(windows)]
fn restore_windows_backup(handoff: &NativeUpgradeHandoff, package: &Path) -> Result<()> {
    let install_root = windows_install_path(handoff)?;
    let rollback = package
        .parent()
        .ok_or_else(|| DaemonError::Lifecycle("Windows rollback package has no parent".into()))?;
    let candidate_registry = handoff.staging_root.join("candidate-registry");
    remove_owned_staging_directory(handoff, &candidate_registry)?;
    export_windows_registry(&candidate_registry)?;

    let install_swap = WindowsTreeSwap::apply(&install_root, package, "install")?;
    let start_menu = windows_start_menu_path()?;
    let menu_swap = match WindowsTreeSwap::apply(
        &start_menu,
        &rollback.join("windows-start-menu"),
        "start-menu",
    ) {
        Ok(swap) => swap,
        Err(error) => {
            let _ = install_swap.revert();
            return Err(error);
        }
    };
    if let Err(error) = import_windows_registry(&rollback.join("windows-registry")) {
        let _ = import_windows_registry(&candidate_registry);
        let _ = menu_swap.revert();
        let _ = install_swap.revert();
        let _ = remove_owned_staging_directory(handoff, &candidate_registry);
        return Err(error);
    }
    menu_swap.commit();
    install_swap.commit();
    remove_owned_staging_directory(handoff, &candidate_registry)?;
    Ok(())
}

#[cfg(windows)]
fn remove_owned_staging_directory(handoff: &NativeUpgradeHandoff, path: &Path) -> Result<()> {
    if fs::symlink_metadata(path).is_err() {
        return Ok(());
    }
    let staging = fs::canonicalize(&handoff.staging_root)?;
    let metadata = fs::symlink_metadata(path)?;
    if metadata_is_link(&metadata) {
        return Err(DaemonError::Lifecycle(
            "Windows rollback scratch path is a link".into(),
        ));
    }
    let canonical = fs::canonicalize(path)?;
    if canonical.parent() != Some(staging.as_path()) {
        return Err(DaemonError::Lifecycle(
            "Windows rollback scratch path escaped staging".into(),
        ));
    }
    fs::remove_dir_all(canonical)?;
    Ok(())
}

#[cfg(windows)]
#[derive(Debug)]
struct WindowsTreeSwap {
    target: PathBuf,
    displaced: Option<PathBuf>,
}

#[cfg(windows)]
impl WindowsTreeSwap {
    fn apply(target: &Path, source: &Path, label: &str) -> Result<Self> {
        let parent = target
            .parent()
            .ok_or_else(|| DaemonError::Lifecycle("Windows package tree has no parent".into()))?;
        let staged = parent.join(format!(".sylvops-{label}-restore"));
        let displaced = parent.join(format!(".sylvops-{label}-candidate"));
        for stale in [&staged, &displaced] {
            if stale.exists() {
                fs::remove_dir_all(stale)?;
            }
        }
        copy_tree(source, &staged)?;
        let displaced = if target.exists() {
            fs::rename(target, &displaced)?;
            Some(displaced)
        } else {
            None
        };
        if let Err(error) = fs::rename(&staged, target) {
            if let Some(candidate) = &displaced {
                let _ = fs::rename(candidate, target);
            }
            return Err(error.into());
        }
        Ok(Self {
            target: target.to_path_buf(),
            displaced,
        })
    }

    fn revert(&self) -> Result<()> {
        if self.target.exists() {
            fs::remove_dir_all(&self.target)?;
        }
        if let Some(displaced) = &self.displaced {
            fs::rename(displaced, &self.target)?;
        }
        Ok(())
    }

    fn commit(&self) {
        if let Some(displaced) = &self.displaced {
            let _ = fs::remove_dir_all(displaced);
        }
    }
}

#[cfg(windows)]
fn export_windows_registry(directory: &Path) -> Result<()> {
    fs::create_dir(directory)?;
    for (index, key) in windows_registry_keys().iter().enumerate() {
        successful(
            Command::new("reg.exe")
                .args(["export", key])
                .arg(directory.join(format!("{index}.reg")))
                .arg("/y"),
            "Windows installation registry backup",
        )?;
    }
    Ok(())
}

#[cfg(windows)]
fn import_windows_registry(directory: &Path) -> Result<()> {
    for key in windows_registry_keys() {
        let _ = successful(
            Command::new("reg.exe").args(["delete", key, "/f"]),
            "Windows candidate registry cleanup",
        );
    }
    for index in 0..windows_registry_keys().len() {
        successful(
            Command::new("reg.exe")
                .arg("import")
                .arg(directory.join(format!("{index}.reg"))),
            "Windows installation registry restore",
        )?;
    }
    Ok(())
}

#[cfg(windows)]
fn windows_install_root(handoff: &NativeUpgradeHandoff) -> Result<PathBuf> {
    let expected = windows_install_path(handoff)?;
    let actual = fs::canonicalize(&expected)?;
    if actual != expected {
        return Err(DaemonError::Lifecycle(
            "Windows update target is outside the installed package".into(),
        ));
    }
    Ok(actual)
}

#[cfg(windows)]
fn windows_install_path(handoff: &NativeUpgradeHandoff) -> Result<PathBuf> {
    let executable = &handoff.installed_executable;
    if !executable.is_absolute()
        || (executable.exists() && fs::symlink_metadata(executable)?.file_type().is_symlink())
    {
        return Err(DaemonError::Lifecycle(
            "installed Windows executable path is unsafe".into(),
        ));
    }
    let root = executable.parent().ok_or_else(|| {
        DaemonError::Lifecycle("installed Windows executable has no parent".into())
    })?;
    let local_app_data = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .ok_or_else(|| DaemonError::Lifecycle("LOCALAPPDATA is unavailable".into()))?;
    let expected = fs::canonicalize(local_app_data)?
        .join("Programs")
        .join("SylvOps");
    let root_matches = if root.exists() {
        fs::canonicalize(root)? == expected
    } else {
        root == expected
    };
    if !root_matches {
        return Err(DaemonError::Lifecycle(
            "Windows update target is outside the installed package".into(),
        ));
    }
    Ok(expected)
}

#[cfg(windows)]
fn windows_start_menu_root() -> Result<PathBuf> {
    let expected = windows_start_menu_path()?;
    let canonical = fs::canonicalize(&expected)?;
    if canonical != expected {
        return Err(DaemonError::Lifecycle(
            "Windows Start Menu package path is unsafe".into(),
        ));
    }
    Ok(canonical)
}

#[cfg(windows)]
fn windows_start_menu_path() -> Result<PathBuf> {
    let app_data = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .ok_or_else(|| DaemonError::Lifecycle("APPDATA is unavailable".into()))?;
    let app_data = fs::canonicalize(app_data)?;
    let root = app_data
        .join("Microsoft")
        .join("Windows")
        .join("Start Menu")
        .join("Programs")
        .join("SylvOps");
    if root.file_name().is_none_or(|name| name != "SylvOps") {
        return Err(DaemonError::Lifecycle(
            "Windows Start Menu package path is unsafe".into(),
        ));
    }
    Ok(root)
}

#[cfg(not(windows))]
fn restore_windows_backup(_handoff: &NativeUpgradeHandoff, _package: &Path) -> Result<()> {
    Err(DaemonError::Lifecycle(
        "Windows rollback is unavailable on this platform".into(),
    ))
}

#[cfg(windows)]
fn windows_registry_keys() -> [&'static str; 3] {
    [
        r"HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\SylvOps",
        r"HKCU\Software\Microsoft\Windows\CurrentVersion\App Paths\sylvops.exe",
        r"HKCU\Software\devemit\SylvOps",
    ]
}

#[cfg(target_os = "linux")]
fn create_debian_backup(directory: &Path) -> Result<PathBuf> {
    successful(
        Command::new("pkexec")
            .arg("dpkg-repack")
            .arg("sylvops")
            .current_dir(directory),
        "Debian rollback package creation",
    )?;
    fs::read_dir(directory)?
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|extension| extension == "deb"))
        .ok_or_else(|| DaemonError::Lifecycle("Debian rollback package was not created".into()))
}

#[cfg(not(target_os = "linux"))]
fn create_debian_backup(_directory: &Path) -> Result<PathBuf> {
    Err(DaemonError::Lifecycle(
        "Debian rollback is unavailable on this platform".into(),
    ))
}

#[cfg(target_os = "macos")]
fn macos_app_root(executable: &Path) -> Result<PathBuf> {
    if !executable.is_absolute() {
        return Err(DaemonError::Lifecycle(
            "installed macOS bundle path is unsafe".into(),
        ));
    }
    let app = executable
        .ancestors()
        .find(|path| path.extension().is_some_and(|extension| extension == "app"))
        .map(Path::to_path_buf)
        .ok_or_else(|| DaemonError::Lifecycle("installed macOS bundle was not found".into()))?;
    let expected_executable = app.join("Contents/MacOS/sylvops");
    if executable != app && executable != expected_executable {
        return Err(DaemonError::Lifecycle(
            "installed macOS executable is outside the application entry point".into(),
        ));
    }
    if let Some(metadata) = symlink_metadata_if_present(&app)? {
        if metadata_is_link(&metadata) || !metadata.is_dir() {
            return Err(DaemonError::Lifecycle(
                "installed macOS bundle path is unsafe".into(),
            ));
        }
        return Ok(fs::canonicalize(app)?);
    }
    let parent = fs::canonicalize(
        app.parent()
            .ok_or_else(|| DaemonError::Lifecycle("macOS bundle has no parent".into()))?,
    )?;
    Ok(parent.join(
        app.file_name()
            .ok_or_else(|| DaemonError::Lifecycle("macOS bundle has no name".into()))?,
    ))
}

#[cfg(not(target_os = "macos"))]
fn macos_app_root(_executable: &Path) -> Result<PathBuf> {
    Err(DaemonError::Lifecycle(
        "macOS bundle is unavailable on this platform".into(),
    ))
}

#[cfg(target_os = "macos")]
#[derive(Debug, Eq, PartialEq)]
struct MacosCodeIdentity {
    identifier: String,
    team_identifier: String,
}

#[cfg(target_os = "macos")]
fn verify_macos_bundle_identity(handoff: &NativeUpgradeHandoff, candidate: &Path) -> Result<()> {
    let installed = macos_app_root(&handoff.installed_executable)?;
    let installed_identity = macos_code_identity(&installed)?;
    let candidate_identity = macos_code_identity(candidate)?;
    if installed_identity != candidate_identity {
        return Err(DaemonError::Lifecycle(
            "macOS update code-signing identity does not match the installation".into(),
        ));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn macos_code_identity(application: &Path) -> Result<MacosCodeIdentity> {
    let mut command = Command::new("codesign");
    command.args(["--display", "--verbose=4"]).arg(application);
    let encoded = bounded_command_stderr(
        &mut command,
        "macOS code-signing identity check",
        MAX_MACOS_SIGNATURE_OUTPUT_BYTES,
        NATIVE_COMMAND_TIMEOUT,
    )?;
    let details = std::str::from_utf8(&encoded).map_err(|_| {
        DaemonError::Lifecycle("macOS code-signing identity output is invalid".into())
    })?;
    let identifier = details
        .lines()
        .find_map(|line| line.strip_prefix("Identifier="))
        .filter(|identifier| *identifier == sylvops_core::APPLICATION_ID)
        .ok_or_else(|| {
            DaemonError::Lifecycle("macOS application bundle identity is invalid".into())
        })?;
    let team_identifier = details
        .lines()
        .find_map(|line| line.strip_prefix("TeamIdentifier="))
        .filter(|team| {
            team.len() == 10
                && team
                    .bytes()
                    .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
        })
        .ok_or_else(|| {
            DaemonError::Lifecycle("macOS Developer ID team identity is invalid".into())
        })?;
    if !details
        .lines()
        .any(|line| line.starts_with("Authority=Developer ID Application:"))
        || !details
            .lines()
            .any(|line| line.contains("flags=") && line.contains("runtime"))
    {
        return Err(DaemonError::Lifecycle(
            "macOS application is not a hardened Developer ID build".into(),
        ));
    }
    Ok(MacosCodeIdentity {
        identifier: identifier.into(),
        team_identifier: team_identifier.into(),
    })
}

#[cfg(target_os = "macos")]
fn bounded_command_stderr(
    command: &mut Command,
    operation: &str,
    byte_limit: u64,
    timeout: Duration,
) -> Result<Vec<u8>> {
    use std::os::unix::process::CommandExt;

    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = command
        .spawn()
        .map_err(|error| DaemonError::Lifecycle(format!("{operation} did not start: {error}")))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| DaemonError::Lifecycle(format!("{operation} output is unavailable")))?;
    let reader = thread::spawn(move || -> std::io::Result<(Vec<u8>, u64)> {
        let mut stderr = stderr;
        let mut retained =
            Vec::with_capacity(usize::try_from(byte_limit.min(16 * 1024)).unwrap_or(16 * 1024));
        let mut observed = 0_u64;
        let mut buffer = [0_u8; 8 * 1024];
        loop {
            let read = stderr.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            observed = observed.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
            let remaining = usize::try_from(byte_limit)
                .unwrap_or(usize::MAX)
                .saturating_sub(retained.len());
            retained.extend_from_slice(&buffer[..read.min(remaining)]);
        }
        Ok((retained, observed))
    });
    let process_id = child.id();
    let process_tree = match crate::process_tree::attach(process_id, i32::try_from(process_id).ok())
    {
        Ok(tree) => tree,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            return Err(error);
        }
    };
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|error| {
            DaemonError::Lifecycle(format!("{operation} could not be observed: {error}"))
        })? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = process_tree.terminate();
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            return Err(DaemonError::Lifecycle(format!("{operation} timed out")));
        }
        thread::sleep(Duration::from_millis(50));
    };
    process_tree.terminate()?;
    let (retained, observed) = reader
        .join()
        .map_err(|_| DaemonError::Lifecycle(format!("{operation} output reader failed")))??;
    if !status.success() {
        return Err(DaemonError::Lifecycle(format!("{operation} failed")));
    }
    if observed > byte_limit {
        return Err(DaemonError::Lifecycle(format!(
            "{operation} output exceeded its byte limit"
        )));
    }
    Ok(retained)
}

#[cfg(target_os = "macos")]
fn copy_macos_bundle(source: &Path, destination: &Path) -> Result<()> {
    if symlink_metadata_if_present(destination)?.is_some() {
        return Err(DaemonError::Lifecycle(
            "macOS bundle copy target already exists".into(),
        ));
    }
    copy_tree(source, destination)
}

#[cfg(not(target_os = "macos"))]
fn copy_macos_bundle(_source: &Path, _destination: &Path) -> Result<()> {
    Err(DaemonError::Lifecycle(
        "macOS bundle copy is unavailable on this platform".into(),
    ))
}

#[cfg(target_os = "macos")]
fn cleanup_macos_staging(handoff: &NativeUpgradeHandoff) -> Result<()> {
    let mount = handoff.staging_root.join("mounted-update");
    if let Some(metadata) = symlink_metadata_if_present(&mount)? {
        if metadata_is_link(&metadata) || !metadata.is_dir() {
            return Err(DaemonError::Lifecycle(
                "macOS disk image mount path is unsafe".into(),
            ));
        }
        let _ = successful(
            Command::new("hdiutil").args(["detach"]).arg(&mount),
            "macOS disk image detach",
        );
        if symlink_metadata_if_present(&mount)?.is_some() {
            remove_macos_staging_path(handoff, &mount)?;
        }
    }
    let pending = handoff.staging_root.join("pending.app");
    if symlink_metadata_if_present(&pending)?.is_some() {
        remove_macos_staging_path(handoff, &pending)?;
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn cleanup_macos_staging(_handoff: &NativeUpgradeHandoff) -> Result<()> {
    Err(DaemonError::Lifecycle(
        "macOS staging cleanup is unavailable on this platform".into(),
    ))
}

#[cfg(target_os = "macos")]
fn remove_macos_staging_path(handoff: &NativeUpgradeHandoff, path: &Path) -> Result<()> {
    let staging = fs::canonicalize(&handoff.staging_root)?;
    let metadata = fs::symlink_metadata(path)?;
    if metadata_is_link(&metadata) {
        return Err(DaemonError::Lifecycle(
            "macOS upgrade scratch path is a link".into(),
        ));
    }
    let canonical = fs::canonicalize(path)?;
    if canonical.parent() != Some(staging.as_path()) {
        return Err(DaemonError::Lifecycle(
            "macOS upgrade scratch path escaped staging".into(),
        ));
    }
    if metadata.is_dir() {
        fs::remove_dir_all(canonical)?;
    } else {
        fs::remove_file(canonical)?;
    }
    Ok(())
}

fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    copy_tree_with_budget(
        source,
        destination,
        0,
        &mut CopyBudget {
            entries: 0,
            bytes: 0,
        },
    )
}

#[derive(Debug)]
struct CopyBudget {
    entries: u64,
    bytes: u64,
}

fn copy_tree_with_budget(
    source: &Path,
    destination: &Path,
    depth: usize,
    budget: &mut CopyBudget,
) -> Result<()> {
    if depth > MAX_PACKAGE_TREE_DEPTH {
        return Err(DaemonError::Lifecycle(
            "package tree exceeds the nesting limit".into(),
        ));
    }
    if fs::symlink_metadata(source)?.file_type().is_symlink() {
        return Err(DaemonError::Lifecycle(
            "package tree contains a link".into(),
        ));
    }
    fs::create_dir(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        budget.entries = budget.entries.saturating_add(1);
        if budget.entries > MAX_PACKAGE_TREE_ENTRIES {
            return Err(DaemonError::Lifecycle(
                "package tree exceeds the entry limit".into(),
            ));
        }
        let target = destination.join(entry.file_name());
        if file_type.is_dir() {
            copy_tree_with_budget(&entry.path(), &target, depth.saturating_add(1), budget)?;
        } else if file_type.is_file() {
            let metadata = entry.metadata()?;
            budget.bytes = budget.bytes.saturating_add(metadata.len());
            if budget.bytes > MAX_PACKAGE_TREE_BYTES {
                return Err(DaemonError::Lifecycle(
                    "package tree exceeds the byte limit".into(),
                ));
            }
            let copied = fs::copy(entry.path(), target)?;
            if copied != metadata.len() {
                return Err(DaemonError::Lifecycle(
                    "package tree changed during copy".into(),
                ));
            }
        } else {
            return Err(DaemonError::Lifecycle(
                "package tree contains an unsupported entry".into(),
            ));
        }
    }
    Ok(())
}

fn successful(command: &mut Command, operation: &str) -> Result<()> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    successful_with_timeout(command, operation, NATIVE_COMMAND_TIMEOUT, true)
}

fn successful_with_timeout(
    command: &mut Command,
    operation: &str,
    timeout: Duration,
    own_process_tree: bool,
) -> Result<()> {
    #[cfg(unix)]
    if own_process_tree {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    let process_tree = own_process_tree
        .then(crate::process_tree::create)
        .transpose()?;
    #[cfg(windows)]
    if own_process_tree {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::CREATE_SUSPENDED;
        command.creation_flags(CREATE_SUSPENDED);
    }
    let mut child = command
        .spawn()
        .map_err(|error| DaemonError::Lifecycle(format!("{operation} did not start: {error}")))?;
    #[cfg(unix)]
    let process_tree = if own_process_tree {
        let process_id = child.id();
        match crate::process_tree::attach(process_id, i32::try_from(process_id).ok()) {
            Ok(tree) => Some(tree),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        }
    } else {
        None
    };
    #[cfg(windows)]
    if let Some(process_tree) = &process_tree
        && let Err(error) = process_tree
            .assign(&child)
            .and_then(|()| crate::process_tree::ProcessTree::resume(child.id()))
    {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().map_err(|error| {
            DaemonError::Lifecycle(format!("{operation} could not be observed: {error}"))
        })? {
            return if status.success() {
                if let Some(process_tree) = &process_tree {
                    process_tree.terminate()?;
                }
                Ok(())
            } else {
                if let Some(process_tree) = &process_tree {
                    process_tree.terminate()?;
                }
                Err(DaemonError::Lifecycle(format!("{operation} failed")))
            };
        }
        if Instant::now() >= deadline {
            if let Some(process_tree) = &process_tree {
                let _ = process_tree.terminate();
            }
            let _ = child.kill();
            let _ = child.wait();
            return Err(DaemonError::Lifecycle(format!("{operation} timed out")));
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn state_override(handoff: &NativeUpgradeHandoff) -> Option<PathBuf> {
    let data = handoff.data_directory.parent()?;
    (handoff.data_directory.file_name()? == "data"
        && handoff.config_directory == data.join("config")
        && handoff.runtime_directory == data.join("run"))
    .then(|| data.to_path_buf())
}

fn installed_command(handoff: &NativeUpgradeHandoff) -> Command {
    let mut command = Command::new(&handoff.installed_executable);
    if let Some(root) = state_override(handoff) {
        command.arg("--state-dir").arg(root);
    }
    command
}

fn start_installed_daemon(handoff: &NativeUpgradeHandoff) -> Result<()> {
    let mut command = installed_command(handoff);
    command
        .args(["daemon", "start"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    successful_with_timeout(
        &mut command,
        "installed daemon start",
        HEALTH_TIMEOUT,
        false,
    )
}

fn installed_version(handoff: &NativeUpgradeHandoff) -> Result<String> {
    let output_path = handoff.staging_root.join("version-check.txt");
    if output_path.exists() {
        fs::remove_file(&output_path)?;
    }
    let output = fs::File::create(&output_path)?;
    let mut command = installed_command(handoff);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::from(output))
        .stderr(Stdio::null());
    let result = successful_with_timeout(
        &mut command,
        "installed version check",
        HEALTH_TIMEOUT,
        true,
    );
    if result.is_ok() && fs::metadata(&output_path)?.len() > MAX_VERSION_OUTPUT_BYTES {
        let _ = fs::remove_file(&output_path);
        return Err(DaemonError::Lifecycle(
            "installed version output exceeded its byte limit".into(),
        ));
    }
    result?;
    let version = fs::read_to_string(&output_path)?;
    let _ = fs::remove_file(output_path);
    Ok(version)
}

async fn verify_health(paths: &RuntimePaths, handoff: &NativeUpgradeHandoff) -> Result<()> {
    let deadline = tokio::time::Instant::now() + HEALTH_TIMEOUT;
    loop {
        if let Some(DaemonResponse::Health(health)) =
            request_before_deadline(paths, &ClientRequest::Health, deadline).await
        {
            if health.daemon_version != handoff.release.target_version
                || health.protocol_major != PROTOCOL_MAJOR
                || !health.database_ready
            {
                return Err(DaemonError::Lifecycle(
                    "updated daemon reported incompatible health".into(),
                ));
            }
            verify_daemon_executable_identity(health.process_id, handoff)?;
            if installed_version(handoff)?.trim()
                != format!("sylvops {}", handoff.release.target_version)
            {
                return Err(DaemonError::Lifecycle(
                    "updated executable version does not match".into(),
                ));
            }
            verify_application_signature(
                &handoff.installed_executable,
                handoff.release.target.installer,
            )?;
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(DaemonError::Lifecycle(
                "updated daemon health check timed out".into(),
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn verify_daemon_executable_identity(
    process_id: u32,
    handoff: &NativeUpgradeHandoff,
) -> Result<()> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    };

    // SAFETY: the returned handle is checked and transferred into `OwnedHandle` exactly once.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) };
    if process.is_null() {
        return Err(DaemonError::Lifecycle(
            "updated daemon executable identity could not be inspected".into(),
        ));
    }
    // SAFETY: `process` is a valid owned handle and is transferred exactly once.
    let process = unsafe { OwnedHandle::from_raw_handle(process) };
    let mut executable = vec![0_u16; 32_768];
    let mut length = u32::try_from(executable.len()).expect("executable path buffer fits u32");
    // SAFETY: the process handle grants query access and the output buffer is valid for `length`
    // UTF-16 code units.
    if unsafe {
        QueryFullProcessImageNameW(
            process.as_raw_handle(),
            0,
            executable.as_mut_ptr(),
            &raw mut length,
        )
    } == 0
    {
        return Err(DaemonError::Lifecycle(
            "updated daemon executable identity could not be inspected".into(),
        ));
    }
    executable.truncate(usize::try_from(length).unwrap_or(0));
    let actual = fs::canonicalize(PathBuf::from(String::from_utf16_lossy(&executable)))?;
    let expected = fs::canonicalize(&handoff.installed_executable)?;
    if !actual
        .to_string_lossy()
        .eq_ignore_ascii_case(&expected.to_string_lossy())
    {
        return Err(DaemonError::Lifecycle(
            "updated daemon executable identity does not match the installation".into(),
        ));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn verify_daemon_executable_identity(
    process_id: u32,
    handoff: &NativeUpgradeHandoff,
) -> Result<()> {
    use std::os::unix::ffi::OsStringExt;

    const PROCESS_PATH_BYTES: usize = 4 * 1024;
    let process_id = i32::try_from(process_id)
        .map_err(|_| DaemonError::Lifecycle("updated daemon process ID is invalid".into()))?;
    let mut path = vec![0_u8; PROCESS_PATH_BYTES];
    // SAFETY: `path` is writable for the supplied byte length and `proc_pidpath` does not retain
    // the pointer after returning.
    let length = unsafe {
        proc_pidpath(
            process_id,
            path.as_mut_ptr().cast(),
            u32::try_from(path.len()).expect("process path buffer fits u32"),
        )
    };
    if length <= 0 {
        return Err(DaemonError::Lifecycle(
            "updated daemon executable identity could not be inspected".into(),
        ));
    }
    path.truncate(usize::try_from(length).unwrap_or(0));
    while path.last() == Some(&0) {
        path.pop();
    }
    let actual = fs::canonicalize(PathBuf::from(std::ffi::OsString::from_vec(path)))?;
    let expected = fs::canonicalize(&handoff.installed_executable)?;
    if actual != expected {
        return Err(DaemonError::Lifecycle(
            "updated daemon executable identity does not match the installation".into(),
        ));
    }
    Ok(())
}

#[cfg(all(not(windows), not(target_os = "macos")))]
fn verify_daemon_executable_identity(
    _process_id: u32,
    _handoff: &NativeUpgradeHandoff,
) -> Result<()> {
    Ok(())
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn verify_platform_signature(path: &Path, _installer: InstallerKind) -> Result<()> {
    use std::{mem::size_of, os::windows::ffi::OsStrExt, ptr};
    use windows_sys::Win32::Security::WinTrust::{
        WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_DATA, WINTRUST_DATA_0, WINTRUST_FILE_INFO,
        WTD_CHOICE_FILE, WTD_REVOKE_NONE, WTD_STATEACTION_CLOSE, WTD_STATEACTION_VERIFY,
        WTD_UI_NONE, WinVerifyTrust,
    };

    let wide = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let mut file = WINTRUST_FILE_INFO {
        cbStruct: u32::try_from(size_of::<WINTRUST_FILE_INFO>())
            .expect("WINTRUST_FILE_INFO size fits u32"),
        pcwszFilePath: wide.as_ptr(),
        hFile: ptr::null_mut(),
        pgKnownSubject: ptr::null_mut(),
    };
    let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    let mut data = WINTRUST_DATA {
        cbStruct: u32::try_from(size_of::<WINTRUST_DATA>()).expect("WINTRUST_DATA size fits u32"),
        dwUIChoice: WTD_UI_NONE,
        fdwRevocationChecks: WTD_REVOKE_NONE,
        dwUnionChoice: WTD_CHOICE_FILE,
        Anonymous: WINTRUST_DATA_0 {
            pFile: &raw mut file,
        },
        dwStateAction: WTD_STATEACTION_VERIFY,
        ..Default::default()
    };
    let result =
        unsafe { WinVerifyTrust(ptr::null_mut(), &raw mut action, (&raw mut data).cast()) };
    data.dwStateAction = WTD_STATEACTION_CLOSE;
    let _ = unsafe { WinVerifyTrust(ptr::null_mut(), &raw mut action, (&raw mut data).cast()) };
    if result != 0 {
        return Err(DaemonError::Lifecycle(
            "Windows package signature validation failed".into(),
        ));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn verify_staged_package_signature(path: &Path, installer: InstallerKind) -> Result<()> {
    if installer != InstallerKind::MacosDmg {
        return Err(DaemonError::Lifecycle(
            "macOS installer kind is invalid".into(),
        ));
    }
    successful(
        Command::new("codesign")
            .args(["--verify", "--strict"])
            .arg(path),
        "macOS package signature check",
    )?;
    // The protected release job validates the stapled ticket before signing this exact DMG digest.
    // Gatekeeper is the corresponding runtime check available on Macs without developer tools.
    successful(
        Command::new("spctl")
            .args([
                "--assess",
                "--type",
                "open",
                "--context",
                "context:primary-signature",
            ])
            .arg(path),
        "macOS Gatekeeper disk image assessment",
    )
}

#[cfg(target_os = "macos")]
fn verify_application_signature(path: &Path, installer: InstallerKind) -> Result<()> {
    if installer != InstallerKind::MacosDmg {
        return Err(DaemonError::Lifecycle(
            "macOS installer kind is invalid".into(),
        ));
    }
    let application = macos_app_root(path)?;
    successful(
        Command::new("codesign")
            .args(["--verify", "--deep", "--strict"])
            .arg(&application),
        "macOS package signature check",
    )?;
    successful(
        Command::new("spctl")
            .args(["--assess", "--type", "execute"])
            .arg(application),
        "macOS Gatekeeper application assessment",
    )
}

#[cfg(not(target_os = "macos"))]
fn verify_staged_package_signature(path: &Path, installer: InstallerKind) -> Result<()> {
    verify_platform_signature(path, installer)
}

#[cfg(not(target_os = "macos"))]
fn verify_application_signature(path: &Path, installer: InstallerKind) -> Result<()> {
    verify_platform_signature(path, installer)
}

#[cfg(target_os = "linux")]
fn verify_platform_signature(_path: &Path, installer: InstallerKind) -> Result<()> {
    if installer == InstallerKind::LinuxDeb {
        successful(
            Command::new("dpkg").args(["--verify", "sylvops"]),
            "Debian package verification",
        )?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub(crate) fn verify_detached_upgrade_helper(path: &Path) -> Result<()> {
    successful(
        Command::new("codesign")
            .args(["--verify", "--strict"])
            .arg(path),
        "macOS detached upgrade helper signature check",
    )
}

#[cfg(not(target_os = "macos"))]
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn verify_detached_upgrade_helper(_path: &Path) -> Result<()> {
    Ok(())
}

async fn stop_daemon_if_running(paths: &RuntimePaths) {
    let _ = request_before_deadline(
        paths,
        &ClientRequest::ShutdownDaemon,
        tokio::time::Instant::now() + IPC_ATTEMPT_TIMEOUT,
    )
    .await;
}

async fn request_before_deadline(
    paths: &RuntimePaths,
    request: &ClientRequest,
    deadline: tokio::time::Instant,
) -> Option<DaemonResponse> {
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    if remaining.is_zero() {
        return None;
    }
    tokio::time::timeout(remaining.min(IPC_ATTEMPT_TIMEOUT), async {
        let client = DaemonClient::connect(paths, "sylvops-upgrade-helper").await?;
        client.request(request).await
    })
    .await
    .ok()
    .and_then(std::result::Result::ok)
}

async fn report_outcome(
    paths: &RuntimePaths,
    version: &str,
    outcome: NativeUpgradeOutcome,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + HEALTH_TIMEOUT;
    loop {
        if matches!(
            request_before_deadline(
                paths,
                &ClientRequest::FinalizeUpdate {
                    version: version.into(),
                    outcome,
                },
                deadline,
            )
            .await,
            Some(DaemonResponse::Acknowledged)
        ) {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(DaemonError::Lifecycle(
                "daemon did not accept the upgrade outcome".into(),
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(target_os = "macos")]
fn relaunch_desktop(handoff: &NativeUpgradeHandoff) -> Result<()> {
    let application = macos_app_root(&handoff.installed_executable)?;
    let mut command = Command::new("open");
    command
        .args(["-n", "-W", "-a"])
        .arg(application)
        .arg("--args");
    if let Some(root) = state_override(handoff) {
        command.arg("--state-dir").arg(root);
    }
    command.arg("desktop");
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| {
            DaemonError::Lifecycle(format!(
                "updated desktop application did not relaunch: {error}"
            ))
        })?;
    let deadline = Instant::now() + DESKTOP_START_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait().map_err(|error| {
            DaemonError::Lifecycle(format!(
                "updated desktop application could not be observed: {error}"
            ))
        })? {
            return Err(DaemonError::Lifecycle(format!(
                "updated desktop application exited during relaunch with {status}"
            )));
        }
        if Instant::now() >= deadline {
            child.kill().map_err(|error| {
                DaemonError::Lifecycle(format!(
                    "updated desktop launch observer did not stop: {error}"
                ))
            })?;
            child.wait().map_err(|error| {
                DaemonError::Lifecycle(format!(
                    "updated desktop launch observer could not be reaped: {error}"
                ))
            })?;
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(not(target_os = "macos"))]
fn relaunch_desktop(handoff: &NativeUpgradeHandoff) -> Result<()> {
    let mut child = installed_command(handoff)
        .arg("desktop")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| {
            DaemonError::Lifecycle(format!("updated desktop did not relaunch: {error}"))
        })?;
    let deadline = Instant::now() + DESKTOP_START_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait().map_err(|error| {
            DaemonError::Lifecycle(format!("updated desktop could not be observed: {error}"))
        })? {
            return Err(DaemonError::Lifecycle(format!(
                "updated desktop exited during relaunch with {status}"
            )));
        }
        if Instant::now() >= deadline {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }
}
