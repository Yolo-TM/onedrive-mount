// The single source of truth for all user-facing settings shared between GUI and daemon

use crate::{conflict::ConflictStrategy, defaults};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub remotes: Vec<RemoteConfig>,
    #[serde(default)]
    pub log: LogConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteConfig {
    pub name: String,
    #[serde(default = "defaults::remote_type")]
    pub r#type: String,
    pub mount_point: String,
    #[serde(default = "defaults::poll_interval")]
    pub poll_interval: String,
    #[serde(default = "defaults::enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub mount: MountConfig,
    #[serde(default)]
    pub sync_rules: Vec<SyncRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountConfig {
    #[serde(default = "defaults::vfs_cache_mode")]
    pub vfs_cache_mode: String,
    #[serde(default = "defaults::vfs_cache_max_age")]
    pub vfs_cache_max_age: String,
    #[serde(default = "defaults::vfs_cache_max_size")]
    pub vfs_cache_max_size: String,
    #[serde(default = "defaults::vfs_write_back")]
    pub vfs_write_back: String,
    #[serde(default = "defaults::transfers")]
    pub transfers: u32,
    #[serde(default = "defaults::dir_cache_time")]
    pub dir_cache_time: String,
    #[serde(default = "defaults::extra_flags")]
    pub extra_flags: Vec<String>,
}

impl Default for MountConfig {
    fn default() -> Self {
        Self {
            vfs_cache_mode: defaults::vfs_cache_mode(),
            vfs_cache_max_age: defaults::vfs_cache_max_age(),
            vfs_cache_max_size: defaults::vfs_cache_max_size(),
            vfs_write_back: defaults::vfs_write_back(),
            transfers: defaults::transfers(),
            dir_cache_time: defaults::dir_cache_time(),
            extra_flags: defaults::extra_flags(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogConfig {
    #[serde(default = "defaults::log_file")]
    pub file: String,
    #[serde(default = "defaults::log_level")]
    pub level: String,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            file: defaults::log_file(),
            level: defaults::log_level(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncRule {
    pub name: String,
    pub remote_path: String,
    pub local_path: String,
    #[serde(default = "defaults::sync_patterns")]
    pub patterns: Vec<String>,
    #[serde(default = "defaults::sync_interval")]
    pub interval: String,
    #[serde(default)]
    pub conflict_strategy: ConflictStrategy,
    #[serde(default = "defaults::rule_enabled")]
    pub enabled: bool,
}

impl Config {
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&text)?)
    }

    pub fn save(&self, path: &std::path::Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Returns a list of human-readable validation errors.
    /// An empty vec means the config is valid to save.
    pub fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();

        for remote in &self.remotes {
            if remote.name.is_empty() {
                errors.push("A remote has an empty name.".into());
            }
            if remote.mount_point.is_empty() {
                errors.push(format!("Remote '{}': mount point is empty.", remote.name));
            }
            if crate::defaults::parse_interval_secs(&remote.poll_interval).is_none() {
                errors.push(format!(
                    "Remote '{}': invalid poll interval '{}' — use e.g. '30s', '5m', '1h'.",
                    remote.name, remote.poll_interval,
                ));
            }

            for rule in &remote.sync_rules {
                if rule.name.is_empty() {
                    errors.push(format!("Remote '{}': a sync rule has an empty name.", remote.name));
                }
                if rule.local_path.is_empty() {
                    errors.push(format!("Remote '{}', rule '{}': local path is empty.", remote.name, rule.name));
                }
                if rule.remote_path.is_empty() {
                    errors.push(format!("Remote '{}', rule '{}': remote path is empty.", remote.name, rule.name));
                }
                if crate::defaults::parse_interval_secs(&rule.interval).is_none() {
                    errors.push(format!(
                        "Remote '{}', rule '{}': invalid interval '{}' — use e.g. '5m', '1h'.",
                        remote.name, rule.name, rule.interval,
                    ));
                }
            }
        }

        errors
    }
}
