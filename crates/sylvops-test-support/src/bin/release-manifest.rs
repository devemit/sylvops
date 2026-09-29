use std::{
    collections::BTreeMap,
    env,
    fs::{self, File},
    io::Read,
    path::Path,
    process::ExitCode,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sylvops_core::upgrade::{
    InstallerKind, MAX_RELEASE_METADATA_BYTES, MAX_UPGRADE_PAYLOAD_BYTES, ReleaseArchitecture,
    ReleaseMetadata, ReleasePlatform, ReleaseTarget, ReleaseValidationContext,
    SignedReleaseMetadata,
};

const MAX_KEY_FILE_BYTES: u64 = 512;
const MAX_EVIDENCE_BYTES: u64 = 64 * 1024;
const MAX_REPOSITORY_BYTES: usize = 200;

const PROMOTION_TARGETS: [PromotionTarget; 5] = [
    PromotionTarget {
        asset: "sylvops-windows-x86_64-setup.exe",
        manifest: "sylvops-update-windows-x86_64-nsis.json",
        target: ReleaseTarget {
            platform: ReleasePlatform::Windows,
            architecture: ReleaseArchitecture::X86_64,
            installer: InstallerKind::WindowsNsis,
        },
    },
    PromotionTarget {
        asset: "sylvops-macos-x86_64.dmg",
        manifest: "sylvops-update-macos-x86_64-dmg.json",
        target: ReleaseTarget {
            platform: ReleasePlatform::Macos,
            architecture: ReleaseArchitecture::X86_64,
            installer: InstallerKind::MacosDmg,
        },
    },
    PromotionTarget {
        asset: "sylvops-macos-aarch64.dmg",
        manifest: "sylvops-update-macos-aarch64-dmg.json",
        target: ReleaseTarget {
            platform: ReleasePlatform::Macos,
            architecture: ReleaseArchitecture::Aarch64,
            installer: InstallerKind::MacosDmg,
        },
    },
    PromotionTarget {
        asset: "sylvops-linux-x86_64.AppImage",
        manifest: "sylvops-update-linux-x86_64-appimage.json",
        target: ReleaseTarget {
            platform: ReleasePlatform::Linux,
            architecture: ReleaseArchitecture::X86_64,
            installer: InstallerKind::LinuxAppImage,
        },
    },
    PromotionTarget {
        asset: "sylvops-linux-x86_64.deb",
        manifest: "sylvops-update-linux-x86_64-deb.json",
        target: ReleaseTarget {
            platform: ReleasePlatform::Linux,
            architecture: ReleaseArchitecture::X86_64,
            installer: InstallerKind::LinuxDeb,
        },
    },
];

const EVIDENCE_TARGETS: [EvidenceTarget; 4] = [
    EvidenceTarget {
        name: "windows_x86_64",
        native_package_signature: "verified",
    },
    EvidenceTarget {
        name: "linux_x86_64",
        native_package_signature: "not_applicable",
    },
    EvidenceTarget {
        name: "macos_x86_64",
        native_package_signature: "verified",
    },
    EvidenceTarget {
        name: "macos_aarch64",
        native_package_signature: "verified",
    },
];

fn main() -> ExitCode {
    match run() {
        Ok(message) => {
            println!("{message}");
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("release manifest failed: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<&'static str, String> {
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    match arguments.first().map(String::as_str) {
        Some("generate") if arguments.len() == 13 => generate(&arguments[1..]),
        Some("verify") if arguments.len() == 4 => verify(&arguments[1..]),
        Some("assemble-evidence") if arguments.len() == 9 => {
            assemble_evidence(&arguments[1..])
        }
        Some("verify-promotion") if arguments.len() == 6 => verify_promotion(&arguments[1..]),
        _ => Err("expected `generate <asset> <manifest> <installer-url> <notes-url> <minimum-source-version> <target-version> <platform> <architecture> <installer> <private-key-file> <published-at> <release-notes>`, `verify <manifest> <asset> <public-key-base64>`, `assemble-evidence <output> <version> <commit> <repository> <four-native-evidence-files>`, or `verify-promotion <dist> <version> <commit> <repository> <public-key-base64>`".into()),
    }
}

fn generate(arguments: &[String]) -> Result<&'static str, String> {
    let asset = Path::new(&arguments[0]);
    let output = Path::new(&arguments[1]);
    let (byte_length, sha256) = release_asset_digest(asset)?;
    let target = parse_target(&arguments[6], &arguments[7], &arguments[8])?;
    let release = ReleaseMetadata {
        schema_version: 1,
        minimum_source_version: arguments[4].clone(),
        target_version: arguments[5].clone(),
        target,
        installer_url: arguments[2].clone(),
        byte_length,
        sha256,
        release_notes_url: arguments[3].clone(),
        release_notes: arguments[11].clone(),
        published_at_unix_seconds: arguments[10]
            .parse()
            .map_err(|_| "publication time is invalid")?,
    };
    let key_path = Path::new(&arguments[9]);
    let encoded_key_bytes = read_bounded(key_path, MAX_KEY_FILE_BYTES, "signing key file")?;
    let mut encoded_key =
        String::from_utf8(encoded_key_bytes).map_err(|_| "signing key is invalid")?;
    let mut key_bytes = STANDARD
        .decode(encoded_key.trim())
        .map_err(|_| "signing key is invalid")?;
    encoded_key.clear();
    let mut seed: [u8; 32] = key_bytes
        .as_slice()
        .try_into()
        .map_err(|_| "signing key has invalid length")?;
    key_bytes.fill(0);
    let signing_key = SigningKey::from_bytes(&seed);
    seed.fill(0);
    let signature = signing_key
        .sign(&release.signed_bytes().map_err(|error| error.to_string())?)
        .to_bytes();
    let envelope = SignedReleaseMetadata::new(release, signature);
    let encoded = serde_json::to_vec_pretty(&envelope)
        .map_err(|_| "could not encode signed release metadata")?;
    fs::write(output, encoded).map_err(|_| "could not write signed release metadata")?;
    Ok("signed release manifest generated")
}

fn verify(arguments: &[String]) -> Result<&'static str, String> {
    let manifest = read_bounded(
        Path::new(&arguments[0]),
        u64::try_from(MAX_RELEASE_METADATA_BYTES).expect("metadata limit fits u64"),
        "release manifest",
    )?;
    let key = parse_public_key(&arguments[2])?;
    let unverified: SignedReleaseMetadata =
        serde_json::from_slice(&manifest).map_err(|_| "release manifest is malformed")?;
    let context = ReleaseValidationContext {
        current_version: unverified.release.minimum_source_version.clone(),
        expected_target: unverified.release.target,
        now_unix_seconds: unverified
            .release
            .published_at_unix_seconds
            .saturating_add(1),
        oldest_allowed_publication: unverified.release.published_at_unix_seconds,
        newest_seen_publication: None,
    };
    let release =
        SignedReleaseMetadata::decode_and_validate_for_publication(&manifest, &key, &context)
            .map_err(|error| error.to_string())?;
    let (byte_length, sha256) = release_asset_digest(Path::new(&arguments[1]))?;
    if release.byte_length != byte_length || release.sha256 != sha256 {
        return Err("release asset does not match signed metadata".into());
    }
    Ok("signed release manifest verified")
}

fn assemble_evidence(arguments: &[String]) -> Result<&'static str, String> {
    let output = Path::new(&arguments[0]);
    let expected_version = &arguments[1];
    let expected_commit = &arguments[2];
    let repository = &arguments[3];
    validate_expected_identity(expected_version, expected_commit, repository)?;

    let mut targets = BTreeMap::new();
    for path in &arguments[4..] {
        let bytes = read_bounded(
            Path::new(path),
            MAX_EVIDENCE_BYTES,
            "native lifecycle evidence",
        )?;
        let native: NativeLifecycleEvidence =
            serde_json::from_slice(&bytes).map_err(|_| "native lifecycle evidence is malformed")?;
        if native.schema_version != 1
            || native.version != *expected_version
            || native.commit != *expected_commit
        {
            return Err("native lifecycle evidence identifies another revision".into());
        }
        let expected_target = EVIDENCE_TARGETS
            .iter()
            .copied()
            .find(|target| target.name == native.target)
            .ok_or_else(|| "native lifecycle evidence target is unsupported".to_string())?;
        let lifecycle = LifecycleEvidence {
            workflow_run_url: native.workflow_run_url,
            runtime_version: native.version.clone(),
            install: native.install,
            launch: native.launch,
            update: native.update,
            failed_health_rollback: native.failed_health_rollback,
            uninstall: native.uninstall,
            native_package_signature: native.native_package_signature,
        };
        verify_lifecycle_evidence(&lifecycle, expected_target, expected_version, repository)?;
        if targets.insert(native.target, lifecycle).is_some() {
            return Err("native lifecycle evidence contains a duplicate target".into());
        }
    }
    if targets.len() != EVIDENCE_TARGETS.len()
        || EVIDENCE_TARGETS
            .iter()
            .any(|expected| !targets.contains_key(expected.name))
    {
        return Err("native lifecycle evidence target set is incomplete".into());
    }

    let evidence = ReleaseEvidence {
        schema_version: 2,
        version: expected_version.clone(),
        commit: expected_commit.clone(),
        source_url: format!("https://github.com/{repository}/commit/{expected_commit}"),
        targets,
        release_contract: ReleaseContractEvidence {
            signed_update_metadata: "verified".into(),
            staged_payloads: "verified".into(),
            coordinator_rollback_unit: "passed".into(),
            detached_helper_rollback_integration: "passed".into(),
        },
    };
    let encoded =
        serde_json::to_vec_pretty(&evidence).map_err(|_| "could not encode release evidence")?;
    fs::write(output, encoded).map_err(|_| "could not write release evidence")?;
    Ok("release evidence assembled")
}

fn verify_promotion(arguments: &[String]) -> Result<&'static str, String> {
    let dist = Path::new(&arguments[0]);
    if !dist.is_dir() {
        return Err("release bundle directory is unavailable".into());
    }
    let expected_version = &arguments[1];
    let expected_commit = &arguments[2];
    let repository = &arguments[3];
    validate_expected_identity(expected_version, expected_commit, repository)?;
    let key = parse_public_key(&arguments[4])?;

    for promotion_target in PROMOTION_TARGETS {
        verify_promotion_manifest(dist, promotion_target, expected_version, repository, &key)?;
    }
    verify_release_evidence(dist, expected_version, expected_commit, repository)?;
    Ok("release promotion evidence verified")
}

