use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SyncStrategy {
    #[default]
    Bidirectional,
    NewestWins,
    MirrorDown,
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

    pub fn is_destructive(&self) -> bool {
        matches!(self, Self::MirrorDown | Self::MirrorUp)
    }
}
