//! Daemon-owned application upgrade coordination.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use ed25519_dalek::VerifyingKey;
use futures_util::StreamExt;
use reqwest::{Client, redirect};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use sylvops_core::upgrade::{
    InstallerKind, MAX_UPGRADE_PAYLOAD_BYTES, NativeUpgradeOutcome, ReleaseArchitecture,
    ReleaseMetadata, ReleasePlatform, ReleaseTarget, ReleaseValidationContext,
    ReleaseValidationError, SignedReleaseMetadata, UpgradeStatus,
};
use thiserror::Error;
use tokio::sync::RwLock;
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncWriteExt},
};

const RELEASE_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
const RELEASE_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const CANCELLATION_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(25);
const MAX_REDIRECTS: usize = 5;
const MAX_PUBLICATION_WATERMARK_BYTES: usize = 32;
const PUBLICATION_WATERMARK_FILE: &str = "publication-watermark";
const ALLOWED_RELEASE_HOSTS: &[&str] = &[
    "github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
];
const DEVELOPMENT_PUBLIC_KEY: [u8; 32] = [
    0xd7, 0x5a, 0x98, 0x01, 0x82, 0xb1, 0x0a, 0xb7, 0xd5, 0x4b, 0xfe, 0xd3, 0xc9, 0x64, 0x07, 0x3a,
    0x0e, 0xe1, 0x72, 0xf3, 0xda, 0xa6, 0x23, 0x25, 0xaf, 0x02, 0x1a, 0x68, 0xf7, 0x07, 0x51, 0x1a,
];

fn is_allowed_release_url(url: &reqwest::Url) -> bool {
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port_or_known_default() == Some(443)
        && url
            .host_str()
            .is_some_and(|host| ALLOWED_RELEASE_HOSTS.contains(&host))
}

#[derive(Debug)]
pub struct HttpReleaseSource {
    client: Client,
    metadata_url: String,
}

impl HttpReleaseSource {
    /// Creates a credential-free, bounded GitHub release source.
    ///
    /// # Errors
    ///
    /// Refuses non-HTTPS or non-GitHub metadata endpoints and invalid client configuration.
    pub fn github(metadata_url: &str) -> Result<Self, UpgradeError> {
        let url = reqwest::Url::parse(metadata_url)
            .map_err(|_| UpgradeError::ReleaseService("release endpoint is invalid".into()))?;
        if !is_allowed_release_url(&url) {
            return Err(UpgradeError::ReleaseService(
                "release endpoint is outside the allowed GitHub origin".into(),
            ));
        }
        let client = Client::builder()
            .connect_timeout(RELEASE_CONNECT_TIMEOUT)
            .timeout(RELEASE_REQUEST_TIMEOUT)
            .redirect(redirect::Policy::custom(|attempt| {
                if attempt.previous().len() >= MAX_REDIRECTS {
                    return attempt.error("release redirect limit exceeded");
                }
                if is_allowed_release_url(attempt.url()) {
                    attempt.follow()
                } else {
                    attempt.stop()
                }
            }))
            .build()
            .map_err(|_| UpgradeError::ReleaseService("release client is unavailable".into()))?;
        Ok(Self {
            client,
            metadata_url: metadata_url.to_owned(),
        })
    }

