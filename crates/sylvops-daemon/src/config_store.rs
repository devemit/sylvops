//! Versioned TOML configuration loading with machine-local overrides.

use std::{fs, path::Path};

use sylvops_core::config::AppConfig;
use toml::Value;

use crate::{DaemonError, Result, atomic_file};

const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

/// Loads the primary configuration and overlays the optional machine-local file.
///
/// A missing primary file is initialized atomically with defaults. Existing files are never
/// rewritten merely by loading them, preserving unknown fields for newer versions.
///
/// # Errors
///
/// Returns an error for oversized, unreadable, invalid, or unsupported configuration.
pub fn load(primary: &Path, machine_local: &Path) -> Result<AppConfig> {
    if !primary.exists() {
        let encoded = toml::to_string_pretty(&AppConfig::default())
            .map_err(|error| DaemonError::Configuration(error.to_string()))?;
        atomic_file::write(primary, encoded.as_bytes())?;
    }

    let mut value = read_toml(primary)?;
    if machine_local.exists() {
        merge(&mut value, read_toml(machine_local)?);
    }
    let config: AppConfig = value
        .try_into()
        .map_err(|error| DaemonError::Configuration(format!("invalid configuration: {error}")))?;
    config.validate().map_err(DaemonError::Configuration)?;
    Ok(config)
}

fn read_toml(path: &Path) -> Result<Value> {
    let metadata = fs::metadata(path)
        .map_err(|error| DaemonError::Configuration(format!("{}: {error}", path.display())))?;
    if metadata.len() > MAX_CONFIG_BYTES {
        return Err(DaemonError::Configuration(format!(
            "{} exceeds the 1 MiB configuration limit",
            path.display()
        )));
    }
    let source = fs::read_to_string(path)
        .map_err(|error| DaemonError::Configuration(format!("{}: {error}", path.display())))?;
    toml::from_str::<Value>(&source)
        .map_err(|error| DaemonError::Configuration(format!("{}: {error}", path.display())))
}

fn merge(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Table(base), Value::Table(overlay)) => {
            for (key, value) in overlay {
                if let Some(existing) = base.get_mut(&key) {
                    merge(existing, value);
                } else {
                    base.insert(key, value);
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_defaults_and_applies_local_override() {
        let directory = tempfile::tempdir().unwrap();
        let primary = directory.path().join("config.toml");
        let local = directory.path().join("config.local.toml");
        fs::write(&local, "theme = 'local'\n").unwrap();

        let config = load(&primary, &local).unwrap();
        assert_eq!(config.theme, "local");
        assert!(primary.exists());
    }
}