fn verify_promotion_manifest(
    dist: &Path,
    promotion_target: PromotionTarget,
    expected_version: &str,
    repository: &str,
    key: &VerifyingKey,
) -> Result<(), String> {
    let manifest_path = dist.join(promotion_target.manifest);
    let manifest = read_bounded(
        &manifest_path,
        u64::try_from(MAX_RELEASE_METADATA_BYTES).expect("metadata limit fits u64"),
        "release manifest",
    )?;
    let unverified: SignedReleaseMetadata =
        serde_json::from_slice(&manifest).map_err(|_| "release manifest is malformed")?;
    let context = ReleaseValidationContext {
        current_version: unverified.release.minimum_source_version.clone(),
        expected_target: promotion_target.target,
        now_unix_seconds: unverified
            .release
            .published_at_unix_seconds
            .saturating_add(1),
        oldest_allowed_publication: unverified.release.published_at_unix_seconds,
        newest_seen_publication: None,
    };
    let release =
        SignedReleaseMetadata::decode_and_validate_for_publication(&manifest, key, &context)
            .map_err(|_| {
                format!(
                    "signed update metadata is invalid: {}",
                    promotion_target.manifest
                )
            })?;
    if release.target_version != expected_version {
        return Err(format!(
            "update metadata version is inconsistent: {}",
            promotion_target.manifest
        ));
    }
    let expected_tag = format!("v{expected_version}");
    let expected_installer_url = format!(
        "https://github.com/{repository}/releases/download/{expected_tag}/{}",
        promotion_target.asset
    );
    let expected_notes_url = format!("https://github.com/{repository}/releases/tag/{expected_tag}");
    if release.installer_url != expected_installer_url
        || release.release_notes_url != expected_notes_url
    {
        return Err(format!(
            "update metadata URL is inconsistent: {}",
            promotion_target.manifest
        ));
    }
    let (byte_length, sha256) = release_asset_digest(&dist.join(promotion_target.asset))?;
    if release.byte_length != byte_length || release.sha256 != sha256 {
        return Err(format!(
            "update metadata asset is inconsistent: {}",
            promotion_target.manifest
        ));
    }
    Ok(())
}

