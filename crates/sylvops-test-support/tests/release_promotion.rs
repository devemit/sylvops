use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use ed25519_dalek::SigningKey;
use serde_json::json;

const REPOSITORY: &str = "devemit/sylvops";
const VERSION: &str = "0.2.0";
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

#[test]
fn promotion_verifier_accepts_one_complete_revision_consistent_release() {
    let fixture = PromotionFixture::new();

    let output = fixture.verify();

    assert_success(&output);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "release promotion evidence verified"
    );
}

#[test]
fn promotion_verifier_rejects_an_invalid_update_signature() {
    let fixture = PromotionFixture::new();
    fixture.mutate_json("sylvops-update-windows-x86_64-nsis.json", |manifest| {
        manifest["signature"] = serde_json::Value::String(STANDARD.encode([0_u8; 64]));
    });

    let output = fixture.verify();

    assert_failure(&output, "signed update metadata is invalid");
}

#[test]
fn promotion_verifier_rejects_a_manifest_assigned_to_the_wrong_target() {
    let fixture = PromotionFixture::new();
    let windows = fixture.dist.join("sylvops-update-windows-x86_64-nsis.json");
    let macos = fixture.dist.join("sylvops-update-macos-x86_64-dmg.json");
    fs::copy(macos, windows).expect("replace Windows manifest with signed macOS manifest");

    let output = fixture.verify();

    assert_failure(&output, "signed update metadata is invalid");
}

#[test]
fn promotion_verifier_rejects_missing_target_lifecycle_evidence() {
    let fixture = PromotionFixture::new();
    fixture.mutate_json("RELEASE-EVIDENCE.json", |evidence| {
        evidence["targets"]
            .as_object_mut()
            .expect("target evidence object")
            .remove("macos_aarch64");
    });

    let output = fixture.verify();

    assert_failure(&output, "release evidence target set is incomplete");
}

#[test]
fn promotion_verifier_rejects_evidence_from_another_commit() {
    let fixture = PromotionFixture::new();
    fixture.mutate_json("RELEASE-EVIDENCE.json", |evidence| {
        evidence["commit"] =
            serde_json::Value::String("ffffffffffffffffffffffffffffffffffffffff".into());
    });

    let output = fixture.verify();

    assert_failure(
        &output,
        "release evidence does not identify the promoted revision",
    );
}

#[test]
fn promotion_verifier_rejects_a_partial_release_set() {
    let fixture = PromotionFixture::new();
    fs::remove_file(fixture.dist.join("sylvops-macos-aarch64.dmg"))
        .expect("remove required native package");

    let output = fixture.verify();

    assert_failure(&output, "could not read release asset");
}

#[test]
fn promotion_verifier_rejects_missing_native_signature_evidence() {
    let fixture = PromotionFixture::new();
    fixture.mutate_json("RELEASE-EVIDENCE.json", |evidence| {
        evidence["targets"]["windows_x86_64"]["native_package_signature"] =
            serde_json::Value::String("missing".into());
    });

    let output = fixture.verify();

    assert_failure(&output, "release evidence lifecycle result is incomplete");
}

#[test]
fn promotion_verifier_rejects_oversized_evidence_before_parsing_it() {
    let fixture = PromotionFixture::new();
    fs::write(
        fixture.dist.join("RELEASE-EVIDENCE.json"),
        vec![b'x'; 64 * 1024 + 1],
    )
    .expect("write oversized evidence");

    let output = fixture.verify();

    assert_failure(&output, "release evidence is oversized");
}

#[test]
fn evidence_assembler_preserves_each_targets_linked_lifecycle_results() {
    let temporary = tempfile::tempdir().expect("temporary evidence directory");
    let output_path = temporary.path().join("RELEASE-EVIDENCE.json");
    let workflow_run_url = format!("https://github.com/{REPOSITORY}/actions/runs/123456");
    let inputs = [
        ("windows_x86_64", "verified"),
        ("linux_x86_64", "not_applicable"),
        ("macos_x86_64", "verified"),
        ("macos_aarch64", "verified"),
    ]
    .map(|(target, native_package_signature)| {
        let path = temporary.path().join(format!("{target}.json"));
        let evidence = json!({
            "schema_version": 1,
            "target": target,
            "version": VERSION,
            "commit": COMMIT,
            "workflow_run_url": workflow_run_url,
            "install": "passed",
            "launch": "passed",
            "update": "passed",
            "failed_health_rollback": "passed",
            "uninstall": "passed",
            "native_package_signature": native_package_signature
        });
        fs::write(
            &path,
            serde_json::to_vec_pretty(&evidence).expect("encode native evidence"),
        )
        .expect("write native evidence");
        path
    });

    let output = release_manifest()
        .arg("assemble-evidence")
        .arg(&output_path)
        .arg(VERSION)
        .arg(COMMIT)
        .arg(REPOSITORY)
        .args(&inputs)
        .output()
        .expect("assemble release evidence");

    assert_success(&output);
    let evidence: serde_json::Value =
        serde_json::from_slice(&fs::read(output_path).expect("read assembled release evidence"))
            .expect("parse assembled release evidence");
    assert_eq!(evidence["schema_version"], 2);
    assert_eq!(evidence["commit"], COMMIT);
    assert_eq!(
        evidence["targets"]["macos_aarch64"]["failed_health_rollback"],
        "passed"
    );
    assert_eq!(
        evidence["targets"]["linux_x86_64"]["native_package_signature"],
        "not_applicable"
    );
    assert_eq!(
        evidence["targets"]["windows_x86_64"]["runtime_version"],
        VERSION
    );
}

