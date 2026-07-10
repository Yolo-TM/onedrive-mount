// Runtime state written by the daemon and read by the GUI — mirrors the config hierarchy

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DaemonStatus {
    pub pid: u32,
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub remotes: Vec<RemoteStatus>,
    /// Set when the config file was changed but failed to parse.
    /// Cleared when a subsequent valid config is loaded successfully.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteStatus {
    pub name: String,
    pub mount: MountState,
    #[serde(default)]
    pub sync_rules: Vec<SyncRuleStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum MountState {
    #[default]
    Unmounted,
    Mounting,
    Mounted {
        since: DateTime<Utc>,
    },
    Failed {
        error: String,
        at: DateTime<Utc>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncRuleStatus {
    pub name: String,
    pub last_sync: Option<DateTime<Utc>>,
    pub next_sync: Option<DateTime<Utc>>,
    pub state: SyncState,
    /// Files transferred in the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files_transferred: Option<u32>,
    /// Bytes transferred in the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_transferred: Option<u64>,
    /// Unresolved conflicts blocking this rule.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<ConflictEntry>,
}

/// A single file conflict detected during bidirectional sync.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConflictEntry {
    pub file: String,
    pub local_path: String,
    pub remote_path: String,
    pub local_size: u64,
    pub local_mtime: DateTime<Utc>,
    pub remote_size: u64,
    pub remote_mtime: DateTime<Utc>,
    pub detected_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SyncState {
    #[default]
    Idle,
    Running,
    Succeeded,
    Failed {
        error: String,
        at: DateTime<Utc>,
    },
    /// Rule is blocked because unresolved conflicts exist.
    /// No further sync attempts until all conflicts are resolved.
    BlockedOnConflicts {
        since: DateTime<Utc>,
    },
}

impl SyncState {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Idle => "Idle",
            Self::Running => "Running",
            Self::Succeeded => "OK",
            Self::Failed { .. } => "Failed",
            Self::BlockedOnConflicts { .. } => "Blocked (conflicts)",
        }
    }

    pub fn is_blocked(&self) -> bool {
        matches!(self, Self::BlockedOnConflicts { .. })
    }
}

impl DaemonStatus {
    pub fn load(path: &std::path::Path) -> Option<Self> {
        let text = std::fs::read_to_string(path).ok()?;
        toml::from_str(&text).ok()
    }

    pub fn save(&self, path: &std::path::Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Write to a temp file first, then rename for atomicity
        let tmp = path.with_extension("toml.tmp");
        let text = toml::to_string_pretty(self)?;
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}