fn verify_release_evidence(
    dist: &Path,
    expected_version: &str,
    expected_commit: &str,
    repository: &str,
) -> Result<(), String> {
    let bytes = read_bounded(
        &dist.join("RELEASE-EVIDENCE.json"),
        MAX_EVIDENCE_BYTES,
        "release evidence",
    )?;
    let evidence: ReleaseEvidence =
        serde_json::from_slice(&bytes).map_err(|_| "release evidence is malformed")?;
    if evidence.schema_version != 2
        || evidence.version != expected_version
        || evidence.commit != expected_commit
        || evidence.source_url
            != format!("https://github.com/{repository}/commit/{expected_commit}")
    {
        return Err("release evidence does not identify the promoted revision".into());
    }
    if evidence.targets.len() != EVIDENCE_TARGETS.len() {
        return Err("release evidence target set is incomplete".into());
    }
    for expected_target in EVIDENCE_TARGETS {
        let target = evidence
            .targets
            .get(expected_target.name)
            .ok_or_else(|| "release evidence target set is incomplete".to_string())?;
        verify_lifecycle_evidence(target, expected_target, expected_version, repository)?;
    }
    if evidence.release_contract.signed_update_metadata != "verified"
        || evidence.release_contract.staged_payloads != "verified"
        || evidence.release_contract.coordinator_rollback_unit != "passed"
        || evidence
            .release_contract
            .detached_helper_rollback_integration
            != "passed"
    {
        return Err("release evidence is missing the signed staging contract".into());
    }
    Ok(())
}

