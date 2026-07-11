use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SyncBaseline {
    pub files: HashMap<String, DateTime<Utc>>,
}

impl SyncBaseline {
    pub fn load(path: &Path) -> Self {
        let Ok(content) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        toml::from_str(&content).unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = toml::to_string(self)?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, content)?;
        std::fs::rename(tmp, path)?;
        Ok(())
    }

    pub fn is_unchanged(&self, file: &str, mtime: DateTime<Utc>) -> bool {
        match self.files.get(file) {
            Some(&baseline_mtime) => (mtime - baseline_mtime).num_seconds().abs() <= 2,
            None => false,
        }
    }

    pub fn set(&mut self, file: &str, mtime: DateTime<Utc>) {
        self.files.insert(file.to_string(), mtime);
    }

    pub fn remove(&mut self, file: &str) {
        self.files.remove(file);
    }
}
