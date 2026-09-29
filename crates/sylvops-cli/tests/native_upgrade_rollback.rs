#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use std::{
    fs,
    path::PathBuf,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use ed25519_dalek::{Signer, SigningKey};
use semver::Version;
use sha2::{Digest, Sha256};
use sylvops_core::{
    protocol::{ClientRequest, DaemonResponse},
    upgrade::{
        InstallerKind, NativeUpgradeHandoff, ReleaseArchitecture, ReleaseMetadata, ReleasePlatform,
        ReleaseTarget, SignedReleaseMetadata, UpgradeStatus,
    },
};
use sylvops_daemon::{client::DaemonClient, database::DatabaseHandle, runtime::RuntimePaths};

const HELPER_COMPLETION_TIMEOUT: Duration = Duration::from_secs(180);
const HELPER_PHASE_TIMEOUT: Duration = Duration::from_secs(60);
const WATCHDOG_ROLLBACK_TIMEOUT: Duration = Duration::from_secs(60);

static NATIVE_UPGRADE_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct NativeUpgradeFixture {
    _temporary: tempfile::TempDir,
    paths: RuntimePaths,
    staging: PathBuf,
    installed: PathBuf,
    previous: Vec<u8>,
    payload: Vec<u8>,
    target_version: String,
    signed_release: Vec<u8>,
    handoff: NativeUpgradeHandoff,
    handoff_path: PathBuf,
}

impl NativeUpgradeFixture {
    async fn stage(extra_payload: &[u8]) -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let state_root = temporary.path().join("state");
        let paths = RuntimePaths::discover(Some(&state_root)).unwrap();
        paths.prepare().unwrap();
        let database = DatabaseHandle::open(&paths.database).unwrap();
        tokio::time::timeout(Duration::from_secs(10), database.shutdown())
            .await
            .expect("database fixture shutdown timed out")
            .unwrap();
        let staging = paths.data_directory.join("upgrades");
        fs::create_dir(&staging).unwrap();

        let installed = temporary.path().join("installed/sylvops");
        fs::create_dir_all(installed.parent().unwrap()).unwrap();
        fs::copy(env!("CARGO_BIN_EXE_sylvops"), &installed).unwrap();
        let previous = fs::read(&installed).unwrap();
        let mut payload = previous.clone();
        payload.extend_from_slice(extra_payload);
        fs::write(staging.join("payload.staged"), &payload).unwrap();

        let current = Version::parse(env!("CARGO_PKG_VERSION")).unwrap();
        let mut target = current.clone();
        target.patch = target.patch.saturating_add(1);
        target.pre = semver::Prerelease::EMPTY;
        target.build = semver::BuildMetadata::EMPTY;
        let target_version = target.to_string();
        let release = ReleaseMetadata {
            schema_version: 1,
            minimum_source_version: current.to_string(),
            target_version: target_version.clone(),
            target: ReleaseTarget {
                platform: ReleasePlatform::Linux,
                architecture: ReleaseArchitecture::X86_64,
                installer: InstallerKind::LinuxAppImage,
            },
            installer_url: format!(
                "https://github.com/devemit/sylvops/releases/download/v{target_version}/sylvops-linux-x86_64.AppImage"
            ),
            byte_length: payload.len().try_into().unwrap(),
            sha256: format!("{:x}", Sha256::digest(&payload)),
            release_notes_url: format!(
                "https://github.com/devemit/sylvops/releases/tag/v{target_version}"
            ),
            release_notes: "Native rollback integration test.".into(),
            published_at_unix_seconds: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs()
                .try_into()
                .unwrap(),
        };
        let signing_key = SigningKey::from_bytes(&[
            0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec,
            0x2c, 0xc4, 0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03,
            0x1c, 0xae, 0x7f, 0x60,
        ]);
        let signature = signing_key
            .sign(&release.signed_bytes().unwrap())
            .to_bytes();
        let signed_release =
            serde_json::to_vec(&SignedReleaseMetadata::new(release.clone(), signature)).unwrap();
        fs::write(staging.join("release.json"), &signed_release).unwrap();
        let handoff = NativeUpgradeHandoff {
            release,
            staging_root: staging.clone(),
            installed_executable: installed.clone(),
            data_directory: paths.data_directory.clone(),
            config_directory: paths.config_directory.clone(),
            runtime_directory: paths.runtime_directory.clone(),
            client_process_ids: vec![i32::MAX.cast_unsigned()],
            relaunch_desktop: false,
        };
        let handoff_path = staging.join("handoff.json");
        fs::write(&handoff_path, serde_json::to_vec(&handoff).unwrap()).unwrap();