fn verify_lifecycle_evidence(
    target: &LifecycleEvidence,
    expected_target: EvidenceTarget,
    expected_version: &str,
    repository: &str,
) -> Result<(), String> {
    let expected_run_prefix = format!("https://github.com/{repository}/actions/runs/");
    let run_id = target
        .workflow_run_url
        .strip_prefix(&expected_run_prefix)
        .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()));
    if run_id.is_none() || target.workflow_run_url.len() > 2_048 {
        return Err(format!(
            "release evidence workflow link is invalid: {}",
            expected_target.name
        ));
    }
    if target.install != "passed"
        || target.launch != "passed"
        || target.uninstall != "passed"
        || target.runtime_version != expected_version
        || target.native_package_signature != expected_target.native_package_signature
    {
        return Err(format!(
            "release evidence lifecycle result is incomplete: {}",
            expected_target.name
        ));
    }
    let required_upgrade_status = if expected_version == "0.1.0" {
        "not_applicable_initial_release"
    } else {
        "passed"
    };
    if target.update != required_upgrade_status
        || target.failed_health_rollback != required_upgrade_status
    {
        return Err(format!(
            "release evidence upgrade result is incomplete: {}",
            expected_target.name
        ));
    }
    Ok(())
}

fn validate_expected_identity(
    expected_version: &str,
    expected_commit: &str,
    repository: &str,
) -> Result<(), String> {
    if expected_version.is_empty() || expected_version.len() > 64 {
        return Err("expected release version is invalid".into());
    }
    if expected_commit.len() != 40
        || !expected_commit
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err("expected release commit is invalid".into());
    }
    if repository.is_empty()
        || repository.len() > MAX_REPOSITORY_BYTES
        || repository.matches('/').count() != 1
        || !repository
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'/'))
    {
        return Err("expected release repository is invalid".into());
    }
    Ok(())
}

fn parse_public_key(encoded: &str) -> Result<VerifyingKey, String> {
    if encoded.len() > MAX_KEY_FILE_BYTES.try_into().expect("key limit fits usize") {
        return Err("public key is invalid".into());
    }
    let key_bytes = STANDARD
        .decode(encoded)
        .map_err(|_| "public key is invalid")?;
    let key_bytes: [u8; 32] = key_bytes
        .try_into()
        .map_err(|_| "public key has invalid length")?;
    VerifyingKey::from_bytes(&key_bytes).map_err(|_| "public key is invalid".into())
}

