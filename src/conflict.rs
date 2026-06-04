// The three strategies differ in which side wins when both have changed

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConflictStrategy {
    /// Remote is the source of truth; local edits are overwritten on conflict.
    #[default]
    RemoteWins,
    /// Whichever copy has the newer mtime survives.
    NewestWins,
    /// Both copies are kept; the local one is renamed with a `.conflict-{timestamp}` suffix.
    KeepBoth,
}

impl std::fmt::Display for ConflictStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RemoteWins => write!(f, "remote_wins"),
            Self::NewestWins => write!(f, "newest_wins"),
            Self::KeepBoth => write!(f, "keep_both"),
        }
    }
}

impl ConflictStrategy {
    pub fn all() -> &'static [ConflictStrategy] {
        &[Self::RemoteWins, Self::NewestWins, Self::KeepBoth]
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::RemoteWins => "Remote wins",
            Self::NewestWins => "Newest wins",
            Self::KeepBoth => "Keep both",
        }
    }
}
