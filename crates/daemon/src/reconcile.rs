//! The reconciliation pass: the invariants we enforce ourselves.
//!
//! bisync does the transport. It does not promise the two properties that
//! actually matter to someone whose password database lives in a pinned folder,
//! so this runs after every sync and establishes them.
//!
//! **Invariant 1 — the canonical path always exists.** Verified against rclone
//! 1.75.1: when both sides' mtimes are *exactly equal* and the content differs,
//! `--conflict-resolve newer` cannot pick a winner, so it suffixes **both**
//! files and leaves the canonical path absent:
//!
//! ```text
//! Timm.kdbx.conflict-local   = LOCAL-ENTRIES
//! Timm.kdbx.conflict-remote  = REMOTE-ENTRIES
//! Timm.kdbx                  -- gone
//! ```
//!
//! No bytes are lost, but KeePassXC reports its database missing — which is the
//! symptom that started this rewrite. With OneDrive's 1-second mtime precision
//! and a 1s modify window, two edits inside the same second are enough.
//!
//! **Invariant 2 — conflict copies are timestamped and keep their extension.**
//! rclone appends its suffix after the whole filename, giving
//! `Timm.kdbx.conflict-local`: the extension is gone, so the file no longer
//! opens by double-click, and the name is fixed, so the *next* conflict would
//! overwrite it.
//!
//! What this pass refuses to do: it never deletes, never overwrites an existing
//! canonical file, and never merges. A pass whose failure mode is "quietly do
//! something clever" is not worth having.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use tracing::{info, warn};

use crate::bisync::{SUFFIX_LOCAL, SUFFIX_REMOTE, VERSIONS_DIR};

/// One conflict the pass found and normalised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    /// Canonical relative path, e.g. `Timm.kdbx`.
    pub path: String,
    /// The timestamped copy we renamed the loser to, relative to the pin root.
    pub copy: String,
    /// Which side lost.
    pub side: Side,
    /// True when the canonical name was absent and we restored it — the mtime
    /// tie case. Worth surfacing, because it means the winner was our choice
    /// rather than rclone's.
    pub restored_canonical: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Local,
    Remote,
}

impl Side {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Remote => "remote",
        }
    }
}

/// Split `Timm.kdbx.conflict-local` into (`Timm.kdbx`, [`Side::Local`]).
fn parse_conflict_name(name: &str) -> Option<(&str, Side)> {
    for (suffix, side) in [(SUFFIX_LOCAL, Side::Local), (SUFFIX_REMOTE, Side::Remote)] {
        if let Some(stem) = name.strip_suffix(&format!(".{suffix}")) {
            return Some((stem, side));
        }
    }
    None
}

/// Build the name a conflict copy should have.
///
/// `Timm.kdbx` + local + ts → `Timm.conflict-local-2026-10-08T124530Z.kdbx`.
/// The extension is restored so the file still opens, and the timestamp means
/// copies accumulate rather than overwriting each other.
pub fn conflict_copy_name(canonical: &str, side: Side, at: DateTime<Utc>) -> String {
    let stamp = at.format("%Y-%m-%dT%H%M%SZ");
    let marker = format!("conflict-{}-{stamp}", side.as_str());

    match canonical.rsplit_once('.') {
        // A leading dot is part of the name, not an extension (`.bashrc`).
        Some((stem, ext)) if !stem.is_empty() => format!("{stem}.{marker}.{ext}"),
        _ => format!("{canonical}.{marker}"),
    }
}

/// The inverse of [`conflict_copy_name`].
///
/// `Timm.conflict-local-2026-10-08T124530Z.kdbx` →
/// (`Timm.kdbx`, [`Side::Local`], `2026-10-08T124530Z`).
///
/// Needed because resolving a conflict has to find *every* copy of a file, and
/// copies accumulate: the database row records only the first one. The filename
/// is the authoritative record of which side a version came from.
pub fn parse_copy_name(name: &str) -> Option<(String, Side, String)> {
    for (marker, side) in [
        (".conflict-local-", Side::Local),
        (".conflict-remote-", Side::Remote),
    ] {
        let Some(at) = name.find(marker) else {
            continue;
        };
        let stem = &name[..at];
        let rest = &name[at + marker.len()..];
        if stem.is_empty() || rest.is_empty() {
            continue;
        }

        // The stamp never contains a dot, so the first one starts the extension
        // that `conflict_copy_name` moved to the end.
        let (stamp, canonical) = match rest.split_once('.') {
            Some((stamp, ext)) => (stamp, format!("{stem}.{ext}")),
            None => (rest, stem.to_owned()),
        };
        if stamp.is_empty() {
            continue;
        }
        return Some((canonical, side, stamp.to_owned()));
    }
    None
}

