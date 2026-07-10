// Sync strategies: how files move between local and remote, and what happens on conflict

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SyncStrategy {
    /// Bidirectional sync. Files are pushed and pulled both ways.
    /// On conflict (same file changed on both sides): the local copy is renamed
    /// with a `.conflict-<timestamp>` suffix and kept locally only — the remote
    /// version overwrites the original local path. No data is lost.
    #[default]
    Bidirectional,
    /// Bidirectional sync. On conflict the file with the newer mtime wins;
    /// the older version is overwritten. Risk of data loss if clocks are skewed.
    NewestWins,
    /// One-way remote → local. Local is a read-only replica of remote.
    /// Local-only files are deleted, local changes are discarded on every sync.
    MirrorDown,
    /// One-way local → remote. Remote is always overwritten with local.
    /// Remote-only files are deleted. Pure backup-to-cloud use case.
    MirrorUp,
}

impl std::fmt::Display for SyncStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bidirectional => write!(f, "bidirectional"),
            Self::NewestWins => write!(f, "newest_wins"),
            Self::MirrorDown => write!(f, "mirror_down"),
            Self::MirrorUp => write!(f, "mirror_up"),
        }
    }
}

impl SyncStrategy {
    pub fn all() -> &'static [SyncStrategy] {
        &[
            Self::Bidirectional,
            Self::NewestWins,
            Self::MirrorDown,
            Self::MirrorUp,
        ]
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Bidirectional => "Bidirectional",
            Self::NewestWins => "Newest wins",
            Self::MirrorDown => "Mirror down (remote → local)",
            Self::MirrorUp => "Mirror up (local → remote)",
        }
    }

    /// Whether this strategy is destructive to one side and should show a warning.
    pub fn is_destructive(&self) -> bool {
        matches!(self, Self::MirrorDown | Self::MirrorUp)
    }
}
