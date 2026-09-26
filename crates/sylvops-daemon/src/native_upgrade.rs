//! Detached native package application, health verification, and one-shot rollback.

use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

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
const STOP_TIMEOUT: Duration = Duration::from_secs(20);
const CLIENT_EXIT_TIMEOUT: Duration = Duration::from_secs(20);
const HEALTH_TIMEOUT: Duration = Duration::from_secs(20);
const IPC_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(2);
const NATIVE_COMMAND_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const MAX_VERSION_OUTPUT_BYTES: u64 = 4 * 1024;
const MAX_PACKAGE_TREE_DEPTH: usize = 32;
const MAX_PACKAGE_TREE_ENTRIES: u64 = 8_192;
const MAX_PACKAGE_TREE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

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
    let payload = validate_staged_release(&handoff).await?;
    verify_platform_signature(&payload, handoff.release.target.installer)?;
    let backup = create_backup(&handoff)?;
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
    if let Err(error) = apply_package(&handoff, &payload) {
        return rollback_and_restart(&paths, &handoff, &backup, &error).await;
    }
    if let Err(error) = start_installed_daemon(&handoff) {
        return rollback_and_restart(&paths, &handoff, &backup, &error).await;
    }
    if let Err(error) = verify_health(&paths, &handoff).await {
        return rollback_and_restart(&paths, &handoff, &backup, &error).await;
    }
    report_outcome(
        &paths,
        &handoff.release.target_version,
        NativeUpgradeOutcome::Installed,
    )
    .await?;
    if handoff.relaunch_desktop {
        relaunch_desktop(&handoff)?;
    }
    Ok(())
}

async fn rollback_and_restart(
    paths: &RuntimePaths,
    handoff: &NativeUpgradeHandoff,
    backup: &Backup,
    cause: &dyn std::fmt::Display,
) -> Result<()> {
    stop_daemon_if_running(paths).await;
    wait_for_daemon_stop(paths).await?;
    restore_backup(handoff, backup)?;
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
    Err(DaemonError::Lifecycle(format!(
        "application upgrade failed and the previous version was restored: {cause}"
    )))
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
}

fn create_backup(handoff: &NativeUpgradeHandoff) -> Result<Backup> {
    let rollback = handoff.staging_root.join("rollback");
    if rollback.exists() {
        fs::remove_dir_all(&rollback)?;
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
            copy_tree(&app, &backup)?;
            backup
        }
        InstallerKind::LinuxDeb => create_debian_backup(&rollback)?,
    };
    Ok(Backup { payload })
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
    successful(
        Command::new(payload).args(["/S", "/R"]),
        "Windows installer",
    )
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
    let mount = handoff.staging_root.join("mounted-update");
    fs::create_dir_all(&mount)?;
    successful(
        Command::new("hdiutil")
            .args(["attach", "-nobrowse", "-readonly", "-mountpoint"])
            .arg(&mount)
            .arg(payload),
        "macOS disk image mount",
    )?;
    let source = fs::read_dir(&mount)?
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|extension| extension == "app"))
        .ok_or_else(|| DaemonError::Lifecycle("mounted update has no application bundle".into()))?;
    let target = macos_app_root(&handoff.installed_executable)?;
    let pending = handoff.staging_root.join("pending.app");
    copy_tree(&source, &pending)?;
    if target.exists() {
        fs::remove_dir_all(&target)?;
    }
    fs::rename(pending, target)?;
    let _ = successful(
        Command::new("hdiutil").args(["detach"]).arg(&mount),
        "macOS disk image detach",
    );
    Ok(())
}

fn restore_backup(handoff: &NativeUpgradeHandoff, backup: &Backup) -> Result<()> {
    match handoff.release.target.installer {
        InstallerKind::WindowsNsis => restore_windows_backup(handoff, &backup.payload),
        InstallerKind::LinuxAppImage => {
            fs::copy(&backup.payload, installed_payload_path(handoff)?)?;
            Ok(())
        }
        InstallerKind::MacosDmg => {
            let target = macos_app_root(&handoff.installed_executable)?;
            if target.exists() {
                fs::remove_dir_all(&target)?;
            }
            copy_tree(&backup.payload, &target)
        }
        InstallerKind::LinuxDeb => successful(
            Command::new("pkexec")
                .args(["dpkg", "--install"])
                .arg(&backup.payload),
            "Debian rollback installer",
        ),
    }
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
    let candidate_registry = rollback.join("candidate-registry");
    if candidate_registry.exists() {
        fs::remove_dir_all(&candidate_registry)?;
    }
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
        return Err(error);
    }
    menu_swap.commit();
    install_swap.commit();
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
    executable
        .ancestors()
        .find(|path| path.extension().is_some_and(|extension| extension == "app"))
        .map(Path::to_path_buf)
        .ok_or_else(|| DaemonError::Lifecycle("installed macOS bundle was not found".into()))
}

#[cfg(not(target_os = "macos"))]
fn macos_app_root(_executable: &Path) -> Result<PathBuf> {
    Err(DaemonError::Lifecycle(
        "macOS bundle is unavailable on this platform".into(),
    ))
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
            budget.bytes = budget.bytes.saturating_add(entry.metadata()?.len());
            if budget.bytes > MAX_PACKAGE_TREE_BYTES {
                return Err(DaemonError::Lifecycle(
                    "package tree exceeds the byte limit".into(),
                ));
            }
            fs::copy(entry.path(), target)?;
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
            if !installed_version(handoff)?.contains(&handoff.release.target_version) {
                return Err(DaemonError::Lifecycle(
                    "updated executable version does not match".into(),
                ));
            }
            verify_platform_signature(
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
fn verify_platform_signature(path: &Path, installer: InstallerKind) -> Result<()> {
    let target = if installer == InstallerKind::MacosDmg {
        macos_app_root(path).unwrap_or_else(|_| path.to_path_buf())
    } else {
        path.to_path_buf()
    };
    successful(
        Command::new("codesign")
            .args(["--verify", "--deep", "--strict"])
            .arg(target),
        "macOS package signature check",
    )
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

fn relaunch_desktop(handoff: &NativeUpgradeHandoff) -> Result<()> {
    installed_command(handoff)
        .arg("desktop")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| {
            DaemonError::Lifecycle(format!("updated desktop did not relaunch: {error}"))
        })?;
    Ok(())
}
