//! Signed, bounded application-release metadata shared by update clients.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use semver::Version;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use thiserror::Error;
use url::Url;

pub const RELEASE_SCHEMA_VERSION: u16 = 1;
pub const MAX_RELEASE_METADATA_BYTES: usize = 64 * 1024;
pub const MAX_RELEASE_NOTES_BYTES: usize = 32 * 1024;
pub const MAX_UPGRADE_PAYLOAD_BYTES: u64 = 4 * 1024 * 1024 * 1024;
pub const MAX_UPGRADE_CLIENT_PROCESSES: usize = 128;
const MAX_URL_BYTES: usize = 2_048;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleasePlatform {
    Windows,
    Macos,
    Linux,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseArchitecture {
    X86_64,
    Aarch64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallerKind {
    WindowsNsis,
    MacosDmg,
    LinuxAppImage,
    LinuxDeb,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReleaseTarget {
    pub platform: ReleasePlatform,
    pub architecture: ReleaseArchitecture,
    pub installer: InstallerKind,
}

impl ReleaseTarget {
    #[must_use]
    pub const fn is_supported(self) -> bool {
        matches!(
            (self.platform, self.architecture, self.installer),
            (
                ReleasePlatform::Windows,
                ReleaseArchitecture::X86_64,
                InstallerKind::WindowsNsis
            ) | (
                ReleasePlatform::Macos,
                ReleaseArchitecture::X86_64 | ReleaseArchitecture::Aarch64,
                InstallerKind::MacosDmg
            ) | (
                ReleasePlatform::Linux,
                ReleaseArchitecture::X86_64,
                InstallerKind::LinuxAppImage | InstallerKind::LinuxDeb
            )
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseMetadata {
    pub schema_version: u16,
    pub minimum_source_version: String,
    pub target_version: String,
    pub target: ReleaseTarget,
    pub installer_url: String,
    pub byte_length: u64,
    pub sha256: String,
    pub release_notes_url: String,
    pub release_notes: String,
    pub published_at_unix_seconds: i64,
}

impl ReleaseMetadata {
    /// Produces the deterministic bytes covered by the application update signature.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization unexpectedly fails.
    pub fn signed_bytes(&self) -> Result<Vec<u8>, ReleaseValidationError> {
        serde_json::to_vec(self).map_err(|_| ReleaseValidationError::Malformed)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedReleaseMetadata {
    pub release: ReleaseMetadata,
    pub signature: String,
}

impl SignedReleaseMetadata {
    #[must_use]
    pub fn new(release: ReleaseMetadata, signature: [u8; 64]) -> Self {
        Self {
            release,
            signature: STANDARD.encode(signature),
        }
    }

    /// Parses, verifies, and checks compatibility of one bounded release envelope.
    ///
    /// # Errors
    ///
    /// Fails closed for malformed, unsigned, stale, replayed, incompatible, or oversized data.
    pub fn decode_and_validate(
        bytes: &[u8],
        verifying_key: &VerifyingKey,
        context: &ReleaseValidationContext,
    ) -> Result<ReleaseMetadata, ReleaseValidationError> {
        let envelope = Self::decode_and_verify(bytes, verifying_key)?;
        envelope.release.validate(context)?;
        Ok(envelope.release)
    }

    /// Verifies a release envelope for publication, including an initial release whose target is
    /// also its minimum supported source version.
    ///
    /// # Errors
    ///
    /// Fails closed for every validation error except the expected current-version result.
    pub fn decode_and_validate_for_publication(
        bytes: &[u8],
        verifying_key: &VerifyingKey,
        context: &ReleaseValidationContext,
    ) -> Result<ReleaseMetadata, ReleaseValidationError> {
        let envelope = Self::decode_and_verify(bytes, verifying_key)?;
        match envelope.release.validate(context) {
            Ok(()) | Err(ReleaseValidationError::CurrentVersion) => Ok(envelope.release),
            Err(error) => Err(error),
        }
    }

    fn decode_and_verify(
        bytes: &[u8],
        verifying_key: &VerifyingKey,
    ) -> Result<Self, ReleaseValidationError> {
        if bytes.len() > MAX_RELEASE_METADATA_BYTES {
            return Err(ReleaseValidationError::OversizedMetadata);
        }
        let envelope: Self =
            serde_json::from_slice(bytes).map_err(|_| ReleaseValidationError::Malformed)?;
        let signature_bytes = STANDARD
            .decode(&envelope.signature)
            .map_err(|_| ReleaseValidationError::InvalidSignature)?;
        let signature = Signature::from_slice(&signature_bytes)
            .map_err(|_| ReleaseValidationError::InvalidSignature)?;
        verifying_key
            .verify(&envelope.release.signed_bytes()?, &signature)
            .map_err(|_| ReleaseValidationError::InvalidSignature)?;
        Ok(envelope)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReleaseValidationContext {
    pub current_version: String,
    pub expected_target: ReleaseTarget,
    pub now_unix_seconds: i64,
    pub oldest_allowed_publication: i64,
    pub newest_seen_publication: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ActiveUpgradeSession {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum InstallDisposition {
    Blocked {
        active_sessions: Vec<ActiveUpgradeSession>,
    },
    Prepared {
        version: String,
    },
    Installed,
    RolledBack,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum UpgradeStatus {
    Idle,
    UpToDate { version: String },
    Available { release: ReleaseMetadata },
    Downloading { release: ReleaseMetadata },
    Staged { release: ReleaseMetadata },
    Installing { release: ReleaseMetadata },
    Installed { version: String },
    RolledBack { version: String },
    Failed { message: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeUpgradeOutcome {
    Installed,
    RolledBack,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeUpgradeHandoff {
    pub release: ReleaseMetadata,
    pub staging_root: PathBuf,
    pub installed_executable: PathBuf,
    pub data_directory: PathBuf,
    pub config_directory: PathBuf,
    pub runtime_directory: PathBuf,
    pub client_process_ids: Vec<u32>,
    pub relaunch_desktop: bool,
}

impl ReleaseMetadata {
    fn validate(&self, context: &ReleaseValidationContext) -> Result<(), ReleaseValidationError> {
        if self.schema_version != RELEASE_SCHEMA_VERSION {
            return Err(ReleaseValidationError::UnsupportedSchema);
        }
        if !self.target.is_supported() || self.target != context.expected_target {
            return Err(ReleaseValidationError::WrongTarget);
        }
        if self.byte_length == 0 || self.byte_length > MAX_UPGRADE_PAYLOAD_BYTES {
            return Err(ReleaseValidationError::InvalidLength);
        }
        if self.sha256.len() != 64
            || !self
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(ReleaseValidationError::InvalidDigest);
        }
        validate_url(&self.installer_url)?;
        validate_url(&self.release_notes_url)?;
        if self.release_notes.len() > MAX_RELEASE_NOTES_BYTES {
            return Err(ReleaseValidationError::OversizedReleaseNotes);
        }
        if self.published_at_unix_seconds < context.oldest_allowed_publication
            || self.published_at_unix_seconds > context.now_unix_seconds.saturating_add(300)
        {
            return Err(ReleaseValidationError::StalePublication);
        }
        if context
            .newest_seen_publication
            .is_some_and(|seen| self.published_at_unix_seconds < seen)
        {
            return Err(ReleaseValidationError::ReplayedPublication);
        }

        let current = Version::parse(&context.current_version)
            .map_err(|_| ReleaseValidationError::InvalidVersion)?;
        let minimum = Version::parse(&self.minimum_source_version)
            .map_err(|_| ReleaseValidationError::InvalidVersion)?;
        let target = Version::parse(&self.target_version)
            .map_err(|_| ReleaseValidationError::InvalidVersion)?;
        if current < minimum {
            return Err(ReleaseValidationError::UnsupportedSourceVersion);
        }
        if target == current {
            return Err(ReleaseValidationError::CurrentVersion);
        }
        if target < current {
            return Err(ReleaseValidationError::Downgrade);
        }
        Ok(())
    }
}

fn validate_url(value: &str) -> Result<(), ReleaseValidationError> {
    if value.len() > MAX_URL_BYTES {
        return Err(ReleaseValidationError::InvalidUrl);
    }
    let url = Url::parse(value).map_err(|_| ReleaseValidationError::InvalidUrl)?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ReleaseValidationError::InvalidUrl);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ReleaseValidationError {
    #[error("release metadata is malformed")]
    Malformed,
    #[error("release metadata exceeds the size limit")]
    OversizedMetadata,
    #[error("release notes exceed the size limit")]
    OversizedReleaseNotes,
    #[error("release metadata uses an unsupported schema")]
    UnsupportedSchema,
    #[error("release metadata signature is invalid")]
    InvalidSignature,
    #[error("release target does not match this installation")]
    WrongTarget,
    #[error("release payload length is invalid")]
    InvalidLength,
    #[error("release digest is invalid")]
    InvalidDigest,
    #[error("release URL is invalid")]
    InvalidUrl,
    #[error("release version is invalid")]
    InvalidVersion,
    #[error("release does not support this installed version")]
    UnsupportedSourceVersion,
    #[error("release matches the installed version")]
    CurrentVersion,
    #[error("release would not upgrade the installed version")]
    Downgrade,
    #[error("release publication time is outside the accepted window")]
    StalePublication,
    #[error("release metadata is older than a previously accepted publication")]
    ReplayedPublication,
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::{Signer, SigningKey};

    use super::{
        InstallerKind, ReleaseArchitecture, ReleaseMetadata, ReleasePlatform, ReleaseTarget,
        ReleaseValidationContext, SignedReleaseMetadata,
    };

    const NOW: i64 = 1_800_000_000;

    fn target() -> ReleaseTarget {
        ReleaseTarget {
            platform: ReleasePlatform::Linux,
            architecture: ReleaseArchitecture::X86_64,
            installer: InstallerKind::LinuxAppImage,
        }
    }

    fn signed_release() -> (Vec<u8>, ed25519_dalek::VerifyingKey) {
        let key = SigningKey::from_bytes(&[7; 32]);
        let metadata = ReleaseMetadata {
            schema_version: 1,
            minimum_source_version: "0.1.0".into(),
            target_version: "0.2.0".into(),
            target: target(),
            installer_url: "https://github.com/devemit/sylvops/releases/download/v0.2.0/sylvops-linux-x86_64.AppImage".into(),
            byte_length: 12_345,
            sha256: "11".repeat(32),
            release_notes_url: "https://github.com/devemit/sylvops/releases/tag/v0.2.0".into(),
            release_notes: "Health-checked application upgrades.".into(),
            published_at_unix_seconds: NOW,
        };
        let signature = key.sign(&metadata.signed_bytes().unwrap());
        let envelope = SignedReleaseMetadata::new(metadata, signature.to_bytes());
        (serde_json::to_vec(&envelope).unwrap(), key.verifying_key())
    }

    fn context() -> ReleaseValidationContext {
        ReleaseValidationContext {
            current_version: "0.1.0".into(),
            expected_target: target(),
            now_unix_seconds: NOW,
            oldest_allowed_publication: NOW - 86_400,
            newest_seen_publication: None,
        }
    }

    #[test]
    fn signed_compatible_release_is_available() {
        let (encoded, key) = signed_release();

        let release = SignedReleaseMetadata::decode_and_validate(&encoded, &key, &context())
            .expect("valid signed release");

        assert_eq!(release.target_version, "0.2.0");
        assert_eq!(release.byte_length, 12_345);
    }

    #[test]
    fn current_release_is_distinct_from_a_downgrade() {
        let (encoded, key) = signed_release();
        let mut current = context();
        current.current_version = "0.2.0".into();

        assert!(matches!(
            SignedReleaseMetadata::decode_and_validate(&encoded, &key, &current),
            Err(super::ReleaseValidationError::CurrentVersion)
        ));
        assert!(
            SignedReleaseMetadata::decode_and_validate_for_publication(&encoded, &key, &current)
                .is_ok()
        );

        current.current_version = "0.3.0".into();
        assert!(matches!(
            SignedReleaseMetadata::decode_and_validate(&encoded, &key, &current),
            Err(super::ReleaseValidationError::Downgrade)
        ));
    }

    #[test]
    fn tampering_wrong_target_downgrade_stale_and_oversized_metadata_fail_closed() {
        let (encoded, key) = signed_release();

        let mut tampered = encoded.clone();
        let digit = tampered
            .iter()
            .position(|byte| *byte == b'2')
            .expect("version digit");
        tampered[digit] = b'3';
        assert!(SignedReleaseMetadata::decode_and_validate(&tampered, &key, &context()).is_err());

        let mut wrong_target = context();
        wrong_target.expected_target.architecture = ReleaseArchitecture::Aarch64;
        assert!(SignedReleaseMetadata::decode_and_validate(&encoded, &key, &wrong_target).is_err());

        let mut downgrade = context();
        downgrade.current_version = "0.3.0".into();
        assert!(SignedReleaseMetadata::decode_and_validate(&encoded, &key, &downgrade).is_err());

        let mut stale = context();
        stale.oldest_allowed_publication = NOW + 1;
        assert!(SignedReleaseMetadata::decode_and_validate(&encoded, &key, &stale).is_err());

        let oversized = vec![b'x'; super::MAX_RELEASE_METADATA_BYTES + 1];
        assert!(SignedReleaseMetadata::decode_and_validate(&oversized, &key, &context()).is_err());
    }

    #[test]
    fn release_urls_refuse_unauthenticated_query_or_fragment_data() {
        let (encoded, key) = signed_release();
        let mut envelope: SignedReleaseMetadata = serde_json::from_slice(&encoded).unwrap();
        envelope.release.installer_url.push_str("?token=untrusted");
        let signature =
            SigningKey::from_bytes(&[7; 32]).sign(&envelope.release.signed_bytes().unwrap());
        envelope = SignedReleaseMetadata::new(envelope.release, signature.to_bytes());

        assert!(matches!(
            SignedReleaseMetadata::decode_and_validate(
                &serde_json::to_vec(&envelope).unwrap(),
                &key,
                &context()
            ),
            Err(super::ReleaseValidationError::InvalidUrl)
        ));
    }
}
