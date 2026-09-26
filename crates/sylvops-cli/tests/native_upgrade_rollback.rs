#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use std::{
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
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
use sylvops_daemon::{client::DaemonClient, runtime::RuntimePaths};

#[tokio::test]
async fn detached_helper_restores_and_reports_after_native_health_failure() {
    let temporary = tempfile::tempdir().unwrap();
    let state_root = temporary.path().join("state");
    let paths = RuntimePaths::discover(Some(&state_root)).unwrap();
    paths.prepare().unwrap();
    let staging = paths.data_directory.join("upgrades");
    fs::create_dir(&staging).unwrap();

    let installed = temporary.path().join("installed/sylvops");
    fs::create_dir_all(installed.parent().unwrap()).unwrap();
    fs::copy(env!("CARGO_BIN_EXE_sylvops"), &installed).unwrap();

    let payload = fs::read(env!("CARGO_BIN_EXE_sylvops")).unwrap();
    fs::write(staging.join("payload.staged"), &payload).unwrap();
    let current = Version::parse(env!("CARGO_PKG_VERSION")).unwrap();
    let mut target = current.clone();
    target.patch = target.patch.saturating_add(1);
    target.pre = semver::Prerelease::EMPTY;
    target.build = semver::BuildMetadata::EMPTY;
    let target_version = target.to_string();
    let published_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .try_into()
        .unwrap();
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
        published_at_unix_seconds: published_at,
    };
    let signing_key = SigningKey::from_bytes(&[
        0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec, 0x2c,
        0xc4, 0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03, 0x1c, 0xae,
        0x7f, 0x60,
    ]);
    let signature = signing_key
        .sign(&release.signed_bytes().unwrap())
        .to_bytes();
    fs::write(
        staging.join("release.json"),
        serde_json::to_vec(&SignedReleaseMetadata::new(release.clone(), signature)).unwrap(),
    )
    .unwrap();
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

    let output = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_sylvops"))
            .arg("update-helper")
            .arg("--handoff")
            .arg(&handoff_path)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("native helper timed out")
    .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("previous version was restored"),
        "unexpected helper error: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read(&installed).unwrap(),
        fs::read(PathBuf::from(env!("CARGO_BIN_EXE_sylvops"))).unwrap()
    );

    let client = DaemonClient::connect(&paths, "native-upgrade-integration")
        .await
        .unwrap();
    assert_eq!(
        client
            .request(&ClientRequest::GetUpdateStatus)
            .await
            .unwrap(),
        DaemonResponse::UpdateStatus(UpgradeStatus::RolledBack {
            version: target_version,
        })
    );
    assert_eq!(
        client
            .request(&ClientRequest::ShutdownDaemon)
            .await
            .unwrap(),
        DaemonResponse::Acknowledged
    );
}
