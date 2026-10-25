//! Acting on a conflict the user has decided about.
//!
//! Everything else in the sync engine is built so that no file is ever removed
//! without being recoverable. This module is the one place that deletes a file
//! because somebody asked it to, so it is also the one place that takes a
//! snapshot *unconditionally* — including when `versions = false`, because that
//! setting is about routine syncing, not about a one-off explicit deletion.
//!
//! The decision is expressed as a side (`local` / `remote`) rather than a
//! filename, and the side is read back off the filenames rather than out of the
//! database: copies accumulate across repeated conflicts, and the `conflict`
//! row only ever recorded the first one. The filename is the authoritative
//! record of where a version came from.
//!
//! What this does *not* do is mark the conflict resolved. That is deliberate:
//! [`crate::pins::resolve_finished`] closes a conflict only once it can see for
//! itself that the extra copies are gone and the canonical file is in place, so
//! the row is closed on observed fact rather than on an intention.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use onedrive_rc_core::ipc::Keep;
use tracing::info;

use crate::bisync::VERSIONS_DIR;
use crate::reconcile::{self, Side};

/// Timestamp format for the snapshot directory, matching `snapshot.rs` so the
/// normal retention pruning picks these up too.
const STAMP: &str = "%Y-%m-%dT%H%M%SZ";

/// One extra version of a conflicted file, as found on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Copy {
    /// File name, relative to the pin directory.
    pub name: String,
    pub side: Side,
    /// The stamp from the filename. Sorts lexicographically.
    pub stamp: String,
}

/// What [`apply`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    /// The file now holding the canonical name.
    pub kept: String,
    /// Copies that were removed, after being snapshotted.
    pub removed: Vec<String>,
    /// Whether a copy had to be promoted into the canonical name, as opposed to
    /// the canonical file already holding the requested side.
    pub promoted: bool,
    /// Where the removed bytes still are.
    pub snapshot: PathBuf,
}

/// Every extra version of `canonical` sitting in `folder`, oldest first.
pub async fn copies(folder: &Path, canonical: &str) -> Result<Vec<Copy>> {
    let mut found = Vec::new();

    let mut entries = match tokio::fs::read_dir(folder).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(found),
        Err(e) => return Err(e).with_context(|| format!("reading {}", folder.display())),
    };

    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some((owner, side, stamp)) = reconcile::parse_copy_name(&name) else {
            continue;
        };
        if owner != canonical {
            continue;
        }
        found.push(Copy { name, side, stamp });
    }

    found.sort_by(|a, b| a.stamp.cmp(&b.stamp).then_with(|| a.name.cmp(&b.name)));
    Ok(found)
}