    async fn bounded_response(
        response: reqwest::Response,
        maximum_bytes: u64,
    ) -> Result<Vec<u8>, UpgradeError> {
        if !response.status().is_success() {
            return Err(UpgradeError::ReleaseService(format!(
                "release service returned HTTP {}",
                response.status().as_u16()
            )));
        }
        if response
            .content_length()
            .is_some_and(|length| length > maximum_bytes)
        {
            return Err(UpgradeError::ReleaseService(
                "release response exceeds its byte limit".into(),
            ));
        }
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk
                .map_err(|_| UpgradeError::ReleaseService("release transfer failed".into()))?;
            if u64::try_from(bytes.len().saturating_add(chunk.len()))
                .map_or(true, |length| length > maximum_bytes)
            {
                return Err(UpgradeError::ReleaseService(
                    "release response exceeds its byte limit".into(),
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
}

/// Returns the installed package target used to select release metadata.
///
/// # Errors
///
/// Refuses architectures outside the supported application-release matrix.
pub fn current_release_target() -> Result<ReleaseTarget, UpgradeError> {
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    return Ok(ReleaseTarget {
        platform: ReleasePlatform::Windows,
        architecture: ReleaseArchitecture::X86_64,
        installer: InstallerKind::WindowsNsis,
    });
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    return Ok(ReleaseTarget {
        platform: ReleasePlatform::Macos,
        architecture: ReleaseArchitecture::X86_64,
        installer: InstallerKind::MacosDmg,
    });
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    return Ok(ReleaseTarget {
        platform: ReleasePlatform::Macos,
        architecture: ReleaseArchitecture::Aarch64,
        installer: InstallerKind::MacosDmg,
    });
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    return Ok(ReleaseTarget {
        platform: ReleasePlatform::Linux,
        architecture: ReleaseArchitecture::X86_64,
        installer: if std::env::var_os("APPIMAGE").is_some() {
            InstallerKind::LinuxAppImage
        } else {
            InstallerKind::LinuxDeb
        },
    });
    #[allow(unreachable_code)]
    Err(UpgradeError::ReleaseService(
        "this platform has no supported application package".into(),
    ))
}

#[must_use]
pub fn manifest_url(target: ReleaseTarget) -> String {
    let suffix = match target.installer {
        InstallerKind::WindowsNsis => "windows-x86_64-nsis",
        InstallerKind::MacosDmg if target.architecture == ReleaseArchitecture::X86_64 => {
            "macos-x86_64-dmg"
        }
        InstallerKind::MacosDmg => "macos-aarch64-dmg",
        InstallerKind::LinuxAppImage => "linux-x86_64-appimage",
        InstallerKind::LinuxDeb => "linux-x86_64-deb",
    };
    format!(
        "https://github.com/devemit/sylvops/releases/latest/download/sylvops-update-{suffix}.json"
    )
}

/// Reads the build-embedded Ed25519 release key.
///
/// Tagged release validation requires an explicit protected key value; ordinary development
/// builds use a non-production key so their update checks can only fail closed.
///
/// # Errors
///
/// Returns an error when the embedded value is not a valid base64-encoded Ed25519 public key.
pub fn embedded_verifying_key() -> Result<VerifyingKey, UpgradeError> {
    let bytes = if let Some(encoded) = option_env!("SYLVOPS_UPDATE_PUBLIC_KEY_BASE64") {
        STANDARD
            .decode(encoded)
            .map_err(|_| UpgradeError::ReleaseService("embedded update key is invalid".into()))?
    } else {
        DEVELOPMENT_PUBLIC_KEY.to_vec()
    };
    let bytes: [u8; 32] = bytes.try_into().map_err(|_| {
        UpgradeError::ReleaseService("embedded update key has invalid length".into())
    })?;
    VerifyingKey::from_bytes(&bytes)
        .map_err(|_| UpgradeError::ReleaseService("embedded update key is invalid".into()))
}

#[async_trait]
impl ReleaseSource for HttpReleaseSource {
    async fn metadata(&self) -> Result<Vec<u8>, UpgradeError> {
        let response = self
            .client
            .get(&self.metadata_url)
            .send()
            .await
            .map_err(|_| UpgradeError::ReleaseService("release metadata request failed".into()))?;
        Self::bounded_response(
            response,
            u64::try_from(sylvops_core::upgrade::MAX_RELEASE_METADATA_BYTES)
                .expect("metadata limit fits u64"),
        )
        .await
    }

    async fn download(
        &self,
        url: &str,
        destination: &Path,
        maximum_bytes: u64,
        cancelled: &AtomicBool,
    ) -> Result<(), UpgradeError> {
        let parsed = reqwest::Url::parse(url)
            .map_err(|_| UpgradeError::ReleaseService("payload URL is invalid".into()))?;
        if !is_allowed_release_url(&parsed) {
            return Err(UpgradeError::ReleaseService(
                "payload URL is outside the allowed GitHub origin".into(),
            ));
        }
        let response = tokio::select! {
            response = self.client.get(parsed).send() => response
                .map_err(|_| UpgradeError::ReleaseService("payload request failed".into()))?,
            () = wait_until_cancelled(cancelled) => return Err(UpgradeError::Cancelled),
        };
        if !response.status().is_success()
            || response
                .content_length()
                .is_some_and(|length| length > maximum_bytes)
        {
            return Err(UpgradeError::ReleaseService(
                "payload response was refused".into(),
            ));
        }
        write_payload_response(response, destination, maximum_bytes, cancelled).await
    }
}

async fn write_payload_response(
    response: reqwest::Response,
    destination: &Path,
    maximum_bytes: u64,
    cancelled: &AtomicBool,
) -> Result<(), UpgradeError> {
    let mut output = File::create(destination).await?;
    let mut received = 0_u64;
    let mut stream = response.bytes_stream();
    loop {
        let next = tokio::select! {
            next = stream.next() => next,
            () = wait_until_cancelled(cancelled) => return Err(UpgradeError::Cancelled),
        };
        let Some(chunk) = next else {
            break;
        };
        if cancelled.load(Ordering::Acquire) {
            return Err(UpgradeError::Cancelled);
        }
        let chunk =
            chunk.map_err(|_| UpgradeError::ReleaseService("payload transfer failed".into()))?;
        received =
            received
                .checked_add(u64::try_from(chunk.len()).map_err(|_| {
                    UpgradeError::ReleaseService("payload chunk is oversized".into())
                })?)
                .ok_or_else(|| UpgradeError::ReleaseService("payload is oversized".into()))?;
        if received > maximum_bytes {
            return Err(UpgradeError::ReleaseService("payload is oversized".into()));
        }
        output.write_all(&chunk).await?;
    }
    output.flush().await?;
    Ok(())
}

async fn wait_until_cancelled(cancelled: &AtomicBool) {
    while !cancelled.load(Ordering::Acquire) {
        tokio::time::sleep(CANCELLATION_POLL_INTERVAL).await;
    }
}

pub use sylvops_core::upgrade::{ActiveUpgradeSession, InstallDisposition};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StagedUpgrade {
    pub release: ReleaseMetadata,
    pub payload: PathBuf,
}

#[cfg(test)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HealthCheck {
    Passed,
    Failed,
}

#[cfg(test)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HealthReport {
    pub application_version: String,
    pub daemon_version: String,
    pub protocol: HealthCheck,
    pub database: HealthCheck,
    pub executable_identity: HealthCheck,
    pub platform_signature: HealthCheck,
}

#[cfg(test)]
impl HealthReport {
    fn is_healthy(&self, release: &ReleaseMetadata) -> bool {
        self.application_version == release.target_version
            && self.daemon_version == release.target_version
            && self.protocol == HealthCheck::Passed
            && self.database == HealthCheck::Passed
            && self.executable_identity == HealthCheck::Passed
            && self.platform_signature == HealthCheck::Passed
    }
}

#[async_trait]
pub trait ReleaseSource: std::fmt::Debug + Send + Sync {
    async fn metadata(&self) -> Result<Vec<u8>, UpgradeError>;

    async fn download(
        &self,
        url: &str,
        destination: &Path,
        maximum_bytes: u64,
        cancelled: &AtomicBool,
    ) -> Result<(), UpgradeError>;
}

#[cfg(test)]
#[async_trait]
pub trait PlatformInstaller: std::fmt::Debug + Send + Sync {
    async fn apply(
        &self,
        payload: &Path,
        release: &ReleaseMetadata,
    ) -> Result<HealthReport, UpgradeError>;

    async fn rollback(&self, release: &ReleaseMetadata) -> Result<(), UpgradeError>;
}

#[derive(Clone, Debug)]
struct CoordinatorState {
    available: Option<ReleaseMetadata>,
    metadata_bytes: Option<Vec<u8>>,
    staged: Option<StagedUpgrade>,
    status: UpgradeStatus,
}

impl Default for CoordinatorState {
    fn default() -> Self {
        Self {
            available: None,
            metadata_bytes: None,
            staged: None,
            status: UpgradeStatus::Idle,
        }
    }
}

#[derive(Debug)]
pub struct UpgradeCoordinator {
    staging_root: PathBuf,
    source: Box<dyn ReleaseSource>,
    #[cfg(test)]
    installer: Option<Box<dyn PlatformInstaller>>,
    verifying_key: VerifyingKey,
    validation: ReleaseValidationContext,
    state: Arc<RwLock<CoordinatorState>>,
    cancelled: Arc<AtomicBool>,
    installation_bound: bool,
    started: Instant,
}

impl UpgradeCoordinator {
    #[must_use]
    pub fn new(
        staging_root: PathBuf,
        source: Box<dyn ReleaseSource>,
        verifying_key: VerifyingKey,
        validation: ReleaseValidationContext,
    ) -> Self {
        Self {
            staging_root,
            source,
            #[cfg(test)]
            installer: None,
            verifying_key,
            validation,
            state: Arc::new(RwLock::new(CoordinatorState::default())),
            cancelled: Arc::new(AtomicBool::new(false)),
            installation_bound: false,
            started: Instant::now(),
        }
    }

    /// Creates a coordinator after proving its staging root cannot overlap the active installation.
    ///
    /// # Errors
    ///
    /// Refuses relative, unresolved, or overlapping installation and staging paths.
    pub fn new_for_installation(
        staging_root: &Path,
        installed_executable: &Path,
        source: Box<dyn ReleaseSource>,
        verifying_key: VerifyingKey,
        validation: ReleaseValidationContext,
    ) -> Result<Self, UpgradeError> {
        let staging_root = validate_staging_location(staging_root, installed_executable)?;
        let mut coordinator = Self::new(staging_root, source, verifying_key, validation);
        coordinator.installation_bound = true;
        Ok(coordinator)
    }

    #[cfg(test)]
    fn new_for_test(
        staging_root: PathBuf,
        source: Box<dyn ReleaseSource>,
        installer: Box<dyn PlatformInstaller>,
        verifying_key: VerifyingKey,
        validation: ReleaseValidationContext,
    ) -> Self {
        Self {
            staging_root,
            source,
            installer: Some(installer),
            verifying_key,
            validation,
            state: Arc::new(RwLock::new(CoordinatorState::default())),
            cancelled: Arc::new(AtomicBool::new(false)),
            installation_bound: false,
            started: Instant::now(),
        }
    }

    /// Fetches and verifies bounded release metadata without downloading the payload.
    ///
    /// # Errors
    ///
    /// Returns a redacted error for transport, parsing, or validation failures.
    pub async fn check(&self) -> Result<Option<ReleaseMetadata>, UpgradeError> {
        let bytes = self.source.metadata().await?;
        let mut validation = self.validation.clone();
        let elapsed = i64::try_from(self.started.elapsed().as_secs()).unwrap_or(i64::MAX);
        validation.now_unix_seconds = validation.now_unix_seconds.saturating_add(elapsed);
        validation.oldest_allowed_publication = validation
            .oldest_allowed_publication
            .saturating_add(elapsed);
        let in_memory_watermark = self
            .state
            .read()
            .await
            .available
            .as_ref()
            .map(|release| release.published_at_unix_seconds);
        validation.newest_seen_publication = match (
            in_memory_watermark,
            self.read_publication_watermark().await?,
        ) {
            (Some(left), Some(right)) => Some(left.max(right)),
            (watermark @ Some(_), None) | (None, watermark @ Some(_)) => watermark,
            (None, None) => None,
        };
        let release = SignedReleaseMetadata::decode_and_validate_for_publication(
            &bytes,
            &self.verifying_key,
            &validation,
        )?;
        self.persist_publication_watermark(release.published_at_unix_seconds)
            .await?;
        if release.target_version == validation.current_version {
            let mut state = self.state.write().await;
            state.available = None;
            state.metadata_bytes = None;
            state.staged = None;
            state.status = UpgradeStatus::UpToDate {
                version: validation.current_version,
            };
            return Ok(None);
        }
        let mut state = self.state.write().await;
        state.available = Some(release.clone());
        state.metadata_bytes = Some(bytes);
        state.staged = None;
        state.status = UpgradeStatus::Available {
            release: release.clone(),
        };
        Ok(Some(release))
    }

    async fn read_publication_watermark(&self) -> Result<Option<i64>, UpgradeError> {
        let path = self.staging_root.join(PUBLICATION_WATERMARK_FILE);
        if !tokio::fs::try_exists(&path).await? {
            return Ok(None);
        }
        let bytes = read_bounded_file(&path, MAX_PUBLICATION_WATERMARK_BYTES).await?;
        let value = std::str::from_utf8(&bytes)
            .ok()
            .and_then(|text| text.parse::<i64>().ok())
            .filter(|value| *value >= 0)
            .ok_or(UpgradeError::InvalidPublicationWatermark)?;
        Ok(Some(value))
    }

    async fn persist_publication_watermark(&self, value: i64) -> Result<(), UpgradeError> {
        prepare_staging_root(&self.staging_root, self.installation_bound).await?;
        crate::atomic_file::write(
            &self.staging_root.join(PUBLICATION_WATERMARK_FILE),
            value.to_string().as_bytes(),
        )
        .map_err(|_| UpgradeError::PublicationWatermarkPersistence)
    }

    /// Downloads and verifies the selected payload before atomically marking it staged.
    ///
    /// # Errors
    ///
    /// Returns an error without changing the active installation when transfer or verification fails.
    pub async fn download(&self) -> Result<StagedUpgrade, UpgradeError> {
        self.cancelled.store(false, Ordering::Release);
        let (release, metadata_bytes) = {
            let state = self.state.read().await;
            (
                state
                    .available
                    .clone()
                    .ok_or(UpgradeError::NoAvailableRelease)?,
                state
                    .metadata_bytes
                    .clone()
                    .ok_or(UpgradeError::NoAvailableRelease)?,
            )
        };
        self.state.write().await.status = UpgradeStatus::Downloading {
            release: release.clone(),
        };
        let partial = self.staging_root.join("payload.partial");
        let payload = self.staging_root.join("payload.staged");
        let metadata_partial = self.staging_root.join("release.partial");
        let metadata = self.staging_root.join("release.json");
        let result = async {
            prepare_staging_root(&self.staging_root, self.installation_bound).await?;
            remove_file_if_present(&partial).await?;
            remove_file_if_present(&metadata_partial).await?;
            self.source
                .download(
                    &release.installer_url,
                    &partial,
                    release.byte_length.min(MAX_UPGRADE_PAYLOAD_BYTES),
                    &self.cancelled,
                )
                .await?;
            if self.cancelled.load(Ordering::Acquire) {
                return Err(UpgradeError::Cancelled);
            }
            verify_payload(&partial, &release).await?;
            tokio::fs::write(&metadata_partial, metadata_bytes).await?;
            remove_file_if_present(&payload).await?;
            remove_file_if_present(&metadata).await?;
            tokio::fs::rename(&partial, &payload).await?;
            if let Err(error) = tokio::fs::rename(&metadata_partial, &metadata).await {
                let _ = remove_file_if_present(&payload).await;
                return Err(error.into());
            }
            Ok(StagedUpgrade {
                release: release.clone(),
                payload: payload.clone(),
            })
        }
        .await;
        let staged = match result {
            Ok(staged) => staged,
            Err(error) => {
                for path in [&partial, &metadata_partial, &payload, &metadata] {
                    let _ = remove_file_if_present(path).await;
                }
                let mut state = self.state.write().await;
                state.staged = None;
                state.status = UpgradeStatus::Available {
                    release: release.clone(),
                };
                return Err(error);
            }
        };
        if self.cancelled.load(Ordering::Acquire) {
            let _ = remove_file_if_present(&partial).await;
            let _ = remove_file_if_present(&metadata_partial).await;
            let _ = remove_file_if_present(&payload).await;
            let _ = remove_file_if_present(&metadata).await;
            let mut state = self.state.write().await;
            state.staged = None;
            state.status = UpgradeStatus::Available { release };
            return Err(UpgradeError::Cancelled);
        }
        self.state.write().await.staged = Some(staged.clone());
        self.state.write().await.status = UpgradeStatus::Staged {
            release: staged.release.clone(),
        };
        Ok(staged)
    }

    /// Restores a previously verified staged payload after a daemon restart.
    ///
    /// # Errors
    ///
    /// Corrupt or incompatible staged state is removed before an error is returned.
    pub async fn recover_staged(&self) -> Result<bool, UpgradeError> {
        prepare_staging_root(&self.staging_root, self.installation_bound).await?;
        remove_file_if_present(&self.staging_root.join("payload.partial")).await?;
        remove_file_if_present(&self.staging_root.join("release.partial")).await?;
        let metadata_path = self.staging_root.join("release.json");
        let payload = self.staging_root.join("payload.staged");
        if !tokio::fs::try_exists(&metadata_path).await? || !tokio::fs::try_exists(&payload).await?
        {
            remove_file_if_present(&metadata_path).await?;
            remove_file_if_present(&payload).await?;
            return Ok(false);
        }
        let result = async {
            let bytes = read_bounded_file(
                &metadata_path,
                sylvops_core::upgrade::MAX_RELEASE_METADATA_BYTES,
            )
            .await?;
            let mut validation = self.validation.clone();
            let elapsed = i64::try_from(self.started.elapsed().as_secs()).unwrap_or(i64::MAX);
            validation.now_unix_seconds = validation.now_unix_seconds.saturating_add(elapsed);
            validation.oldest_allowed_publication = validation
                .oldest_allowed_publication
                .saturating_add(elapsed);
            let release = SignedReleaseMetadata::decode_and_validate(
                &bytes,
                &self.verifying_key,
                &validation,
            )?;
            verify_payload(&payload, &release).await?;
            Ok::<_, UpgradeError>((bytes, release))
        }
        .await;
        let (bytes, release) = match result {
            Ok(recovered) => recovered,
            Err(error) => {
                let _ = remove_file_if_present(&metadata_path).await;
                let _ = remove_file_if_present(&payload).await;
                return Err(error);
            }
        };
        let staged = StagedUpgrade {
            release: release.clone(),
            payload,
        };
        let mut state = self.state.write().await;
        state.available = Some(release.clone());
        state.metadata_bytes = Some(bytes);
        state.staged = Some(staged);
        state.status = UpgradeStatus::Staged { release };
        Ok(true)
    }

    pub fn cancel_download(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    /// Verifies a staged upgrade and applies the active-session policy before native handoff.
    ///
    /// # Errors
    ///
    /// Returns an error when no verified payload is staged or another lifecycle mutation is active.
    pub async fn prepare_install(
        &self,
        active_sessions: &[ActiveUpgradeSession],
        override_confirmed: bool,
    ) -> Result<InstallDisposition, UpgradeError> {
        if !active_sessions.is_empty() && !override_confirmed {
            return Ok(InstallDisposition::Blocked {
                active_sessions: active_sessions.to_vec(),
            });
        }
        let staged = self
            .state
            .read()
            .await
            .staged
            .clone()
            .ok_or(UpgradeError::NoStagedRelease)?;
        verify_payload(&staged.payload, &staged.release).await?;
        self.state.write().await.status = UpgradeStatus::Installing {
            release: staged.release.clone(),
        };
        Ok(InstallDisposition::Prepared {
            version: staged.release.target_version,
        })
    }

    /// Applies a staged upgrade only after active-session policy allows it.
    ///
    /// # Errors
    ///
    /// Returns an error when no verified payload exists or the installer/rollback fails.
    #[cfg(test)]
    async fn install(
        &self,
        active_sessions: &[ActiveUpgradeSession],
        override_confirmed: bool,
    ) -> Result<InstallDisposition, UpgradeError> {
        if !active_sessions.is_empty() && !override_confirmed {
            return Ok(InstallDisposition::Blocked {
                active_sessions: active_sessions.to_vec(),
            });
        }
        let staged = self
            .state
            .read()
            .await
            .staged
            .clone()
            .ok_or(UpgradeError::NoStagedRelease)?;
        self.state.write().await.status = UpgradeStatus::Installing {
            release: staged.release.clone(),
        };
        verify_payload(&staged.payload, &staged.release).await?;
        let health = self
            .installer
            .as_ref()
            .expect("test installer is configured")
            .apply(&staged.payload, &staged.release)
            .await?;
        if health.is_healthy(&staged.release) {
            self.state.write().await.status = UpgradeStatus::Installed {
                version: staged.release.target_version.clone(),
            };
            return Ok(InstallDisposition::Installed);
        }
        self.installer
            .as_ref()
            .expect("test installer is configured")
            .rollback(&staged.release)
            .await?;
        self.state.write().await.status = UpgradeStatus::RolledBack {
            version: staged.release.target_version.clone(),
        };
        Ok(InstallDisposition::RolledBack)
    }

    pub async fn status(&self) -> UpgradeStatus {
        self.state.read().await.status.clone()
    }

    pub async fn cancel_prepared_install(&self) {
        let mut state = self.state.write().await;
        if matches!(state.status, UpgradeStatus::Installing { .. })
            && let Some(staged) = &state.staged
        {
            state.status = UpgradeStatus::Staged {
                release: staged.release.clone(),
            };
        }
    }

    /// Records the detached helper's authenticated terminal outcome.
    ///
    /// # Errors
    ///
    /// Refuses outcomes that do not agree with the running or staged version.
    pub async fn record_outcome(
        &self,
        version: &str,
        outcome: NativeUpgradeOutcome,
    ) -> Result<(), UpgradeError> {
        let mut state = self.state.write().await;
        if matches!(
            (&state.status, outcome),
            (
                UpgradeStatus::Installed {
                    version: installed_version
                },
                NativeUpgradeOutcome::Installed
            ) if installed_version == version
        ) || matches!(
            (&state.status, outcome),
            (
                UpgradeStatus::RolledBack {
                    version: rolled_back_version
                },
                NativeUpgradeOutcome::RolledBack
            ) if rolled_back_version == version
        ) {
            return Ok(());
        }
        let staged_version = state
            .staged
            .as_ref()
            .map(|staged| staged.release.target_version.as_str());
        match outcome {
            NativeUpgradeOutcome::Installed if version == env!("CARGO_PKG_VERSION") => {
                state.status = UpgradeStatus::Installed {
                    version: version.into(),
                };
            }
            NativeUpgradeOutcome::RolledBack if staged_version == Some(version) => {
                state.status = UpgradeStatus::RolledBack {
                    version: version.into(),
                };
            }
            _ => return Err(UpgradeError::OutcomeMismatch),
        }
        state.available = None;
        state.metadata_bytes = None;
        state.staged = None;
        drop(state);
        remove_file_if_present(&self.staging_root.join("release.json")).await?;
        remove_file_if_present(&self.staging_root.join("payload.staged")).await?;
        Ok(())
    }
}

fn validate_staging_location(
    staging_root: &Path,
    installed_executable: &Path,
) -> Result<PathBuf, UpgradeError> {
    if !staging_root.is_absolute() || !installed_executable.is_absolute() {
        return Err(UpgradeError::UnsafeStagingPath);
    }
    let staging_parent = staging_root
        .parent()
        .ok_or(UpgradeError::UnsafeStagingPath)?;
    let staging_name = staging_root
        .file_name()
        .ok_or(UpgradeError::UnsafeStagingPath)?;
    let staging_root = if staging_root.exists() {
        std::fs::canonicalize(staging_root)?
    } else {
        std::fs::canonicalize(staging_parent)?.join(staging_name)
    };
    let installed_executable = std::fs::canonicalize(installed_executable)?;
    let installation_root =
        active_installation_root(&installed_executable).ok_or(UpgradeError::UnsafeStagingPath)?;
    if staging_root.starts_with(installation_root) || installation_root.starts_with(&staging_root) {
        return Err(UpgradeError::UnsafeStagingPath);
    }
    Ok(staging_root)
}

#[cfg(target_os = "macos")]
fn active_installation_root(executable: &Path) -> Option<&Path> {
    executable
        .ancestors()
        .find(|path| path.extension().is_some_and(|extension| extension == "app"))
        .or_else(|| executable.parent())
}

#[cfg(not(target_os = "macos"))]
fn active_installation_root(executable: &Path) -> Option<&Path> {
    executable.parent()
}

async fn prepare_staging_root(
    path: &Path,
    require_canonical_match: bool,
) -> Result<(), UpgradeError> {
    tokio::fs::create_dir_all(path).await?;
    let metadata = tokio::fs::symlink_metadata(path).await?;
    if unsafe_staging_entry(&metadata)
        || require_canonical_match && tokio::fs::canonicalize(path).await? != path
    {
        return Err(UpgradeError::UnsafeStagingPath);
    }
    Ok(())
}

#[cfg(unix)]
fn unsafe_staging_entry(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
fn unsafe_staging_entry(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

pub(crate) async fn verify_payload(
    path: &Path,
    release: &ReleaseMetadata,
) -> Result<(), UpgradeError> {
    let mut file = File::open(path).await?;
    let mut digest = Sha256::new();
    let mut length = 0_u64;
    let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        length = length
            .checked_add(u64::try_from(read).expect("buffer length fits u64"))
            .ok_or(UpgradeError::LengthMismatch)?;
        if length > release.byte_length {
            return Err(UpgradeError::LengthMismatch);
        }
        digest.update(&buffer[..read]);
    }
    if length != release.byte_length {
        return Err(UpgradeError::LengthMismatch);
    }
    let actual = format!("{:x}", digest.finalize());
    if actual
        .as_bytes()
        .ct_eq(release.sha256.as_bytes())
        .unwrap_u8()
        != 1
    {
        return Err(UpgradeError::DigestMismatch);
    }
    Ok(())
}

pub(crate) async fn read_bounded_file(
    path: &Path,
    maximum_bytes: usize,
) -> Result<Vec<u8>, UpgradeError> {
    let metadata = tokio::fs::symlink_metadata(path).await?;
    if metadata.file_type().is_symlink() {
        return Err(UpgradeError::UnsafeStagingPath);
    }
    if metadata.len() > u64::try_from(maximum_bytes).unwrap_or(u64::MAX) {
        return Err(ReleaseValidationError::OversizedMetadata.into());
    }
    let file = File::open(path).await?;
    let limit = u64::try_from(maximum_bytes)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let mut reader = file.take(limit);
    let mut bytes = Vec::with_capacity(maximum_bytes.min(64 * 1024));
    reader.read_to_end(&mut bytes).await?;
    if bytes.len() > maximum_bytes {
        return Err(ReleaseValidationError::OversizedMetadata.into());
    }
    Ok(bytes)
}

async fn remove_file_if_present(path: &Path) -> Result<(), UpgradeError> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[derive(Debug, Error)]
pub enum UpgradeError {
    #[error("no compatible application upgrade has been selected")]
    NoAvailableRelease,
    #[error("no verified application upgrade is staged")]
    NoStagedRelease,
    #[error("application upgrade download was cancelled")]
    Cancelled,
    #[error("upgrade staging path is unsafe")]
    UnsafeStagingPath,
    #[error("upgrade payload length did not match signed metadata")]
    LengthMismatch,
    #[error("upgrade payload digest did not match signed metadata")]
    DigestMismatch,
    #[error("upgrade publication watermark is invalid")]
    InvalidPublicationWatermark,
    #[error("upgrade publication watermark could not be persisted")]
    PublicationWatermarkPersistence,
    #[error("upgrade metadata was refused: {0}")]
    Validation(#[from] ReleaseValidationError),
    #[error("application lifecycle I/O failed")]
    Io(#[from] std::io::Error),
    #[error("application installer failed: {0}")]
    Installer(String),
    #[error("upgrade helper outcome does not match the running installation")]
    OutcomeMismatch,
    #[error("application release service failed: {0}")]
    ReleaseService(String),
}

#[cfg(test)]
mod tests {
    use std::{
        net::Ipv4Addr,
        path::Path,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use async_trait::async_trait;
    use ed25519_dalek::{Signer, SigningKey};
    use sha2::{Digest, Sha256};
    use sylvops_core::upgrade::{
        InstallerKind, MAX_RELEASE_METADATA_BYTES, ReleaseArchitecture, ReleaseMetadata,
        ReleasePlatform, ReleaseTarget, ReleaseValidationContext, ReleaseValidationError,
        SignedReleaseMetadata, UpgradeStatus,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::{
        ActiveUpgradeSession, HealthCheck, HealthReport, HttpReleaseSource, InstallDisposition,
        PlatformInstaller, ReleaseSource, UpgradeCoordinator, UpgradeError, is_allowed_release_url,
    };

    #[test]
    fn release_redirects_remain_on_authenticated_https_origins() {
        assert!(is_allowed_release_url(
            &reqwest::Url::parse("https://github.com/devemit/sylvops").unwrap()
        ));
        for refused in [
            "http://github.com/devemit/sylvops",
            "https://user@github.com/devemit/sylvops",
            "https://github.com:444/devemit/sylvops",
            "https://example.com/devemit/sylvops",
        ] {
            assert!(!is_allowed_release_url(
                &reqwest::Url::parse(refused).unwrap()
            ));
        }
    }

    #[derive(Debug)]
    struct FakeSource {
        metadata: Vec<u8>,
        payload: Vec<u8>,
    }

    #[async_trait]
    impl ReleaseSource for FakeSource {
        async fn metadata(&self) -> Result<Vec<u8>, UpgradeError> {
            Ok(self.metadata.clone())
        }

        async fn download(
            &self,
            _url: &str,
            destination: &Path,
            _maximum_bytes: u64,
            _cancelled: &std::sync::atomic::AtomicBool,
        ) -> Result<(), UpgradeError> {
            tokio::fs::write(destination, &self.payload).await?;
            Ok(())
        }
    }

    #[derive(Debug)]
    struct StorageFailureSource {
        metadata: Vec<u8>,
    }

    #[async_trait]
    impl ReleaseSource for StorageFailureSource {
        async fn metadata(&self) -> Result<Vec<u8>, UpgradeError> {
            Ok(self.metadata.clone())
        }

        async fn download(
            &self,
            _url: &str,
            destination: &Path,
            _maximum_bytes: u64,
            _cancelled: &std::sync::atomic::AtomicBool,
        ) -> Result<(), UpgradeError> {
            tokio::fs::write(destination, b"partial payload").await?;
            Err(std::io::Error::new(
                std::io::ErrorKind::StorageFull,
                "simulated full staging volume",
            )
            .into())
        }
    }

    #[derive(Debug)]
    struct MetadataOnlySource {
        metadata: Vec<u8>,
    }

    #[async_trait]
    impl ReleaseSource for MetadataOnlySource {
        async fn metadata(&self) -> Result<Vec<u8>, UpgradeError> {
            Ok(self.metadata.clone())
        }

        async fn download(
            &self,
            _url: &str,
            _destination: &Path,
            _maximum_bytes: u64,
            _cancelled: &std::sync::atomic::AtomicBool,
        ) -> Result<(), UpgradeError> {
            panic!("release discovery must not download an upgrade payload");
        }
    }

    #[derive(Debug)]
    struct EndpointReleaseSource {
        client: reqwest::Client,
        metadata_url: String,
        payload_url: String,
    }

    #[async_trait]
    impl ReleaseSource for EndpointReleaseSource {
        async fn metadata(&self) -> Result<Vec<u8>, UpgradeError> {
            let response = self
                .client
                .get(&self.metadata_url)
                .send()
                .await
                .map_err(|_| {
                    UpgradeError::ReleaseService("release metadata request failed".into())
                })?;
            HttpReleaseSource::bounded_response(
                response,
                u64::try_from(MAX_RELEASE_METADATA_BYTES).expect("metadata limit fits u64"),
            )
            .await
        }

        async fn download(
            &self,
            _url: &str,
            destination: &Path,
            maximum_bytes: u64,
            cancelled: &std::sync::atomic::AtomicBool,
        ) -> Result<(), UpgradeError> {
            let response = self
                .client
                .get(&self.payload_url)
                .send()
                .await
                .map_err(|_| UpgradeError::ReleaseService("payload request failed".into()))?;
            if !response.status().is_success()
                || response
                    .content_length()
                    .is_some_and(|length| length > maximum_bytes)
            {
                return Err(UpgradeError::ReleaseService(
                    "payload response was refused".into(),
                ));
            }
            super::write_payload_response(response, destination, maximum_bytes, cancelled).await
        }
    }

    #[derive(Debug)]
    enum FakePayloadResponse {
        Immediate(Vec<u8>),
        Paused {
            first: Vec<u8>,
            rest: Vec<u8>,
            first_sent: Arc<tokio::sync::Notify>,
            resume: Arc<tokio::sync::Notify>,
        },
    }

    #[derive(Debug)]
    struct FakeUpgradeEndpoint {
        metadata_url: String,
        payload_url: String,
        server: Option<tokio::task::JoinHandle<()>>,
    }

    impl FakeUpgradeEndpoint {
        async fn finish(mut self) {
            let mut server = self.server.take().expect("fake upgrade endpoint task");
            if let Ok(joined) = tokio::time::timeout(Duration::from_secs(5), &mut server).await {
                joined.unwrap();
            } else {
                server.abort();
                let _ = server.await;
                panic!("fake upgrade endpoint shutdown timed out");
            }
        }
    }

    impl Drop for FakeUpgradeEndpoint {
        fn drop(&mut self) {
            if let Some(server) = &self.server {
                server.abort();
            }
        }
    }

    async fn read_fake_request(stream: &mut tokio::net::TcpStream) -> String {
        let mut request = [0_u8; 2_048];
        let read = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut request))
            .await
            .expect("fake upgrade endpoint read timed out")
            .unwrap();
        String::from_utf8_lossy(&request[..read]).into_owned()
    }

    async fn write_fake_response(stream: &mut tokio::net::TcpStream, body: &[u8]) {
        let headers = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        tokio::time::timeout(Duration::from_secs(5), stream.write_all(headers.as_bytes()))
            .await
            .expect("fake upgrade endpoint header write timed out")
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), stream.write_all(body))
            .await
            .expect("fake upgrade endpoint body write timed out")
            .unwrap();
    }

    async fn fake_upgrade_endpoint(
        metadata: Vec<u8>,
        payload: FakePayloadResponse,
    ) -> FakeUpgradeEndpoint {
        let listener = tokio::time::timeout(
            Duration::from_secs(5),
            tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)),
        )
        .await
        .expect("fake upgrade endpoint bind timed out")
        .unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut metadata_stream, _) =
                tokio::time::timeout(Duration::from_secs(5), listener.accept())
                    .await
                    .expect("fake metadata endpoint accept timed out")
                    .unwrap();
            assert!({
                let request = read_fake_request(&mut metadata_stream).await;
                assert!(!request.to_ascii_lowercase().contains("\r\nauthorization:"));
                request.starts_with("GET /release.json HTTP/1.1")
            });
            write_fake_response(&mut metadata_stream, &metadata).await;
            let _ = metadata_stream.shutdown().await;

            let (mut payload_stream, _) =
                tokio::time::timeout(Duration::from_secs(5), listener.accept())
                    .await
                    .expect("fake payload endpoint accept timed out")
                    .unwrap();
            assert!({
                let request = read_fake_request(&mut payload_stream).await;
                assert!(!request.to_ascii_lowercase().contains("\r\nauthorization:"));
                request.starts_with("GET /payload HTTP/1.1")
            });
            match payload {
                FakePayloadResponse::Immediate(body) => {
                    write_fake_response(&mut payload_stream, &body).await;
                }
                FakePayloadResponse::Paused {
                    first,
                    rest,
                    first_sent,
                    resume,
                } => {
                    let headers = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        first.len() + rest.len()
                    );
                    payload_stream.write_all(headers.as_bytes()).await.unwrap();
                    payload_stream.write_all(&first).await.unwrap();
                    payload_stream.flush().await.unwrap();
                    first_sent.notify_one();
                    tokio::time::timeout(Duration::from_secs(5), resume.notified())
                        .await
                        .expect("fake payload endpoint resume timed out");
                    let _ = payload_stream.write_all(&rest).await;
                }
            }
            let _ = payload_stream.shutdown().await;
        });
        let base_url = format!("http://{address}");
        FakeUpgradeEndpoint {
            metadata_url: format!("{base_url}/release.json"),
            payload_url: format!("{base_url}/payload"),
            server: Some(server),
        }
    }

    fn endpoint_source(endpoint: &FakeUpgradeEndpoint) -> EndpointReleaseSource {
        EndpointReleaseSource {
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(2))
                .timeout(Duration::from_secs(5))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            metadata_url: endpoint.metadata_url.clone(),
            payload_url: endpoint.payload_url.clone(),
        }
    }

    #[derive(Debug)]
    struct FakeReleaseEndpoint {
        url: String,
        server: Option<tokio::task::JoinHandle<()>>,
    }

    impl FakeReleaseEndpoint {
        async fn finish(mut self) {
            let mut server = self.server.take().expect("fake release endpoint task");
            if let Ok(joined) = tokio::time::timeout(Duration::from_secs(5), &mut server).await {
                joined.unwrap();
            } else {
                server.abort();
                let _ = server.await;
                panic!("fake release endpoint shutdown timed out");
            }
        }
    }

    impl Drop for FakeReleaseEndpoint {
        fn drop(&mut self) {
            if let Some(server) = &self.server {
                server.abort();
            }
        }
    }

    #[derive(Clone, Copy, Debug)]
    enum FakeResponseFraming {
        ContentLength,
        ConnectionClose,
    }

    async fn fake_release_endpoint(
        body: Vec<u8>,
        framing: FakeResponseFraming,
    ) -> FakeReleaseEndpoint {
        let listener = tokio::time::timeout(
            Duration::from_secs(5),
            tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)),
        )
        .await
        .expect("fake release endpoint bind timed out")
        .unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
                .await
                .expect("fake release endpoint accept timed out")
                .unwrap();
            let mut request = [0_u8; 2_048];
            let read = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut request))
                .await
                .expect("fake release endpoint read timed out")
                .unwrap();
            assert!(
                String::from_utf8_lossy(&request[..read]).starts_with("GET /release.json HTTP/1.1")
            );
            let headers = match framing {
                FakeResponseFraming::ContentLength => format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                ),
                FakeResponseFraming::ConnectionClose => {
                    "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".into()
                }
            };
            let _ =
                tokio::time::timeout(Duration::from_secs(5), stream.write_all(headers.as_bytes()))
                    .await
                    .expect("fake release endpoint header write timed out");
            let _ = tokio::time::timeout(Duration::from_secs(5), stream.write_all(&body))
                .await
                .expect("fake release endpoint body write timed out");
            let _ = tokio::time::timeout(Duration::from_secs(5), stream.shutdown())
                .await
                .expect("fake release endpoint shutdown timed out");
        });
        FakeReleaseEndpoint {
            url: format!("http://{address}/release.json"),
            server: Some(server),
        }
    }

    fn test_http_source(metadata_url: String) -> HttpReleaseSource {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        HttpReleaseSource {
            client,
            metadata_url,
        }
    }

    async fn assert_endpoint_validation_error(
        body: Vec<u8>,
        key: &SigningKey,
        validation: ReleaseValidationContext,
        expected: ReleaseValidationError,
    ) {
        let temporary = tempfile::tempdir().unwrap();
        let endpoint = fake_release_endpoint(body, FakeResponseFraming::ContentLength).await;
        let coordinator = UpgradeCoordinator::new(
            temporary.path().join("endpoint-rejection"),
            Box::new(test_http_source(endpoint.url.clone())),
            key.verifying_key(),
            validation,
        );

        let result = tokio::time::timeout(Duration::from_secs(5), coordinator.check()).await;
        endpoint.finish().await;
        let result = result.expect("release check timed out");
        match result {
            Err(UpgradeError::Validation(actual)) => assert_eq!(actual, expected),
            other => panic!("expected {expected:?}, got {other:?}"),
        }
    }

    #[derive(Debug)]
    struct RecordingInstaller {
        healthy: bool,
        calls: Arc<Mutex<Vec<&'static str>>>,
    }

    #[async_trait]
    impl PlatformInstaller for RecordingInstaller {
        async fn apply(
            &self,
            _payload: &Path,
            release: &ReleaseMetadata,
        ) -> Result<HealthReport, UpgradeError> {
            self.calls.lock().unwrap().push("apply");
            Ok(HealthReport {
                application_version: release.target_version.clone(),
                daemon_version: release.target_version.clone(),
                protocol: if self.healthy {
                    HealthCheck::Passed
                } else {
                    HealthCheck::Failed
                },
                database: if self.healthy {
                    HealthCheck::Passed
                } else {
                    HealthCheck::Failed
                },
                executable_identity: if self.healthy {
                    HealthCheck::Passed
                } else {
                    HealthCheck::Failed
                },
                platform_signature: if self.healthy {
                    HealthCheck::Passed
                } else {
                    HealthCheck::Failed
                },
            })
        }

        async fn rollback(&self, _release: &ReleaseMetadata) -> Result<(), UpgradeError> {
            self.calls.lock().unwrap().push("rollback");
            Ok(())
        }
    }

    fn target() -> ReleaseTarget {
        ReleaseTarget {
            platform: ReleasePlatform::Linux,
            architecture: ReleaseArchitecture::X86_64,
            installer: InstallerKind::LinuxAppImage,
        }
    }

    fn release_fixture() -> (Vec<u8>, ReleaseMetadata, SigningKey) {
        let payload = b"verified application payload".to_vec();
        let release = ReleaseMetadata {
            schema_version: 1,
            minimum_source_version: "0.1.0".into(),
            target_version: "0.2.0".into(),
            target: target(),
            installer_url: "https://github.com/devemit/sylvops/releases/download/v0.2.0/sylvops-linux-x86_64.AppImage".into(),
            byte_length: payload.len() as u64,
            sha256: format!("{:x}", Sha256::digest(&payload)),
            release_notes_url: "https://github.com/devemit/sylvops/releases/tag/v0.2.0".into(),
            release_notes: "Upgrade test.".into(),
            published_at_unix_seconds: 1_800_000_000,
        };
        (payload, release, SigningKey::from_bytes(&[9; 32]))
    }

    fn signed_metadata(release: ReleaseMetadata, key: &SigningKey) -> Vec<u8> {
        let signature = key.sign(&release.signed_bytes().unwrap()).to_bytes();
        serde_json::to_vec(&SignedReleaseMetadata::new(release, signature)).unwrap()
    }

    fn validation_context(current_version: &str) -> ReleaseValidationContext {
        ReleaseValidationContext {
            current_version: current_version.into(),
            expected_target: target(),
            now_unix_seconds: 1_800_000_000,
            oldest_allowed_publication: 1_799_000_000,
            newest_seen_publication: None,
        }
    }

    fn fixture_for_current_version(
        healthy: bool,
        current_version: &str,
    ) -> (
        UpgradeCoordinator,
        tempfile::TempDir,
        Arc<Mutex<Vec<&'static str>>>,
    ) {
        let temporary = tempfile::tempdir().unwrap();
        let (payload, release, key) = release_fixture();
        let metadata = signed_metadata(release, &key);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let coordinator = UpgradeCoordinator::new_for_test(
            temporary.path().join("staging"),
            Box::new(FakeSource { metadata, payload }),
            Box::new(RecordingInstaller {
                healthy,
                calls: calls.clone(),
            }),
            key.verifying_key(),
            validation_context(current_version),
        );
        (coordinator, temporary, calls)
    }

    fn fixture(
        healthy: bool,
    ) -> (
        UpgradeCoordinator,
        tempfile::TempDir,
        Arc<Mutex<Vec<&'static str>>>,
    ) {
        fixture_for_current_version(healthy, "0.1.0")
    }

    #[tokio::test]
    async fn check_and_download_stage_verified_bytes_outside_the_installation() {
        let (coordinator, temporary, _) = fixture(true);

        let release = coordinator.check().await.unwrap().unwrap();
        let staged = coordinator.download().await.unwrap();

        assert_eq!(release.target_version, "0.2.0");
        assert!(staged.payload.starts_with(temporary.path()));
        assert_eq!(
            tokio::fs::read(&staged.payload).await.unwrap(),
            b"verified application payload"
        );
        assert!(!staged.payload.to_string_lossy().ends_with(".partial"));
    }

    #[tokio::test]
    async fn rejected_payload_is_cleaned_up_and_remains_available_for_retry() {
        let temporary = tempfile::tempdir().unwrap();
        let (mut payload, release, key) = release_fixture();
        payload[0] ^= 0xff;
        let coordinator = UpgradeCoordinator::new(
            temporary.path().join("rejected-payload"),
            Box::new(FakeSource {
                metadata: signed_metadata(release.clone(), &key),
                payload,
            }),
            key.verifying_key(),
            validation_context("0.1.0"),
        );

        coordinator.check().await.unwrap();
        assert!(matches!(
            coordinator.download().await,
            Err(UpgradeError::DigestMismatch)
        ));

        assert!(matches!(
            coordinator.status().await,
            UpgradeStatus::Available { release: available } if available == release
        ));
        assert!(
            !temporary
                .path()
                .join("rejected-payload/payload.partial")
                .exists()
        );
        assert!(
            !temporary
                .path()
                .join("rejected-payload/payload.staged")
                .exists()
        );
    }

    #[tokio::test]
    async fn full_staging_volume_cleans_partial_payload_and_allows_retry() {
        let temporary = tempfile::tempdir().unwrap();
        let staging = temporary.path().join("storage-full");
        let (_payload, release, key) = release_fixture();
        let coordinator = UpgradeCoordinator::new(
            staging.clone(),
            Box::new(StorageFailureSource {
                metadata: signed_metadata(release.clone(), &key),
            }),
            key.verifying_key(),
            validation_context("0.1.0"),
        );

        coordinator.check().await.unwrap();
        assert!(matches!(
            coordinator.download().await,
            Err(UpgradeError::Io(error)) if error.kind() == std::io::ErrorKind::StorageFull
        ));

        assert!(matches!(
            coordinator.status().await,
            UpgradeStatus::Available { release: available } if available == release
        ));
        assert!(!staging.join("payload.partial").exists());
        assert!(!staging.join("payload.staged").exists());
        assert!(!staging.join("release.partial").exists());
        assert!(!staging.join("release.json").exists());
    }

    #[tokio::test]
    async fn fake_endpoint_downloads_and_stages_verified_payload() {
        let temporary = tempfile::tempdir().unwrap();
        let staging = temporary.path().join("endpoint-success");
        let (payload, release, key) = release_fixture();
        let endpoint = fake_upgrade_endpoint(
            signed_metadata(release.clone(), &key),
            FakePayloadResponse::Immediate(payload.clone()),
        )
        .await;
        let coordinator = UpgradeCoordinator::new(
            staging.clone(),
            Box::new(endpoint_source(&endpoint)),
            key.verifying_key(),
            validation_context("0.1.0"),
        );

        coordinator.check().await.unwrap();
        let staged = tokio::time::timeout(Duration::from_secs(5), coordinator.download())
            .await
            .expect("upgrade download timed out")
            .unwrap();
        endpoint.finish().await;

        assert_eq!(tokio::fs::read(&staged.payload).await.unwrap(), payload);
        assert_eq!(staged.payload, staging.join("payload.staged"));
        assert!(staging.join("release.json").exists());
        assert!(!staging.join("payload.partial").exists());
        assert!(!staging.join("release.partial").exists());
        assert_eq!(
            coordinator.status().await,
            UpgradeStatus::Staged { release }
        );
    }

    #[tokio::test]
    async fn fake_endpoint_cancellation_removes_partial_payload_and_allows_retry() {
        let temporary = tempfile::tempdir().unwrap();
        let staging = temporary.path().join("endpoint-cancelled");
        let (payload, release, key) = release_fixture();
        let split = payload.len() / 2;
        let first_sent = Arc::new(tokio::sync::Notify::new());
        let resume = Arc::new(tokio::sync::Notify::new());
        let endpoint = fake_upgrade_endpoint(
            signed_metadata(release.clone(), &key),
            FakePayloadResponse::Paused {
                first: payload[..split].to_vec(),
                rest: payload[split..].to_vec(),
                first_sent: first_sent.clone(),
                resume: resume.clone(),
            },
        )
        .await;
        let coordinator = Arc::new(UpgradeCoordinator::new(
            staging.clone(),
            Box::new(endpoint_source(&endpoint)),
            key.verifying_key(),
            validation_context("0.1.0"),
        ));
        coordinator.check().await.unwrap();

        let downloading = {
            let coordinator = coordinator.clone();
            tokio::spawn(async move { coordinator.download().await })
        };
        tokio::time::timeout(Duration::from_secs(5), first_sent.notified())
            .await
            .expect("fake endpoint did not begin the payload response");
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if tokio::fs::metadata(staging.join("payload.partial"))
                    .await
                    .is_ok_and(|metadata| metadata.len() > 0)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("download did not persist the first payload chunk");
        coordinator.cancel_download();
        let result = tokio::time::timeout(Duration::from_secs(1), downloading)
            .await
            .expect("cancelled download did not stop promptly")
            .unwrap();
        resume.notify_one();
        endpoint.finish().await;

        assert!(matches!(result, Err(UpgradeError::Cancelled)));
        assert!(matches!(
            coordinator.status().await,
            UpgradeStatus::Available { release: available } if available == release
        ));
        for name in [
            "payload.partial",
            "payload.staged",
            "release.partial",
            "release.json",
        ] {
            assert!(!staging.join(name).exists(), "left staging artifact {name}");
        }
    }

    #[tokio::test]
    async fn fake_endpoint_truncated_payload_fails_closed_and_cleans_staging() {
        let temporary = tempfile::tempdir().unwrap();
        let staging = temporary.path().join("endpoint-truncated");
        let (payload, release, key) = release_fixture();
        let endpoint = fake_upgrade_endpoint(
            signed_metadata(release.clone(), &key),
            FakePayloadResponse::Immediate(payload[..payload.len() - 3].to_vec()),
        )
        .await;
        let coordinator = UpgradeCoordinator::new(
            staging.clone(),
            Box::new(endpoint_source(&endpoint)),
            key.verifying_key(),
            validation_context("0.1.0"),
        );

        coordinator.check().await.unwrap();
        let result = coordinator.download().await;
        endpoint.finish().await;

        assert!(matches!(result, Err(UpgradeError::LengthMismatch)));
        assert!(matches!(
            coordinator.status().await,
            UpgradeStatus::Available { .. }
        ));
        assert!(!staging.join("payload.partial").exists());
        assert!(!staging.join("payload.staged").exists());
    }

    #[tokio::test]
    async fn fake_endpoint_corrupt_payload_fails_closed_and_cleans_staging() {
        let temporary = tempfile::tempdir().unwrap();
        let staging = temporary.path().join("endpoint-corrupt");
        let (mut payload, release, key) = release_fixture();
        payload[0] ^= 0xff;
        let endpoint = fake_upgrade_endpoint(
            signed_metadata(release.clone(), &key),
            FakePayloadResponse::Immediate(payload),
        )
        .await;
        let coordinator = UpgradeCoordinator::new(
            staging.clone(),
            Box::new(endpoint_source(&endpoint)),
            key.verifying_key(),
            validation_context("0.1.0"),
        );

        coordinator.check().await.unwrap();
        let result = coordinator.download().await;
        endpoint.finish().await;

        assert!(matches!(result, Err(UpgradeError::DigestMismatch)));
        assert!(matches!(
            coordinator.status().await,
            UpgradeStatus::Available { .. }
        ));
        assert!(!staging.join("payload.partial").exists());
        assert!(!staging.join("payload.staged").exists());
    }

    #[tokio::test]
    async fn fake_endpoint_staged_payload_recovers_after_restart() {
        let temporary = tempfile::tempdir().unwrap();
        let staging = temporary.path().join("endpoint-restart");
        let (payload, release, key) = release_fixture();
        let metadata = signed_metadata(release.clone(), &key);
        let endpoint =
            fake_upgrade_endpoint(metadata.clone(), FakePayloadResponse::Immediate(payload)).await;
        let coordinator = UpgradeCoordinator::new(
            staging.clone(),
            Box::new(endpoint_source(&endpoint)),
            key.verifying_key(),
            validation_context("0.1.0"),
        );
        coordinator.check().await.unwrap();
        coordinator.download().await.unwrap();
        endpoint.finish().await;

        let restarted = UpgradeCoordinator::new(
            staging.clone(),
            Box::new(MetadataOnlySource { metadata }),
            key.verifying_key(),
            validation_context("0.1.0"),
        );

        assert!(restarted.recover_staged().await.unwrap());
        assert_eq!(restarted.status().await, UpgradeStatus::Staged { release });
        assert!(staging.join("payload.staged").exists());
        assert!(staging.join("release.json").exists());
    }

    #[tokio::test]
    async fn fake_endpoint_interrupted_download_is_cleaned_after_restart() {
        let temporary = tempfile::tempdir().unwrap();
        let staging = temporary.path().join("endpoint-interrupted");
        let (payload, release, key) = release_fixture();
        let metadata = signed_metadata(release, &key);
        let split = payload.len() / 2;
        let first_sent = Arc::new(tokio::sync::Notify::new());
        let resume = Arc::new(tokio::sync::Notify::new());
        let endpoint = fake_upgrade_endpoint(
            metadata.clone(),
            FakePayloadResponse::Paused {
                first: payload[..split].to_vec(),
                rest: payload[split..].to_vec(),
                first_sent: first_sent.clone(),
                resume: resume.clone(),
            },
        )
        .await;
        let coordinator = Arc::new(UpgradeCoordinator::new(
            staging.clone(),
            Box::new(endpoint_source(&endpoint)),
            key.verifying_key(),
            validation_context("0.1.0"),
        ));
        coordinator.check().await.unwrap();
        let downloading = {
            let coordinator = coordinator.clone();
            tokio::spawn(async move { coordinator.download().await })
        };
        tokio::time::timeout(Duration::from_secs(5), first_sent.notified())
            .await
            .expect("fake endpoint did not begin the payload response");
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if tokio::fs::metadata(staging.join("payload.partial"))
                    .await
                    .is_ok_and(|metadata| metadata.len() > 0)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("download did not persist the first payload chunk");
        downloading.abort();
        let _ = downloading.await;
        tokio::fs::write(staging.join("release.partial"), &metadata)
            .await
            .unwrap();
        resume.notify_one();
        endpoint.finish().await;
        drop(coordinator);

        let restarted = UpgradeCoordinator::new(
            staging.clone(),
            Box::new(MetadataOnlySource { metadata }),
            key.verifying_key(),
            validation_context("0.1.0"),
        );

        assert!(!restarted.recover_staged().await.unwrap());
        assert!(!staging.join("payload.partial").exists());
        assert!(!staging.join("release.partial").exists());
        assert!(!staging.join("payload.staged").exists());
        assert!(!staging.join("release.json").exists());
    }

    #[tokio::test]
    async fn check_discovers_signed_metadata_without_downloading_the_payload() {
        let temporary = tempfile::tempdir().unwrap();
        let (_payload, release, key) = release_fixture();
        let metadata = signed_metadata(release, &key);
        let coordinator = UpgradeCoordinator::new(
            temporary.path().join("discovery-only"),
            Box::new(MetadataOnlySource { metadata }),
            key.verifying_key(),
            validation_context("0.1.0"),
        );

        let release = coordinator.check().await.unwrap().unwrap();

        assert_eq!(release.target_version, "0.2.0");
        assert!(matches!(
            coordinator.status().await,
            UpgradeStatus::Available { .. }
        ));
        assert!(
            !temporary
                .path()
                .join("discovery-only/payload.partial")
                .exists()
        );
        assert!(
            !temporary
                .path()
                .join("discovery-only/payload.staged")
                .exists()
        );
    }

    #[tokio::test]
    async fn fake_release_endpoint_rejects_malformed_metadata() {
        let key = SigningKey::from_bytes(&[9; 32]);

        assert_endpoint_validation_error(
            b"not json".to_vec(),
            &key,
            validation_context("0.1.0"),
            ReleaseValidationError::Malformed,
        )
        .await;
    }

    #[tokio::test]
    async fn fake_release_endpoint_rejects_oversized_metadata() {
        let key = SigningKey::from_bytes(&[9; 32]);
        let temporary = tempfile::tempdir().unwrap();
        let endpoint = fake_release_endpoint(
            vec![b'x'; MAX_RELEASE_METADATA_BYTES + 1],
            FakeResponseFraming::ConnectionClose,
        )
        .await;
        let coordinator = UpgradeCoordinator::new(
            temporary.path().join("oversized-endpoint"),
            Box::new(test_http_source(endpoint.url.clone())),
            key.verifying_key(),
            validation_context("0.1.0"),
        );

        let result = tokio::time::timeout(Duration::from_secs(5), coordinator.check()).await;
        endpoint.finish().await;
        let result = result.expect("release check timed out");
        assert!(matches!(
            result,
            Err(UpgradeError::ReleaseService(message))
                if message == "release response exceeds its byte limit"
        ));
    }

    #[tokio::test]
    async fn fake_release_endpoint_rejects_stale_metadata() {
        let (_payload, release, key) = release_fixture();
        let body = signed_metadata(release, &key);
        let mut validation = validation_context("0.1.0");
        validation.oldest_allowed_publication = 1_800_000_001;

        assert_endpoint_validation_error(
            body,
            &key,
            validation,
            ReleaseValidationError::StalePublication,
        )
        .await;
    }

    #[tokio::test]
    async fn fake_release_endpoint_rejects_downgrade_metadata() {
        let (_payload, release, key) = release_fixture();

        assert_endpoint_validation_error(
            signed_metadata(release, &key),
            &key,
            validation_context("0.3.0"),
            ReleaseValidationError::Downgrade,
        )
        .await;
    }

    #[tokio::test]
    async fn fake_release_endpoint_rejects_wrong_target_metadata() {
        let (_payload, release, key) = release_fixture();
        let mut validation = validation_context("0.1.0");
        validation.expected_target.architecture = ReleaseArchitecture::Aarch64;

        assert_endpoint_validation_error(
            signed_metadata(release, &key),
            &key,
            validation,
            ReleaseValidationError::WrongTarget,
        )
        .await;
    }

    #[tokio::test]
    async fn fake_release_endpoint_rejects_invalid_signature_metadata() {
        let (_payload, release, key) = release_fixture();
        let untrusted_key = SigningKey::from_bytes(&[8; 32]);

        assert_endpoint_validation_error(
            signed_metadata(release, &untrusted_key),
            &key,
            validation_context("0.1.0"),
            ReleaseValidationError::InvalidSignature,
        )
        .await;
    }

    #[tokio::test]
    async fn check_reports_current_release_as_up_to_date() {
        let (coordinator, _temporary, _) = fixture_for_current_version(true, "0.2.0");

        assert_eq!(coordinator.check().await.unwrap(), None);
        assert_eq!(
            coordinator.status().await,
            UpgradeStatus::UpToDate {
                version: "0.2.0".into()
            }
        );
    }

    #[tokio::test]
    async fn restart_preserves_the_release_replay_watermark() {
        let (coordinator, temporary, _) = fixture(true);
        coordinator.check().await.unwrap();

        let payload = b"verified application payload".to_vec();
        let key = SigningKey::from_bytes(&[9; 32]);
        let release = ReleaseMetadata {
            schema_version: 1,
            minimum_source_version: "0.1.0".into(),
            target_version: "0.2.0".into(),
            target: target(),
            installer_url: "https://github.com/devemit/sylvops/releases/download/v0.2.0/sylvops-linux-x86_64.AppImage".into(),
            byte_length: payload.len() as u64,
            sha256: format!("{:x}", Sha256::digest(&payload)),
            release_notes_url: "https://github.com/devemit/sylvops/releases/tag/v0.2.0".into(),
            release_notes: "Replayed upgrade test.".into(),
            published_at_unix_seconds: 1_799_999_999,
        };
        let signature = key.sign(&release.signed_bytes().unwrap()).to_bytes();
        let metadata = serde_json::to_vec(&SignedReleaseMetadata::new(release, signature)).unwrap();
        let restarted = UpgradeCoordinator::new_for_test(
            temporary.path().join("staging"),
            Box::new(FakeSource { metadata, payload }),
            Box::new(RecordingInstaller {
                healthy: true,
                calls: Arc::new(Mutex::new(Vec::new())),
            }),
            key.verifying_key(),
            ReleaseValidationContext {
                current_version: "0.1.0".into(),
                expected_target: target(),
                now_unix_seconds: 1_800_000_000,
                oldest_allowed_publication: 1_799_000_000,
                newest_seen_publication: None,
            },
        );

        assert!(matches!(
            restarted.check().await,
            Err(UpgradeError::Validation(
                sylvops_core::upgrade::ReleaseValidationError::ReplayedPublication
            ))
        ));
    }

    #[tokio::test]
    async fn active_sessions_block_install_until_named_override_then_unhealthy_install_rolls_back_once()
     {
        let (coordinator, _temporary, calls) = fixture(false);
        coordinator.check().await.unwrap();
        coordinator.download().await.unwrap();
        let active = vec![ActiveUpgradeSession {
            id: sylvops_core::ids::SessionId::new(),
            name: "Release work".into(),
        }];

        let blocked = coordinator.install(&active, false).await.unwrap();
        assert_eq!(
            blocked,
            InstallDisposition::Blocked {
                active_sessions: active.clone()
            }
        );
        assert_eq!(*calls.lock().unwrap(), Vec::<&str>::new());

        let installed = coordinator.install(&active, true).await.unwrap();
        assert_eq!(installed, InstallDisposition::RolledBack);
        assert_eq!(*calls.lock().unwrap(), vec!["apply", "rollback"]);
    }

    #[tokio::test]
    async fn native_handoff_is_prepared_only_after_named_session_override() {
        let (coordinator, _temporary, _) = fixture(true);
        coordinator.check().await.unwrap();
        coordinator.download().await.unwrap();
        let active = vec![ActiveUpgradeSession {
            id: sylvops_core::ids::SessionId::new(),
            name: "Release work".into(),
        }];

        assert_eq!(
            coordinator.prepare_install(&active, false).await.unwrap(),
            InstallDisposition::Blocked {
                active_sessions: active.clone()
            }
        );
        assert_eq!(
            coordinator.prepare_install(&active, true).await.unwrap(),
            InstallDisposition::Prepared {
                version: "0.2.0".into()
            }
        );
    }

    #[tokio::test]
    async fn staged_upgrade_recovers_after_restart_and_corruption_is_cleaned_up() {
        let (coordinator, temporary, _) = fixture(true);
        coordinator.check().await.unwrap();
        let staged = coordinator.download().await.unwrap();

        assert!(coordinator.recover_staged().await.unwrap());

        tokio::fs::write(&staged.payload, b"corrupt").await.unwrap();
        assert!(matches!(
            coordinator.recover_staged().await,
            Err(UpgradeError::LengthMismatch | UpgradeError::DigestMismatch)
        ));
        assert!(!staged.payload.exists());
        assert!(!temporary.path().join("staging/release.json").exists());
    }

    #[test]
    fn installation_bound_coordinator_refuses_overlapping_staging_root() {
        let temporary = tempfile::tempdir().unwrap();
        let installation = temporary.path().join("installation");
        let user_data = temporary.path().join("user-data");
        std::fs::create_dir_all(&installation).unwrap();
        std::fs::create_dir_all(&user_data).unwrap();
        let executable = installation.join("sylvops.exe");
        std::fs::write(&executable, b"test executable").unwrap();
        let (payload, release, key) = release_fixture();
        let metadata = signed_metadata(release, &key);

        let refused = UpgradeCoordinator::new_for_installation(
            &installation.join("upgrades"),
            &executable,
            Box::new(FakeSource {
                metadata: metadata.clone(),
                payload: payload.clone(),
            }),
            key.verifying_key(),
            validation_context("0.1.0"),
        );
        assert!(matches!(refused, Err(UpgradeError::UnsafeStagingPath)));

        let accepted = UpgradeCoordinator::new_for_installation(
            &user_data.join("upgrades"),
            &executable,
            Box::new(FakeSource { metadata, payload }),
            key.verifying_key(),
            validation_context("0.1.0"),
        )
        .unwrap();
        assert_eq!(
            accepted.staging_root,
            std::fs::canonicalize(&user_data).unwrap().join("upgrades")
        );
    }
}
