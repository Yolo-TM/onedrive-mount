// Conflict resolution decisions written by the GUI and consumed by the daemon.
// The GUI writes to conflict-resolutions.toml; the daemon watches it via inotify.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResolutionAction {
    KeepLocal,
    KeepRemote,
    KeepBoth,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resolution {
    pub remote: String,
    pub rule: String,
    pub file: String,
    pub action: ResolutionAction,
    pub resolved_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ResolutionFile {
    #[serde(default)]
    pub resolutions: Vec<Resolution>,
}

impl ResolutionFile {
    pub fn load(path: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(path).ok()?;
        toml::from_str(&text).ok()
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
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
