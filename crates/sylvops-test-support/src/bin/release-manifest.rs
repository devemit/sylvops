use std::{env, fs, path::Path, process::ExitCode};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use sha2::{Digest, Sha256};
use sylvops_core::upgrade::{
    InstallerKind, ReleaseArchitecture, ReleaseMetadata, ReleasePlatform, ReleaseTarget,
    ReleaseValidationContext, SignedReleaseMetadata,
};

const MAX_KEY_FILE_BYTES: u64 = 512;

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
        _ => Err("expected `generate <asset> <manifest> <installer-url> <notes-url> <minimum-source-version> <target-version> <platform> <architecture> <installer> <private-key-file> <published-at> <release-notes>` or `verify <manifest> <asset> <public-key-base64>`".into()),
    }
}

fn generate(arguments: &[String]) -> Result<&'static str, String> {
    let asset = Path::new(&arguments[0]);
    let output = Path::new(&arguments[1]);
    let bytes = fs::read(asset).map_err(|_| "could not read release asset")?;
    let target = parse_target(&arguments[6], &arguments[7], &arguments[8])?;
    let release = ReleaseMetadata {
        schema_version: 1,
        minimum_source_version: arguments[4].clone(),
        target_version: arguments[5].clone(),
        target,
        installer_url: arguments[2].clone(),
        byte_length: u64::try_from(bytes.len()).map_err(|_| "release asset is oversized")?,
        sha256: format!("{:x}", Sha256::digest(&bytes)),
        release_notes_url: arguments[3].clone(),
        release_notes: arguments[11].clone(),
        published_at_unix_seconds: arguments[10]
            .parse()
            .map_err(|_| "publication time is invalid")?,
    };
    let key_path = Path::new(&arguments[9]);
    if fs::metadata(key_path)
        .map_err(|_| "could not read signing key")?
        .len()
        > MAX_KEY_FILE_BYTES
    {
        return Err("signing key file is oversized".into());
    }
    let mut encoded_key = fs::read_to_string(key_path).map_err(|_| "could not read signing key")?;
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
    let manifest = fs::read(&arguments[0]).map_err(|_| "could not read release manifest")?;
    let asset = fs::read(&arguments[1]).map_err(|_| "could not read release asset")?;
    let key_bytes = STANDARD
        .decode(&arguments[2])
        .map_err(|_| "public key is invalid")?;
    let key_bytes: [u8; 32] = key_bytes
        .try_into()
        .map_err(|_| "public key has invalid length")?;
    let key = VerifyingKey::from_bytes(&key_bytes).map_err(|_| "public key is invalid")?;
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
    if release.byte_length != u64::try_from(asset.len()).map_err(|_| "asset is oversized")?
        || release.sha256 != format!("{:x}", Sha256::digest(&asset))
    {
        return Err("release asset does not match signed metadata".into());
    }
    Ok("signed release manifest verified")
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
