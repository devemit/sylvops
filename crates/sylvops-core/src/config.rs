//! Versioned configuration values shared by daemon and clients.

use std::{collections::BTreeMap, path::PathBuf};

use serde::{Deserialize, Serialize};
use toml::Value;

use crate::domain::ProviderKind;

pub const CURRENT_CONFIG_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub version: u32,
    pub enabled_providers: Vec<ProviderKind>,
    pub default_provider: ProviderKind,
    pub default_model: Option<String>,
    pub default_effort: Option<String>,
    pub theme: String,
    pub key_bindings: BTreeMap<String, String>,
    pub scrollback_capacity_bytes: usize,
    pub managed_worktree_directory: Option<PathBuf>,
    pub notifications_enabled: bool,
    pub sound_enabled: bool,
    pub github_enabled: bool,
    pub prompt_retention_count: usize,
    pub hook_body_limit_bytes: usize,
    pub hook_requests_per_minute: u32,
    #[serde(flatten)]
    pub unknown: BTreeMap<String, Value>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            version: CURRENT_CONFIG_VERSION,
            enabled_providers: vec![ProviderKind::Shell, ProviderKind::Codex],
            default_provider: ProviderKind::Shell,
            default_model: None,
            default_effort: None,
            theme: "default".into(),
            key_bindings: BTreeMap::new(),
            scrollback_capacity_bytes: 4 * 1024 * 1024,
            managed_worktree_directory: None,
            notifications_enabled: true,
            sound_enabled: false,
            github_enabled: false,
            prompt_retention_count: 50,
            hook_body_limit_bytes: 64 * 1024,
            hook_requests_per_minute: 600,
            unknown: BTreeMap::new(),
        }
    }
}

impl AppConfig {
    /// Validates configuration fields consumed by the current binary.
    ///
    /// # Errors
    ///
    /// Returns a validation error for unsupported versions, empty provider sets, invalid
    /// defaults, or unsafe capacity and retention values.
    pub fn validate(&self) -> Result<(), String> {
        if self.version > CURRENT_CONFIG_VERSION {
            return Err(format!(
                "configuration version {} is newer than supported version {CURRENT_CONFIG_VERSION}",
                self.version
            ));
        }
        if self.enabled_providers.is_empty() {
            return Err("at least one provider must be enabled".into());
        }
        if self
            .enabled_providers
            .iter()
            .any(|provider| !matches!(provider, ProviderKind::Shell | ProviderKind::Codex))
        {
            return Err("only shell and codex providers are implemented in this release".into());
        }
        if !self.enabled_providers.contains(&self.default_provider) {
            return Err("default provider must be enabled".into());
        }
        if !(64 * 1024..=64 * 1024 * 1024).contains(&self.scrollback_capacity_bytes) {
            return Err("scrollback capacity must be between 64 KiB and 64 MiB".into());
        }
        if self
            .managed_worktree_directory
            .as_ref()
            .is_some_and(|path| !path.is_absolute() || path.parent().is_none())
        {
            return Err(
                "managed worktree directory must be absolute and cannot be a filesystem root"
                    .into(),
            );
        }
        if self.prompt_retention_count > 10_000 {
            return Err("prompt retention count must not exceed 10000".into());
        }
        if !(1024..=1024 * 1024).contains(&self.hook_body_limit_bytes) {
            return Err("hook body limit must be between 1 KiB and 1 MiB".into());
        }
        if !(1..=10_000).contains(&self.hook_requests_per_minute) {
            return Err("hook request limit must be between 1 and 10000 per minute".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_top_level_fields_survive_round_trip() {
        let source = r#"
version = 1
future_setting = "preserved"
"#;
        let config: AppConfig = toml::from_str(source).unwrap();
        let encoded = toml::to_string(&config).unwrap();
        assert!(encoded.contains("future_setting = \"preserved\""));
    }

    #[test]
    fn default_configuration_is_valid() {
        AppConfig::default().validate().unwrap();
    }

    #[test]
    fn managed_worktree_directory_must_be_an_absolute_non_root_path() {
        let mut config = AppConfig {
            managed_worktree_directory: Some(PathBuf::from("relative/worktrees")),
            ..AppConfig::default()
        };
        assert!(config.validate().is_err());

        let root = if cfg!(windows) {
            PathBuf::from(r"C:\")
        } else {
            PathBuf::from("/")
        };
        config.managed_worktree_directory = Some(root);
        assert!(config.validate().is_err());
    }
}