/// Keep one side of a conflict and remove the others.
///
/// `keep` must be [`Keep::Local`] or [`Keep::Remote`]; [`Keep::Both`] changes no
/// files and so never reaches here.
pub async fn apply(
    folder: &Path,
    canonical: &str,
    keep: Keep,
    now: DateTime<Utc>,
) -> Result<Applied> {
    let wanted = match keep {
        Keep::Local => Side::Local,
        Keep::Remote => Side::Remote,
        Keep::Both => bail!("`--keep both` changes no files"),
    };

    let all = copies(folder, canonical).await?;
    if all.is_empty() {
        bail!(
            "no extra versions of {canonical} are left in {} — nothing to choose between",
            folder.display()
        );
    }

    let canonical_path = folder.join(canonical);
    let canonical_exists = tokio::fs::try_exists(&canonical_path)
        .await
        .unwrap_or(false);

    // The newest copy from the requested side, if that side has one. If it does
    // not, the requested side is already the one holding the canonical name.
    let promote = all.iter().rfind(|c| c.side == wanted).cloned();
    if promote.is_none() && !canonical_exists {
        // R3: this is the "I don't know" case. Every remaining copy is from the
        // other side, and the file that would have been the requested version
        // is not there.
        bail!(
            "no {} version of {canonical} exists — the copies present are: {}",
            wanted.as_str(),
            all.iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    // Snapshot first, and snapshot everything: the copies about to be removed
    // and the canonical file about to be replaced.
    let snapshot = folder
        .join(VERSIONS_DIR)
        .join(format!("{}-resolve", now.format(STAMP)));
    tokio::fs::create_dir_all(&snapshot)
        .await
        .with_context(|| format!("creating {}", snapshot.display()))?;

    if canonical_exists {
        preserve(&canonical_path, &snapshot.join(canonical)).await?;
    }
    for copy in &all {
        preserve(&folder.join(&copy.name), &snapshot.join(&copy.name)).await?;
    }

    // Promote, then remove. This order means an interruption leaves the
    // canonical name populated rather than absent, which is invariant 1 of the
    // reconciliation pass.
    let mut promoted = false;
    if let Some(copy) = &promote {
        tokio::fs::rename(folder.join(&copy.name), &canonical_path)
            .await
            .with_context(|| format!("promoting {} to {canonical}", copy.name))?;
        promoted = true;
    }

    let mut removed = Vec::new();
    for copy in &all {
        if promote.as_ref().is_some_and(|p| p.name == copy.name) {
            continue;
        }
        tokio::fs::remove_file(folder.join(&copy.name))
            .await
            .with_context(|| format!("removing {}", copy.name))?;
        removed.push(copy.name.clone());
    }

    info!(
        file = %canonical,
        keep = keep.as_str(),
        promoted,
        removed = removed.len(),
        snapshot = %snapshot.display(),
        "conflict resolved on request"
    );

    Ok(Applied {
        kept: canonical.to_owned(),
        removed,
        promoted,
        snapshot,
    })
}

/// Copy a file into the snapshot. A copy rather than a move, so the source is
/// intact if anything later in the sequence fails.
async fn preserve(from: &Path, to: &Path) -> Result<()> {
    tokio::fs::copy(from, to)
        .await
        .with_context(|| format!("preserving {} before removing it", from.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        "2026-10-09T09:00:00Z".parse().unwrap()
    }

    /// Lay out a folder the way a conflict leaves it.
    fn folder() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Timm.kdbx"), "LOCAL-ENTRIES-AB").unwrap();
        std::fs::write(
            dir.path()
                .join("Timm.conflict-remote-2026-10-08T124530Z.kdbx"),
            "REMOTE-ENTRIES-C",
        )
        .unwrap();
        dir
    }

    fn read(dir: &Path, name: &str) -> Option<String> {
        std::fs::read_to_string(dir.join(name)).ok()
    }

    #[tokio::test]
    async fn copies_are_found_and_attributed_to_a_side() {
        let dir = folder();
        let found = copies(dir.path(), "Timm.kdbx").await.unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].side, Side::Remote);
        assert_eq!(found[0].stamp, "2026-10-08T124530Z");
    }

    #[tokio::test]
    async fn copies_of_other_files_are_not_confused_with_ours() {
        let dir = folder();
        std::fs::write(
            dir.path()
                .join("Other.conflict-local-2026-10-08T124530Z.kdbx"),
            "x",
        )
        .unwrap();
        std::fs::write(dir.path().join("Timm.kdbx.bak"), "x").unwrap();

        let found = copies(dir.path(), "Timm.kdbx").await.unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
    }

    /// The common case: the canonical file already holds the local version, so
    /// keeping local just drops the remote copy.
    #[tokio::test]
    async fn keeping_local_removes_the_remote_copy_without_touching_the_original() {
        let dir = folder();
        let applied = apply(dir.path(), "Timm.kdbx", Keep::Local, now())
            .await
            .unwrap();

        assert!(!applied.promoted, "the canonical file was already local");
        assert_eq!(
            applied.removed,
            vec!["Timm.conflict-remote-2026-10-08T124530Z.kdbx".to_string()]
        );
        assert_eq!(read(dir.path(), "Timm.kdbx").unwrap(), "LOCAL-ENTRIES-AB");
        assert!(read(dir.path(), "Timm.conflict-remote-2026-10-08T124530Z.kdbx").is_none());
    }

    #[tokio::test]
    async fn keeping_remote_promotes_the_copy_into_the_canonical_name() {
        let dir = folder();
        let applied = apply(dir.path(), "Timm.kdbx", Keep::Remote, now())
            .await
            .unwrap();

        assert!(applied.promoted);
        assert!(
            applied.removed.is_empty(),
            "the local version was the canonical one"
        );
        assert_eq!(read(dir.path(), "Timm.kdbx").unwrap(), "REMOTE-ENTRIES-C");
        assert!(read(dir.path(), "Timm.conflict-remote-2026-10-08T124530Z.kdbx").is_none());
    }

    /// R1: the bytes of whatever was discarded are still on disk afterwards.
    #[tokio::test]
    async fn the_discarded_version_is_still_recoverable() {
        for (keep, discarded) in [
            (Keep::Local, "REMOTE-ENTRIES-C"),
            (Keep::Remote, "LOCAL-ENTRIES-AB"),
        ] {
            let dir = folder();
            let applied = apply(dir.path(), "Timm.kdbx", keep, now()).await.unwrap();

            let snapshot = &applied.snapshot;
            assert!(snapshot.is_dir(), "{}", snapshot.display());

            let mut contents: Vec<String> = std::fs::read_dir(snapshot)
                .unwrap()
                .map(|e| std::fs::read_to_string(e.unwrap().path()).unwrap())
                .collect();
            contents.sort();
            assert!(
                contents.iter().any(|c| c == discarded),
                "keeping {} must leave {discarded:?} in the snapshot, found {contents:?}",
                keep.as_str()
            );
            // And the kept version too, so the folder can be reconstructed.
            assert_eq!(contents.len(), 2, "{contents:?}");
        }
    }

    /// The tie case, where reconciliation restored the canonical name: both a
    /// local and a remote copy exist beside it.
    #[tokio::test]
    async fn with_copies_from_both_sides_the_requested_one_wins() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Timm.kdbx"), "RESTORED").unwrap();
        std::fs::write(
            dir.path()
                .join("Timm.conflict-local-2026-10-08T124530Z.kdbx"),
            "LOCAL",
        )
        .unwrap();
        std::fs::write(
            dir.path()
                .join("Timm.conflict-remote-2026-10-08T124530Z.kdbx"),
            "REMOTE",
        )
        .unwrap();

        let applied = apply(dir.path(), "Timm.kdbx", Keep::Remote, now())
            .await
            .unwrap();
        assert!(applied.promoted);
        assert_eq!(read(dir.path(), "Timm.kdbx").unwrap(), "REMOTE");
        assert_eq!(applied.removed.len(), 1);
        assert_eq!(copies(dir.path(), "Timm.kdbx").await.unwrap(), vec![]);
    }

    /// Repeated conflicts accumulate copies; the newest from the chosen side is
    /// the one that should win, not an arbitrary one.
    #[tokio::test]
    async fn the_newest_copy_from_the_chosen_side_is_promoted() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Timm.kdbx"), "CANONICAL").unwrap();
        for (stamp, body) in [
            ("2026-10-01T090000Z", "OLD-REMOTE"),
            ("2026-10-08T124530Z", "NEW-REMOTE"),
        ] {
            std::fs::write(
                dir.path()
                    .join(format!("Timm.conflict-remote-{stamp}.kdbx")),
                body,
            )
            .unwrap();
        }

        let applied = apply(dir.path(), "Timm.kdbx", Keep::Remote, now())
            .await
            .unwrap();
        assert_eq!(read(dir.path(), "Timm.kdbx").unwrap(), "NEW-REMOTE");
        assert_eq!(applied.removed.len(), 1, "the older remote copy goes too");
    }

    #[tokio::test]
    async fn nothing_to_resolve_is_an_error_rather_than_a_silent_success() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Timm.kdbx"), "only one").unwrap();

        let err = apply(dir.path(), "Timm.kdbx", Keep::Local, now())
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("no extra versions"), "{err:#}");
        assert_eq!(read(dir.path(), "Timm.kdbx").unwrap(), "only one");
    }

    /// R3: asking for a side that does not exist aborts without touching
    /// anything, rather than falling back to the other one.
    #[tokio::test]
    async fn asking_for_a_side_that_is_not_there_aborts() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path()
                .join("Timm.conflict-remote-2026-10-08T124530Z.kdbx"),
            "REMOTE",
        )
        .unwrap();

        let err = apply(dir.path(), "Timm.kdbx", Keep::Local, now())
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("no local version"), "{err:#}");
        assert!(
            read(dir.path(), "Timm.conflict-remote-2026-10-08T124530Z.kdbx").is_some(),
            "a refusal must change nothing"
        );
        assert!(
            !dir.path().join(VERSIONS_DIR).exists(),
            "not even a snapshot"
        );
    }

    #[tokio::test]
    async fn keep_both_never_reaches_the_file_operations() {
        let dir = folder();
        assert!(
            apply(dir.path(), "Timm.kdbx", Keep::Both, now())
                .await
                .is_err()
        );
        assert!(read(dir.path(), "Timm.conflict-remote-2026-10-08T124530Z.kdbx").is_some());
    }

    /// Running it twice must not destroy the answer from the first run.
    #[tokio::test]
    async fn resolving_an_already_resolved_conflict_is_refused() {
        let dir = folder();
        apply(dir.path(), "Timm.kdbx", Keep::Local, now())
            .await
            .unwrap();

        let err = apply(dir.path(), "Timm.kdbx", Keep::Local, now())
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("no extra versions"), "{err:#}");
        assert_eq!(read(dir.path(), "Timm.kdbx").unwrap(), "LOCAL-ENTRIES-AB");
    }

    /// The snapshot goes under `.versions/`, which the filters exclude, so
    /// resolving a conflict never uploads the version the user just discarded.
    #[tokio::test]
    async fn the_snapshot_lands_where_sync_will_not_see_it() {
        let dir = folder();
        let applied = apply(dir.path(), "Timm.kdbx", Keep::Local, now())
            .await
            .unwrap();
        assert!(
            applied.snapshot.starts_with(dir.path().join(VERSIONS_DIR)),
            "{}",
            applied.snapshot.display()
        );
    }
}
