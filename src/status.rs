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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files_transferred: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_transferred: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<ConflictEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConflictEntry {
    pub file: String,
    pub local_path: String,
    pub original_local_path: String,
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
        let tmp = path.with_extension("toml.tmp");
        let text = toml::to_string_pretty(self)?;
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}