        Self {
            _temporary: temporary,
            paths,
            staging,
            installed,
            previous,
            payload,
            target_version,
            signed_release,
            handoff,
            handoff_path,
        }
    }

    fn restage(&self) {
        fs::write(self.staging.join("payload.staged"), &self.payload).unwrap();
        fs::write(self.staging.join("release.json"), &self.signed_release).unwrap();
        fs::write(
            &self.handoff_path,
            serde_json::to_vec(&self.handoff).unwrap(),
        )
        .unwrap();
    }

    fn command(&self) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_sylvops"));
        command
            .arg("update-helper")
            .arg("--handoff")
            .arg(&self.handoff_path)
            .kill_on_drop(true);
        command
    }

    fn attempt(&self) -> serde_json::Value {
        serde_json::from_slice(&fs::read(self.staging.join("native-upgrade-attempt.json")).unwrap())
            .unwrap()
    }
}

#[tokio::test]
async fn detached_helper_restores_and_reports_after_native_health_failure() {
    let _test_lock = NATIVE_UPGRADE_TEST_LOCK.lock().await;
    let fixture = NativeUpgradeFixture::stage(&[]).await;
    let output = tokio::time::timeout(HELPER_COMPLETION_TIMEOUT, fixture.command().output())
        .await
        .expect("native helper timed out")
        .unwrap();
    assert!(!output.status.success());
    let helper_error = String::from_utf8_lossy(&output.stderr);
    let daemon_log = fs::read_to_string(&fixture.paths.daemon_log).ok();
    assert!(
        helper_error.contains("previous version was restored"),
        "unexpected helper error: {helper_error}; attempt={:?}; daemon_log={daemon_log:?}",
        fs::read_to_string(fixture.staging.join("native-upgrade-attempt.json")).ok()
    );
    assert_eq!(fs::read(&fixture.installed).unwrap(), fixture.previous);

    let client = DaemonClient::connect(&fixture.paths, "native-upgrade-integration")
        .await
        .unwrap();
    assert_eq!(
        client
            .request(&ClientRequest::GetUpdateStatus)
            .await
            .unwrap(),
        DaemonResponse::UpdateStatus(UpgradeStatus::RolledBack {
            version: fixture.target_version.clone(),
        })
    );
    let attempt = fixture.attempt();
    assert_eq!(attempt["phase"], "rolled_back");
    assert_eq!(attempt["rollback_attempts"], 1);
    assert_eq!(attempt["diagnostic"], "health_check_failed");
    assert_eq!(
        client
            .request(&ClientRequest::ShutdownDaemon)
            .await
            .unwrap(),
        DaemonResponse::Acknowledged
    );

    fixture.restage();
    let repeated = tokio::time::timeout(Duration::from_secs(20), fixture.command().output())
        .await
        .expect("repeated native helper timed out")
        .unwrap();
    assert!(!repeated.status.success());
    assert!(
        String::from_utf8_lossy(&repeated.stderr).contains("already attempted"),
        "unexpected repeated helper error: {}",
        String::from_utf8_lossy(&repeated.stderr)
    );
}

#[tokio::test]
async fn detached_watchdog_restores_after_upgrade_helper_is_interrupted() {
    let _test_lock = NATIVE_UPGRADE_TEST_LOCK.lock().await;
    let fixture = NativeUpgradeFixture::stage(b"interrupted-upgrade-candidate").await;
    let mut helper = fixture.command().spawn().unwrap();
    let applying_deadline = Instant::now() + HELPER_PHASE_TIMEOUT;
    let mut applying = false;
    while Instant::now() < applying_deadline {
        applying = fs::read(fixture.staging.join("native-upgrade-attempt.json"))
            .ok()
            .and_then(|encoded| serde_json::from_slice::<serde_json::Value>(&encoded).ok())
            .is_some_and(|attempt| attempt["phase"] == "applying");
        if applying {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        applying,
        "the helper never began applying the candidate; attempt={:?}",
        fs::read_to_string(fixture.staging.join("native-upgrade-attempt.json")).ok()
    );
    helper.kill().await.unwrap();
    helper.wait().await.unwrap();

    let rollback_deadline = Instant::now() + WATCHDOG_ROLLBACK_TIMEOUT;
    while fs::read(&fixture.installed).unwrap() != fixture.previous
        && Instant::now() < rollback_deadline
    {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let restored = fs::read(&fixture.installed).unwrap() == fixture.previous;
    if let Ok(client) =
        DaemonClient::connect(&fixture.paths, "native-upgrade-interruption-cleanup").await
    {
        let _ = client.request(&ClientRequest::ShutdownDaemon).await;
    }
    assert!(
        restored,
        "the watchdog did not restore the previous executable"
    );
}
