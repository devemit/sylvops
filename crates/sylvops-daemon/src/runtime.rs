//! Per-user runtime paths and ephemeral local IPC authentication.

use std::{
    env, fmt, fs,
    path::{Path, PathBuf},
};

#[cfg(windows)]
use sha2::{Digest, Sha256};
#[cfg(windows)]
use std::fmt::Write as _;
use uuid::Uuid;

use crate::{DaemonError, Result, atomic_file, ipc::LocalEndpoint};

const MAX_TOKEN_BYTES: u64 = 256;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimePaths {
    pub data_directory: PathBuf,
    pub config_directory: PathBuf,
    pub runtime_directory: PathBuf,
    pub database: PathBuf,
    pub config: PathBuf,
    pub machine_config: PathBuf,
    pub authentication_token: PathBuf,
    pub daemon_log: PathBuf,
    pub endpoint: LocalEndpoint,
}

impl RuntimePaths {
    /// Resolves per-user paths, or places every artifact below an explicit test/development root.
    ///
    /// # Errors
    ///
    /// Returns an error when required platform environment paths are unavailable or relative.
    pub fn discover(override_root: Option<&Path>) -> Result<Self> {
        if let Some(root) = override_root {
            if !root.is_absolute() {
                return Err(DaemonError::Lifecycle(
                    "runtime override must be an absolute path".into(),
                ));
            }
            return Ok(Self::from_directories(
                root.join("data"),
                root.join("config"),
                root.join("run"),
            ));
        }
        platform_paths()
    }

    fn from_directories(data: PathBuf, config: PathBuf, runtime: PathBuf) -> Self {
        let endpoint = endpoint_for(&runtime);
        Self {
            database: data.join("sylvops.db"),
            config: config.join("config.toml"),
            machine_config: config.join("config.local.toml"),
            authentication_token: runtime.join("daemon.token"),
            daemon_log: data.join("daemon.log"),
            data_directory: data,
            config_directory: config,
            runtime_directory: runtime,
            endpoint,
        }
    }

    /// Creates and validates the private directories needed before daemon startup.
    ///
    /// # Errors
    ///
    /// Returns an error for filesystem failures or a symlink/reparse-point directory.
    pub fn prepare(&self) -> Result<()> {
        for directory in [
            &self.data_directory,
            &self.config_directory,
            &self.runtime_directory,
        ] {
            fs::create_dir_all(directory)
                .map_err(|error| DaemonError::Lifecycle(error.to_string()))?;
            reject_link(directory)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
                    .map_err(|error| DaemonError::Lifecycle(error.to_string()))?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct AuthenticationToken(String);

impl fmt::Debug for AuthenticationToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AuthenticationToken([REDACTED])")
    }
}

impl AuthenticationToken {
    #[must_use]
    pub fn generate() -> Self {
        Self(format!(
            "{}{}",
            Uuid::new_v4().simple(),
            Uuid::new_v4().simple()
        ))
    }

    /// Reads a bounded token file.
    ///
    /// # Errors
    ///
    /// Returns an error when the file is absent, oversized, unreadable, or malformed.
    pub fn read(path: &Path) -> Result<Self> {
        let metadata = fs::metadata(path).map_err(|error| {
            DaemonError::Lifecycle(format!("cannot read daemon authentication token: {error}"))
        })?;
        if metadata.len() > MAX_TOKEN_BYTES {
            return Err(DaemonError::Lifecycle(
                "daemon authentication token is oversized".into(),
            ));
        }
        let value =
            fs::read_to_string(path).map_err(|error| DaemonError::Lifecycle(error.to_string()))?;
        let value = value.trim();
        if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(DaemonError::Lifecycle(
                "daemon authentication token is malformed".into(),
            ));
        }
        Ok(Self(value.to_owned()))
    }

    /// Writes this token beside the daemon endpoint using atomic replacement.
    ///
    /// # Errors
    ///
    /// Returns an error for filesystem or atomic replacement failures.
    pub fn write(&self, path: &Path) -> Result<()> {
        atomic_file::write(path, self.0.as_bytes())
    }

    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

#[cfg(windows)]
fn platform_paths() -> Result<RuntimePaths> {
    let local = absolute_environment_path("LOCALAPPDATA")?;
    let roaming = absolute_environment_path("APPDATA")?;
    Ok(RuntimePaths::from_directories(
        local.join("SylvOps"),
        roaming.join("SylvOps"),
        local.join("SylvOps").join("run"),
    ))
}

#[cfg(unix)]
fn platform_paths() -> Result<RuntimePaths> {
    let home = absolute_environment_path("HOME")?;
    let data = optional_absolute("XDG_DATA_HOME")
        .unwrap_or_else(|| home.join(".local/share"))
        .join("sylvops");
    let config = optional_absolute("XDG_CONFIG_HOME")
        .unwrap_or_else(|| home.join(".config"))
        .join("sylvops");
    let runtime = optional_absolute("XDG_RUNTIME_DIR")
        .map_or_else(|| data.join("run"), |path| path.join("sylvops"));
    Ok(RuntimePaths::from_directories(data, config, runtime))
}

fn absolute_environment_path(name: &str) -> Result<PathBuf> {
    let path = env::var_os(name)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| {
            DaemonError::Lifecycle(format!("{name} is unavailable or is not an absolute path"))
        })?;
    Ok(path)
}

#[cfg(unix)]
fn optional_absolute(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

#[cfg(windows)]
fn endpoint_for(runtime: &Path) -> LocalEndpoint {
    let digest = Sha256::digest(runtime.as_os_str().to_string_lossy().as_bytes());
    let mut suffix = String::with_capacity(24);
    for byte in &digest[..12] {
        write!(&mut suffix, "{byte:02x}").expect("writing to a string cannot fail");
    }
    LocalEndpoint::windows_pipe(format!(r"\\.\pipe\sylvops-{suffix}"))
}

#[cfg(unix)]
fn endpoint_for(runtime: &Path) -> LocalEndpoint {
    LocalEndpoint::unix(runtime.join("daemon.sock"))
}

#[cfg(unix)]
fn reject_link(path: &Path) -> Result<()> {
    if fs::symlink_metadata(path)
        .map_err(|error| DaemonError::Lifecycle(error.to_string()))?
        .file_type()
        .is_symlink()
    {
        return Err(DaemonError::Lifecycle(format!(
            "runtime directory is a symbolic link: {}",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(windows)]
fn reject_link(path: &Path) -> Result<()> {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    let attributes = fs::metadata(path)
        .map_err(|error| DaemonError::Lifecycle(error.to_string()))?
        .file_attributes();
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(DaemonError::Lifecycle(format!(
            "runtime directory is a reparse point: {}",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authentication_token_debug_output_is_redacted() {
        let token = AuthenticationToken::generate();
        let rendered = format!("{token:?}");

        assert_eq!(token.expose().len(), 64);
        assert!(!rendered.contains(token.expose()));
        assert!(rendered.contains("REDACTED"));
    }
}