struct PromotionFixture {
    _temporary: tempfile::TempDir,
    dist: PathBuf,
    public_key: String,
}

impl PromotionFixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().expect("temporary release directory");
        let dist = temporary.path().join("dist");
        fs::create_dir(&dist).expect("release directory");

        let signing_key = SigningKey::from_bytes(&[7; 32]);
        let public_key = STANDARD.encode(signing_key.verifying_key().to_bytes());
        let private_key = temporary.path().join("private-key");
        fs::write(&private_key, STANDARD.encode(signing_key.to_bytes()))
            .expect("write private key");

        for target in release_targets() {
            let asset = dist.join(target.asset);
            fs::write(&asset, format!("signed package for {}", target.name))
                .expect("write release asset");
            let manifest = dist.join(target.manifest);
            let output = release_manifest()
                .arg("generate")
                .arg(&asset)
                .arg(&manifest)
                .arg(format!(
                    "https://github.com/{REPOSITORY}/releases/download/v{VERSION}/{}",
                    target.asset
                ))
                .arg(format!(
                    "https://github.com/{REPOSITORY}/releases/tag/v{VERSION}"
                ))
                .arg("0.1.0")
                .arg(VERSION)
                .arg(target.platform)
                .arg(target.architecture)
                .arg(target.installer)
                .arg(&private_key)
                .arg("1780000000")
                .arg("Verified release notes.")
                .output()
                .expect("generate signed update manifest");
            assert_success(&output);
        }

        let workflow_run_url = format!("https://github.com/{REPOSITORY}/actions/runs/123456");
        let passed_target = |native_package_signature: &str| {
            json!({
                "workflow_run_url": workflow_run_url,
                "runtime_version": VERSION,
                "install": "passed",
                "launch": "passed",
                "update": "passed",
                "failed_health_rollback": "passed",
                "uninstall": "passed",
                "native_package_signature": native_package_signature
            })
        };
        let evidence = json!({
            "schema_version": 2,
            "version": VERSION,
            "commit": COMMIT,
            "source_url": format!("https://github.com/{REPOSITORY}/commit/{COMMIT}"),
            "targets": {
                "windows_x86_64": passed_target("verified"),
                "linux_x86_64": passed_target("not_applicable"),
                "macos_x86_64": passed_target("verified"),
                "macos_aarch64": passed_target("verified")
            },
            "release_contract": {
                "signed_update_metadata": "verified",
                "staged_payloads": "verified",
                "coordinator_rollback_unit": "passed",
                "detached_helper_rollback_integration": "passed"
            }
        });
        fs::write(
            dist.join("RELEASE-EVIDENCE.json"),
            serde_json::to_vec_pretty(&evidence).expect("encode release evidence"),
        )
        .expect("write release evidence");

        Self {
            _temporary: temporary,
            dist,
            public_key,
        }
    }

    fn verify(&self) -> Output {
        release_manifest()
            .arg("verify-promotion")
            .arg(&self.dist)
            .arg(VERSION)
            .arg(COMMIT)
            .arg(REPOSITORY)
            .arg(&self.public_key)
            .output()
            .expect("run promotion verifier")
    }

    fn mutate_json(&self, name: &str, mutate: impl FnOnce(&mut serde_json::Value)) {
        let path = self.dist.join(name);
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).expect("read JSON fixture"))
                .expect("parse JSON fixture");
        mutate(&mut value);
        fs::write(
            path,
            serde_json::to_vec_pretty(&value).expect("encode JSON fixture"),
        )
        .expect("write JSON fixture");
    }
}

struct ReleaseTarget {
    name: &'static str,
    asset: &'static str,
    manifest: &'static str,
    platform: &'static str,
    architecture: &'static str,
    installer: &'static str,
}

fn release_targets() -> [ReleaseTarget; 5] {
    [
        ReleaseTarget {
            name: "windows_x86_64",
            asset: "sylvops-windows-x86_64-setup.exe",
            manifest: "sylvops-update-windows-x86_64-nsis.json",
            platform: "windows",
            architecture: "x86_64",
            installer: "windows_nsis",
        },
        ReleaseTarget {
            name: "macos_x86_64",
            asset: "sylvops-macos-x86_64.dmg",
            manifest: "sylvops-update-macos-x86_64-dmg.json",
            platform: "macos",
            architecture: "x86_64",
            installer: "macos_dmg",
        },
        ReleaseTarget {
            name: "macos_aarch64",
            asset: "sylvops-macos-aarch64.dmg",
            manifest: "sylvops-update-macos-aarch64-dmg.json",
            platform: "macos",
            architecture: "aarch64",
            installer: "macos_dmg",
        },
        ReleaseTarget {
            name: "linux_x86_64_appimage",
            asset: "sylvops-linux-x86_64.AppImage",
            manifest: "sylvops-update-linux-x86_64-appimage.json",
            platform: "linux",
            architecture: "x86_64",
            installer: "linux_app_image",
        },
        ReleaseTarget {
            name: "linux_x86_64_deb",
            asset: "sylvops-linux-x86_64.deb",
            manifest: "sylvops-update-linux-x86_64-deb.json",
            platform: "linux",
            architecture: "x86_64",
            installer: "linux_deb",
        },
    ]
}

fn release_manifest() -> Command {
    Command::new(env!("CARGO_BIN_EXE_release-manifest"))
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_failure(output: &Output, expected_error: &str) {
    assert!(
        !output.status.success(),
        "command unexpectedly succeeded\nstdout:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(expected_error),
        "expected error {expected_error:?}\nstderr:\n{stderr}"
    );
}