fn read_bounded(path: &Path, maximum_bytes: u64, label: &str) -> Result<Vec<u8>, String> {
    let file = File::open(path).map_err(|_| format!("could not read {label}"))?;
    let mut reader = file.take(maximum_bytes.saturating_add(1));
    let mut bytes = Vec::new();
    reader
        .read_to_end(&mut bytes)
        .map_err(|_| format!("could not read {label}"))?;
    if u64::try_from(bytes.len()).expect("buffer length fits u64") > maximum_bytes {
        return Err(format!("{label} is oversized"));
    }
    Ok(bytes)
}

fn release_asset_digest(path: &Path) -> Result<(u64, String), String> {
    let metadata = fs::metadata(path).map_err(|_| "could not read release asset")?;
    if metadata.len() == 0 || metadata.len() > MAX_UPGRADE_PAYLOAD_BYTES {
        return Err("release asset size is invalid".into());
    }
    let mut file = File::open(path).map_err(|_| "could not read release asset")?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut byte_length = 0_u64;
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|_| "could not read release asset")?;
        if read == 0 {
            break;
        }
        byte_length = byte_length
            .checked_add(u64::try_from(read).expect("buffer length fits u64"))
            .ok_or_else(|| "release asset size is invalid".to_string())?;
        if byte_length > MAX_UPGRADE_PAYLOAD_BYTES {
            return Err("release asset size is invalid".into());
        }
        digest.update(&buffer[..read]);
    }
    if byte_length != metadata.len() {
        return Err("release asset changed during validation".into());
    }
    Ok((byte_length, format!("{:x}", digest.finalize())))
}

#[derive(Clone, Copy)]
struct PromotionTarget {
    asset: &'static str,
    manifest: &'static str,
    target: ReleaseTarget,
}

#[derive(Clone, Copy)]
struct EvidenceTarget {
    name: &'static str,
    native_package_signature: &'static str,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ReleaseEvidence {
    schema_version: u16,
    version: String,
    commit: String,
    source_url: String,
    targets: BTreeMap<String, LifecycleEvidence>,
    release_contract: ReleaseContractEvidence,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LifecycleEvidence {
    workflow_run_url: String,
    runtime_version: String,
    install: String,
    launch: String,
    update: String,
    failed_health_rollback: String,
    uninstall: String,
    native_package_signature: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ReleaseContractEvidence {
    signed_update_metadata: String,
    staged_payloads: String,
    coordinator_rollback_unit: String,
    detached_helper_rollback_integration: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeLifecycleEvidence {
    schema_version: u16,
    target: String,
    version: String,
    commit: String,
    workflow_run_url: String,
    install: String,
    launch: String,
    update: String,
    failed_health_rollback: String,
    uninstall: String,
    native_package_signature: String,
}

fn parse_target(
    platform: &str,
    architecture: &str,
    installer: &str,
) -> Result<ReleaseTarget, String> {
    let platform = match platform {
        "windows" => ReleasePlatform::Windows,
        "macos" => ReleasePlatform::Macos,
        "linux" => ReleasePlatform::Linux,
        _ => return Err("release platform is invalid".into()),
    };
    let architecture = match architecture {
        "x86_64" => ReleaseArchitecture::X86_64,
        "aarch64" => ReleaseArchitecture::Aarch64,
        _ => return Err("release architecture is invalid".into()),
    };
    let installer = match installer {
        "windows_nsis" => InstallerKind::WindowsNsis,
        "macos_dmg" => InstallerKind::MacosDmg,
        "linux_app_image" => InstallerKind::LinuxAppImage,
        "linux_deb" => InstallerKind::LinuxDeb,
        _ => return Err("release installer kind is invalid".into()),
    };
    let target = ReleaseTarget {
        platform,
        architecture,
        installer,
    };
    if !target.is_supported() {
        return Err("release target combination is unsupported".into());
    }
    Ok(target)
}