/// Which of two candidates should hold the canonical name.
///
/// Larger first, then newer, because a `.kdbx` that has grown has strictly more
/// entries in it — size is a better signal than time for the thing this is
/// most likely to be protecting. The choice is deliberately low-stakes: both
/// copies are kept either way, so getting it "wrong" costs nothing but a rename.
fn better(a: &Candidate, b: &Candidate) -> std::cmp::Ordering {
    a.size
        .cmp(&b.size)
        .then(a.mtime.cmp(&b.mtime))
        // Local last so it wins ties: it is the copy the user was working on.
        .then_with(|| match (a.side, b.side) {
            (Side::Local, Side::Remote) => std::cmp::Ordering::Greater,
            (Side::Remote, Side::Local) => std::cmp::Ordering::Less,
            _ => std::cmp::Ordering::Equal,
        })
}

#[derive(Debug, Clone)]
struct Candidate {
    path: PathBuf,
    side: Side,
    size: u64,
    mtime: DateTime<Utc>,
}

/// Run the pass over one pin's local directory.
///
/// Returns the conflicts it normalised. Operates on the local side only; the
/// renames propagate to the remote on the next bisync run, so there is never a
/// window in which two things are modifying both sides at once.
pub async fn run(local: &Path, now: DateTime<Utc>) -> Result<Vec<Conflict>> {
    let mut found = Vec::new();

    // Group rclone's fixed-name conflict files by the canonical path they
    // belong to, so the tie case — where both sides are present — is handled as
    // one decision rather than two.
    let mut groups: std::collections::HashMap<String, Vec<Candidate>> =
        std::collections::HashMap::new();

    let mut entries = match tokio::fs::read_dir(local).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(found),
        Err(e) => return Err(e).with_context(|| format!("reading {}", local.display())),
    };

    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == VERSIONS_DIR {
            continue;
        }
        let Some((canonical, side)) = parse_conflict_name(&name) else {
            continue;
        };
        let meta = match entry.metadata().await {
            Ok(meta) if meta.is_file() => meta,
            _ => continue,
        };

        groups
            .entry(canonical.to_owned())
            .or_default()
            .push(Candidate {
                path: entry.path(),
                side,
                size: meta.len(),
                mtime: meta
                    .modified()
                    .ok()
                    .and_then(|t| DateTime::<Utc>::from(t).into())
                    .unwrap_or(now),
            });
    }

    for (canonical, mut candidates) in groups {
        let canonical_path = local.join(&canonical);

        // Invariant 1: if rclone could not pick a winner, pick one — by copying,
        // so the suffixed version stays exactly where it is and nothing depends
        // on this choice having been right.
        let restored = if canonical_path.exists() {
            false
        } else {
            candidates.sort_by(better);
            let best = candidates.last().expect("non-empty group");
            tokio::fs::copy(&best.path, &canonical_path)
                .await
                .with_context(|| {
                    format!(
                        "restoring {} from {}",
                        canonical_path.display(),
                        best.path.display()
                    )
                })?;
            warn!(
                path = %canonical,
                restored_from = %best.path.file_name().unwrap_or_default().to_string_lossy(),
                code = onedrive_core::Code::Conflict.as_str(),
                "both sides changed with identical timestamps, so rclone left no winner; \
                 restored the canonical name from the larger copy"
            );
            true
        };

        // Invariant 2: give each copy a timestamped name that keeps the
        // extension, so copies accumulate and still open.
        for candidate in &candidates {
            let target_name = conflict_copy_name(&canonical, candidate.side, now);
            let target = local.join(&target_name);

            if target.exists() {
                // Same second, same side, already normalised. Leave it be rather
                // than overwrite a copy we cannot re-create.
                warn!(path = %target_name, "a conflict copy with this name already exists; leaving it");
                continue;
            }

            tokio::fs::rename(&candidate.path, &target)
                .await
                .with_context(|| {
                    format!(
                        "renaming {} to {}",
                        candidate.path.display(),
                        target.display()
                    )
                })?;

            info!(
                path = %canonical,
                copy = %target_name,
                side = candidate.side.as_str(),
                code = onedrive_core::Code::Conflict.as_str(),
                "kept both versions of a conflicting file"
            );

            found.push(Conflict {
                path: canonical.clone(),
                copy: target_name,
                side: candidate.side,
                restored_canonical: restored,
            });
        }
    }

    found.sort_by(|a, b| a.copy.cmp(&b.copy));
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 8, 12, 45, 30).unwrap()
    }

    #[test]
    fn parses_rclones_conflict_names() {
        assert_eq!(
            parse_conflict_name("Timm.kdbx.conflict-local"),
            Some(("Timm.kdbx", Side::Local))
        );
        assert_eq!(
            parse_conflict_name("Timm.kdbx.conflict-remote"),
            Some(("Timm.kdbx", Side::Remote))
        );
        assert_eq!(parse_conflict_name("Timm.kdbx"), None);
        assert_eq!(parse_conflict_name("notes.txt"), None);
        // Not one of ours.
        assert_eq!(parse_conflict_name("x.conflict"), None);
    }

    /// The point of the rename: `Timm.kdbx.conflict-local` does not open in
    /// KeePassXC, and the fixed name means a second conflict overwrites the
    /// first.
    #[test]
    fn conflict_copies_keep_their_extension_and_carry_a_timestamp() {
        assert_eq!(
            conflict_copy_name("Timm.kdbx", Side::Local, now()),
            "Timm.conflict-local-2026-10-08T124530Z.kdbx"
        );
        assert_eq!(
            conflict_copy_name("Timm.kdbx", Side::Remote, now()),
            "Timm.conflict-remote-2026-10-08T124530Z.kdbx"
        );
        // Still works for a file with no extension.
        assert_eq!(
            conflict_copy_name("README", Side::Local, now()),
            "README.conflict-local-2026-10-08T124530Z"
        );
        // A leading dot is a name, not an extension.
        assert_eq!(
            conflict_copy_name(".bashrc", Side::Local, now()),
            ".bashrc.conflict-local-2026-10-08T124530Z"
        );
        // Multiple dots: only the last is the extension.
        assert_eq!(
            conflict_copy_name("a.tar.gz", Side::Local, now()),
            "a.tar.conflict-local-2026-10-08T124530Z.gz"
        );
    }

    /// Resolution reads the side back off the filename, so the parse has to be
    /// the exact inverse of the generator for every name shape above.
    #[test]
    fn copy_names_round_trip() {
        for canonical in ["Timm.kdbx", "README", ".bashrc", "a.tar.gz", "notes.txt"] {
            for side in [Side::Local, Side::Remote] {
                let name = conflict_copy_name(canonical, side, now());
                let (parsed, parsed_side, stamp) =
                    parse_copy_name(&name).unwrap_or_else(|| panic!("{name} should parse"));
                assert_eq!(parsed, canonical, "{name}");
                assert_eq!(parsed_side, side, "{name}");
                assert_eq!(stamp, "2026-10-08T124530Z", "{name}");
            }
        }
    }

    #[test]
    fn ordinary_names_are_not_mistaken_for_copies() {
        for name in [
            "Timm.kdbx",
            "notes.txt",
            // rclone's own fixed-name form: no timestamp, so this pass has
            // already dealt with it and it is not a resolution candidate.
            "Timm.kdbx.conflict-local",
            // Missing stamp.
            "Timm.conflict-local-.kdbx",
            ".conflict-local-2026-10-08T124530Z",
        ] {
            assert!(parse_copy_name(name).is_none(), "{name}");
        }
    }

    #[test]
    fn names_are_unique_per_second_and_side() {
        let a = conflict_copy_name("f.kdbx", Side::Local, now());
        let b = conflict_copy_name("f.kdbx", Side::Remote, now());
        let c = conflict_copy_name("f.kdbx", Side::Local, now() + chrono::Duration::seconds(1));
        assert_ne!(a, b);
        assert_ne!(a, c);
    }

    async fn write(dir: &Path, name: &str, content: &str) -> PathBuf {
        let path = dir.join(name);
        tokio::fs::write(&path, content).await.unwrap();
        path
    }

    #[tokio::test]
    async fn a_clean_directory_yields_nothing() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Timm.kdbx", "db").await;
        assert!(run(dir.path(), now()).await.unwrap().is_empty());
        // And the file is untouched.
        assert_eq!(
            tokio::fs::read_to_string(dir.path().join("Timm.kdbx"))
                .await
                .unwrap(),
            "db"
        );
    }

    #[tokio::test]
    async fn a_missing_directory_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope");
        assert!(run(&missing, now()).await.unwrap().is_empty());
    }

    /// The ordinary case: rclone picked a winner, so the canonical name is
    /// already right and only the loser needs renaming.
    #[tokio::test]
    async fn renames_the_loser_and_leaves_the_winner_alone() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Timm.kdbx", "winner").await;
        write(dir.path(), "Timm.kdbx.conflict-local", "loser").await;

        let found = run(dir.path(), now()).await.unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, "Timm.kdbx");
        assert_eq!(found[0].copy, "Timm.conflict-local-2026-10-08T124530Z.kdbx");
        assert_eq!(found[0].side, Side::Local);
        assert!(!found[0].restored_canonical);

        assert_eq!(
            tokio::fs::read_to_string(dir.path().join("Timm.kdbx"))
                .await
                .unwrap(),
            "winner",
            "the winner must not be touched"
        );
        assert_eq!(
            tokio::fs::read_to_string(
                dir.path()
                    .join("Timm.conflict-local-2026-10-08T124530Z.kdbx")
            )
            .await
            .unwrap(),
            "loser"
        );
        assert!(
            !dir.path().join("Timm.kdbx.conflict-local").exists(),
            "the fixed-name copy should be gone"
        );
    }

    /// **The case this module exists for.** Equal mtimes mean rclone suffixes
    /// both sides and leaves no canonical file; KeePassXC would report its
    /// database missing.
    #[tokio::test]
    async fn restores_the_canonical_name_when_rclone_left_none() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "Timm.kdbx.conflict-local",
            "LOCAL-ENTRIES-longer",
        )
        .await;
        write(dir.path(), "Timm.kdbx.conflict-remote", "REMOTE").await;
        assert!(!dir.path().join("Timm.kdbx").exists());

        let found = run(dir.path(), now()).await.unwrap();

        // Invariant 1.
        assert!(
            dir.path().join("Timm.kdbx").exists(),
            "the canonical name must exist after the pass"
        );
        assert_eq!(
            tokio::fs::read_to_string(dir.path().join("Timm.kdbx"))
                .await
                .unwrap(),
            "LOCAL-ENTRIES-longer",
            "the larger copy should hold the canonical name"
        );

        // Invariant 2: both versions are still there, timestamped.
        assert_eq!(found.len(), 2);
        assert!(found.iter().all(|c| c.restored_canonical));
        let names: Vec<&str> = found.iter().map(|c| c.copy.as_str()).collect();
        assert!(
            names.contains(&"Timm.conflict-local-2026-10-08T124530Z.kdbx"),
            "{names:?}"
        );
        assert!(
            names.contains(&"Timm.conflict-remote-2026-10-08T124530Z.kdbx"),
            "{names:?}"
        );

        // Nothing was lost.
        let mut contents: Vec<String> = Vec::new();
        for name in &names {
            contents.push(
                tokio::fs::read_to_string(dir.path().join(name))
                    .await
                    .unwrap(),
            );
        }
        assert!(contents.contains(&"LOCAL-ENTRIES-longer".to_string()));
        assert!(contents.contains(&"REMOTE".to_string()));
    }

    /// Size decides first, because a password database that has grown has
    /// strictly more entries in it.
    #[tokio::test]
    async fn the_larger_copy_wins_the_canonical_name() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "db.kdbx.conflict-local", "small").await;
        write(
            dir.path(),
            "db.kdbx.conflict-remote",
            "much much larger content",
        )
        .await;

        run(dir.path(), now()).await.unwrap();
        assert_eq!(
            tokio::fs::read_to_string(dir.path().join("db.kdbx"))
                .await
                .unwrap(),
            "much much larger content"
        );
    }

    /// With equal sizes, the newer one wins; with both equal, local does.
    #[tokio::test]
    async fn ties_fall_through_to_mtime_then_to_local() {
        let dir = tempfile::tempdir().unwrap();
        let local = write(dir.path(), "f.txt.conflict-local", "aaaaa").await;
        let remote = write(dir.path(), "f.txt.conflict-remote", "bbbbb").await;

        // Same size; make the remote newer.
        let older = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        let newer = older + std::time::Duration::from_secs(60);
        filetime::set_file_mtime(&local, filetime::FileTime::from_system_time(older)).unwrap();
        filetime::set_file_mtime(&remote, filetime::FileTime::from_system_time(newer)).unwrap();

        run(dir.path(), now()).await.unwrap();
        assert_eq!(
            tokio::fs::read_to_string(dir.path().join("f.txt"))
                .await
                .unwrap(),
            "bbbbb",
            "the newer copy should win when sizes match"
        );
    }

    #[tokio::test]
    async fn handles_several_conflicting_files_at_once() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.kdbx", "winner-a").await;
        write(dir.path(), "a.kdbx.conflict-local", "loser-a").await;
        write(dir.path(), "b.txt.conflict-local", "only-b").await;

        let found = run(dir.path(), now()).await.unwrap();
        assert_eq!(found.len(), 2);
        // b had no winner, so it was restored; a did not need restoring.
        let b = found.iter().find(|c| c.path == "b.txt").unwrap();
        assert!(b.restored_canonical);
        let a = found.iter().find(|c| c.path == "a.kdbx").unwrap();
        assert!(!a.restored_canonical);
        assert!(dir.path().join("b.txt").exists());
    }

    /// Running twice must not duplicate or clobber. The second pass sees already
    /// normalised names, which are not rclone's fixed ones, so it does nothing.
    #[tokio::test]
    async fn the_pass_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Timm.kdbx", "winner").await;
        write(dir.path(), "Timm.kdbx.conflict-local", "loser").await;

        let first = run(dir.path(), now()).await.unwrap();
        assert_eq!(first.len(), 1);

        let second = run(dir.path(), now()).await.unwrap();
        assert!(
            second.is_empty(),
            "a second pass should find nothing: {second:?}"
        );

        assert_eq!(
            tokio::fs::read_to_string(dir.path().join("Timm.kdbx"))
                .await
                .unwrap(),
            "winner"
        );
    }

    /// Two conflicts on the same file in the same second must not destroy the
    /// first copy — it cannot be re-created.
    #[tokio::test]
    async fn an_existing_copy_with_the_same_name_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Timm.kdbx", "winner").await;
        write(
            dir.path(),
            "Timm.conflict-local-2026-10-08T124530Z.kdbx",
            "earlier",
        )
        .await;
        write(dir.path(), "Timm.kdbx.conflict-local", "later").await;

        run(dir.path(), now()).await.unwrap();

        assert_eq!(
            tokio::fs::read_to_string(
                dir.path()
                    .join("Timm.conflict-local-2026-10-08T124530Z.kdbx")
            )
            .await
            .unwrap(),
            "earlier",
            "the existing copy must survive"
        );
        assert!(
            dir.path().join("Timm.kdbx.conflict-local").exists(),
            "and the new one is left in place rather than discarded"
        );
    }

    #[tokio::test]
    async fn the_versions_directory_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let versions = dir.path().join(VERSIONS_DIR);
        tokio::fs::create_dir_all(&versions).await.unwrap();
        write(&versions, "Timm.kdbx.conflict-local", "snapshot").await;

        assert!(run(dir.path(), now()).await.unwrap().is_empty());
        assert!(versions.join("Timm.kdbx.conflict-local").exists());
    }

    /// Directories are not conflict copies, even if named like one.
    #[tokio::test]
    async fn a_directory_named_like_a_conflict_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::create_dir_all(dir.path().join("sub.conflict-local"))
            .await
            .unwrap();
        assert!(run(dir.path(), now()).await.unwrap().is_empty());
    }
}
