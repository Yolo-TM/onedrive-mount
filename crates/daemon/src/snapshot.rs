//! Pre-run snapshots into `<local_path>/.versions/`.
//!
//! These are **not** a version history. Conflict copies already preserve both
//! sides of a real conflict, so there is no need to keep old copies of a
//! database that is merging cleanly.
//!
//! They exist for the one class of event a conflict copy cannot cover: when
//! bisync is *correct* that only one side changed, so there is no conflict and
//! no second copy. The important case is a **propagated delete** — something
//! removes the file on one side, bisync faithfully removes it on the other, and
//! nothing kept a copy. `--max-delete` and `--check-access` make that unlikely,
//! not impossible.
//!
//! Two properties matter and are tested:
//!
//! * `.versions/` lives *inside* the pin but is excluded by the filter file, so
//!   it never leaves the machine. (rclone's own `--backup-dir` cannot be used
//!   for this: it is forbidden from overlapping the synced path, which is
//!   exactly where it needs to be.)
//! * A snapshot is only taken when content has actually changed, so an idle pin
//!   does not accumulate identical copies every five minutes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use onedrive_rc_core::config::PinConfig;
use onedrive_rc_core::units::Size;
use tracing::{debug, warn};

use crate::bisync::{SENTINEL, VERSIONS_DIR};

/// Timestamp format for a snapshot directory. Sorts lexicographically, which is
/// what makes pruning and "find the most recent" trivial.
const STAMP: &str = "%Y-%m-%dT%H%M%SZ";

/// What a snapshot run did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Taken {
    /// Where the files went, if any were taken.
    pub dir: Option<PathBuf>,
    pub files: usize,
    pub bytes: u64,
    /// Files skipped for being larger than `versions_max_size`.
    pub skipped_large: usize,
}

/// Take a snapshot of everything that has changed since the last one.
pub async fn take(pin: &PinConfig, local: &Path, now: DateTime<Utc>) -> Result<Taken> {
    if !pin.versions {
        return Ok(Taken::default());
    }

    let versions = local.join(VERSIONS_DIR);
    let previous = latest(&versions).await;

    let mut candidates = Vec::new();
    let mut skipped_large = 0usize;
    let max = pin.versions_max_size;

    for (name, meta) in list_files(local).await? {
        if meta.len() > max.as_bytes() {
            skipped_large += 1;
            debug!(
                file = %name,
                size = meta.len(),
                limit = %max,
                "not snapshotting a file over versions_max_size"
            );
            continue;
        }
        candidates.push((name, meta));
    }

    // Compare against the previous snapshot so an idle pin takes nothing.
    let mut changed: Vec<String> = Vec::new();
    for (name, _meta) in candidates {
        let prior = previous.as_ref().map(|dir| dir.join(&name));
        match prior {
            Some(prior) if same_content(&prior, &local.join(&name)).await => {}
            _ => changed.push(name),
        }
    }

    if changed.is_empty() {
        debug!("nothing changed since the last snapshot");
        return Ok(Taken::default());
    }

    let dir = versions.join(now.format(STAMP).to_string());
    tokio::fs::create_dir_all(&dir)
        .await
        .with_context(|| format!("creating {}", dir.display()))?;

    let mut taken = Taken {
        dir: Some(dir.clone()),
        ..Default::default()
    };
    for name in changed {
        let from = local.join(&name);
        let to = dir.join(&name);
        if let Some(parent) = to.parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
        match tokio::fs::copy(&from, &to).await {
            Ok(bytes) => {
                taken.files += 1;
                taken.bytes += bytes;
            }
            // A file that vanished between listing and copying is not an error
            // worth failing the sync for — the sync is the point, not the
            // snapshot.
            Err(e) => warn!(file = %name, error = %e, "could not snapshot a file"),
        }
    }
    taken.skipped_large = skipped_large;

    debug!(
        dir = %dir.display(),
        files = taken.files,
        bytes = taken.bytes,
        "snapshot taken"
    );
    Ok(taken)
}

/// Remove snapshot directories older than `versions_retention`.
pub async fn prune(pin: &PinConfig, local: &Path, now: DateTime<Utc>) -> Result<usize> {
    if !pin.versions {
        return Ok(0);
    }
    let versions = local.join(VERSIONS_DIR);
    let cutoff = now - chrono::Duration::seconds(pin.versions_retention.as_secs() as i64);

    let mut entries = match tokio::fs::read_dir(&versions).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e).with_context(|| format!("reading {}", versions.display())),
    };

    // Collect first so the newest can always be kept, however old it is: having
    // *some* recovery point is worth more than honouring retention exactly.
    let mut dirs: Vec<(DateTime<Utc>, PathBuf)> = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(at) = parse_stamp(&name) else {
            continue;
        };
        if entry.metadata().await.map(|m| m.is_dir()).unwrap_or(false) {
            dirs.push((at, entry.path()));
        }
    }
    dirs.sort_by_key(|(at, _)| *at);

    let mut removed = 0usize;
    let keep_newest = dirs.len().saturating_sub(1);
    for (i, (at, path)) in dirs.iter().enumerate() {
        if i >= keep_newest || *at >= cutoff {
            continue;
        }
        match tokio::fs::remove_dir_all(path).await {
            Ok(()) => removed += 1,
            Err(e) => warn!(dir = %path.display(), error = %e, "could not prune a snapshot"),
        }
    }
    if removed > 0 {
        debug!(removed, "pruned old snapshots");
    }
    Ok(removed)
}

/// Total bytes held in `.versions/`, for reporting.
pub async fn size(local: &Path) -> u64 {
    let versions = local.join(VERSIONS_DIR);
    let mut total = 0u64;
    let mut stack = vec![versions];
    while let Some(dir) = stack.pop() {
        let Ok(mut entries) = tokio::fs::read_dir(&dir).await else {
            continue;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            match entry.metadata().await {
                Ok(meta) if meta.is_dir() => stack.push(entry.path()),
                Ok(meta) => total += meta.len(),
                Err(_) => {}
            }
        }
    }
    total
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Files directly in the pin, excluding our own bookkeeping.
///
/// Deliberately shallow: a pin is expected to be a small folder of documents,
/// and walking an arbitrarily deep tree before every sync would turn a cheap
/// safety net into a cost. Nested files are still protected by conflict copies.
async fn list_files(local: &Path) -> Result<Vec<(String, std::fs::Metadata)>> {
    let mut out = Vec::new();
    let mut entries = match tokio::fs::read_dir(local).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e).with_context(|| format!("reading {}", local.display())),
    };

    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == VERSIONS_DIR || name == SENTINEL {
            continue;
        }
        // Conflict copies are themselves the recovery mechanism; snapshotting
        // them would double the storage for no benefit.
        if name.contains(".conflict-") {
            continue;
        }
        match entry.metadata().await {
            Ok(meta) if meta.is_file() => out.push((name, meta)),
            _ => {}
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// The most recent snapshot directory, if any.
async fn latest(versions: &Path) -> Option<PathBuf> {
    let mut entries = tokio::fs::read_dir(versions).await.ok()?;
    let mut best: Option<(DateTime<Utc>, PathBuf)> = None;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(at) = parse_stamp(&name) else {
            continue;
        };
        if best.as_ref().is_none_or(|(b, _)| at > *b) {
            best = Some((at, entry.path()));
        }
    }
    best.map(|(_, path)| path)
}

fn parse_stamp(name: &str) -> Option<DateTime<Utc>> {
    chrono::NaiveDateTime::parse_from_str(name, STAMP)
        .ok()
        .map(|naive| naive.and_utc())
}

/// Whether two files have identical content.
///
/// Size first, which rejects almost everything for free; only then read. A pin
/// is small by design, so a byte comparison is cheaper than maintaining hashes
/// and cannot disagree with itself.
async fn same_content(a: &Path, b: &Path) -> bool {
    let (Ok(ma), Ok(mb)) = (tokio::fs::metadata(a).await, tokio::fs::metadata(b).await) else {
        return false;
    };
    if ma.len() != mb.len() {
        return false;
    }
    match (tokio::fs::read(a).await, tokio::fs::read(b).await) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => false,
    }
}

/// Default ceiling, exposed for the status report.
pub const fn default_max_size() -> Size {
    Size::from_bytes(50 << 20)
}

/// Snapshot counts per file name, for diagnostics.
pub async fn counts(local: &Path) -> HashMap<String, usize> {
    let versions = local.join(VERSIONS_DIR);
    let mut counts: HashMap<String, usize> = HashMap::new();
    let Ok(mut dirs) = tokio::fs::read_dir(&versions).await else {
        return counts;
    };
    while let Ok(Some(dir)) = dirs.next_entry().await {
        let Ok(mut files) = tokio::fs::read_dir(dir.path()).await else {
            continue;
        };
        while let Ok(Some(file)) = files.next_entry().await {
            *counts
                .entry(file.file_name().to_string_lossy().into_owned())
                .or_default() += 1;
        }
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use onedrive_rc_core::config::Config;

    fn at(h: u32, m: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 8, h, m, 0).unwrap()
    }

    fn pin() -> PinConfig {
        let config: Config = toml::from_str(
            r#"
            [[remote]]
            name = "od"
            mount_point = "~/OneDrive"
            [[remote.pin]]
            name = "Keepass"
            remote_path = "Dokumente/Keepass"
            local_path = "~/onedrive-keepass"
            "#,
        )
        .unwrap();
        config
            .remotes
            .into_iter()
            .next()
            .unwrap()
            .pins
            .into_iter()
            .next()
            .unwrap()
    }

    async fn write(dir: &Path, name: &str, content: &str) {
        tokio::fs::write(dir.join(name), content).await.unwrap();
    }

    #[tokio::test]
    async fn takes_a_snapshot_of_a_changed_file() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Timm.kdbx", "v1").await;

        let taken = take(&pin(), dir.path(), at(12, 0)).await.unwrap();
        assert_eq!(taken.files, 1);
        assert_eq!(taken.bytes, 2);

        let snap = dir.path().join(VERSIONS_DIR).join("2026-10-08T120000Z");
        assert!(snap.is_dir(), "{:?}", taken.dir);
        assert_eq!(
            tokio::fs::read_to_string(snap.join("Timm.kdbx"))
                .await
                .unwrap(),
            "v1"
        );
    }

    /// An idle pin syncs every five minutes; it must not accumulate an identical
    /// copy each time.
    #[tokio::test]
    async fn an_unchanged_file_is_not_snapshotted_again() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Timm.kdbx", "v1").await;

        assert_eq!(take(&pin(), dir.path(), at(12, 0)).await.unwrap().files, 1);
        let second = take(&pin(), dir.path(), at(12, 5)).await.unwrap();
        assert_eq!(second.files, 0, "nothing changed, so nothing to snapshot");
        assert!(second.dir.is_none(), "and no empty directory is created");

        let dirs: Vec<_> = std::fs::read_dir(dir.path().join(VERSIONS_DIR))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(dirs.len(), 1, "{dirs:?}");
    }

    #[tokio::test]
    async fn a_changed_file_gets_a_new_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Timm.kdbx", "v1").await;
        take(&pin(), dir.path(), at(12, 0)).await.unwrap();

        write(dir.path(), "Timm.kdbx", "v2-with-new-entries").await;
        let second = take(&pin(), dir.path(), at(12, 5)).await.unwrap();
        assert_eq!(second.files, 1);

        // Both versions are recoverable.
        let base = dir.path().join(VERSIONS_DIR);
        assert_eq!(
            tokio::fs::read_to_string(base.join("2026-10-08T120000Z/Timm.kdbx"))
                .await
                .unwrap(),
            "v1"
        );
        assert_eq!(
            tokio::fs::read_to_string(base.join("2026-10-08T120500Z/Timm.kdbx"))
                .await
                .unwrap(),
            "v2-with-new-entries"
        );
    }

    /// The case snapshots exist for: a delete propagates, no conflict copy is
    /// produced, and the snapshot is the only remaining copy.
    #[tokio::test]
    async fn a_snapshot_survives_the_file_being_deleted() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Timm.kdbx", "the only copy").await;
        take(&pin(), dir.path(), at(12, 0)).await.unwrap();

        tokio::fs::remove_file(dir.path().join("Timm.kdbx"))
            .await
            .unwrap();

        let snap = dir
            .path()
            .join(VERSIONS_DIR)
            .join("2026-10-08T120000Z/Timm.kdbx");
        assert_eq!(
            tokio::fs::read_to_string(&snap).await.unwrap(),
            "the only copy",
            "the snapshot is the only recovery path for a propagated delete"
        );
    }

    #[tokio::test]
    async fn versions_false_takes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Timm.kdbx", "v1").await;

        let mut p = pin();
        p.versions = false;
        assert_eq!(
            take(&p, dir.path(), at(12, 0)).await.unwrap(),
            Taken::default()
        );
        assert!(!dir.path().join(VERSIONS_DIR).exists());
    }

    #[tokio::test]
    async fn files_over_the_size_limit_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("big.bin"), vec![0u8; 4096])
            .await
            .unwrap();
        write(dir.path(), "small.kdbx", "tiny").await;

        let mut p = pin();
        p.versions_max_size = Size::from_bytes(1024);

        let taken = take(&p, dir.path(), at(12, 0)).await.unwrap();
        assert_eq!(taken.files, 1);
        assert_eq!(taken.skipped_large, 1);

        let snap = taken.dir.unwrap();
        assert!(snap.join("small.kdbx").exists());
        assert!(!snap.join("big.bin").exists());
    }

    /// The sentinel and existing conflict copies are our own bookkeeping, not
    /// user data worth duplicating.
    #[tokio::test]
    async fn bookkeeping_files_are_not_snapshotted() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), SENTINEL, "sentinel").await;
        write(
            dir.path(),
            "Timm.conflict-local-2026-10-08T120000Z.kdbx",
            "copy",
        )
        .await;
        write(dir.path(), "Timm.kdbx", "real").await;

        let taken = take(&pin(), dir.path(), at(12, 0)).await.unwrap();
        assert_eq!(taken.files, 1);
        let snap = taken.dir.unwrap();
        assert!(snap.join("Timm.kdbx").exists());
        assert!(!snap.join(SENTINEL).exists());
        assert!(
            !snap
                .join("Timm.conflict-local-2026-10-08T120000Z.kdbx")
                .exists()
        );
    }

    #[tokio::test]
    async fn pruning_drops_old_snapshots_but_keeps_the_newest() {
        let dir = tempfile::tempdir().unwrap();
        let versions = dir.path().join(VERSIONS_DIR);

        for stamp in [
            "2026-01-01T000000Z",
            "2026-09-01T000000Z",
            "2026-10-08T120000Z",
        ] {
            let d = versions.join(stamp);
            tokio::fs::create_dir_all(&d).await.unwrap();
            tokio::fs::write(d.join("f"), b"x").await.unwrap();
        }

        // 14-day retention against 2026-10-08 drops the two old ones.
        let removed = prune(&pin(), dir.path(), at(12, 0)).await.unwrap();
        assert_eq!(removed, 2);
        assert!(versions.join("2026-10-08T120000Z").exists());
        assert!(!versions.join("2026-01-01T000000Z").exists());
    }

    /// Having *some* recovery point beats honouring retention exactly.
    #[tokio::test]
    async fn pruning_never_removes_the_only_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let ancient = dir.path().join(VERSIONS_DIR).join("2020-01-01T000000Z");
        tokio::fs::create_dir_all(&ancient).await.unwrap();
        tokio::fs::write(ancient.join("f"), b"x").await.unwrap();

        assert_eq!(prune(&pin(), dir.path(), at(12, 0)).await.unwrap(), 0);
        assert!(ancient.exists());
    }

    #[tokio::test]
    async fn pruning_a_missing_versions_dir_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(prune(&pin(), dir.path(), at(12, 0)).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn pruning_ignores_directories_that_are_not_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let versions = dir.path().join(VERSIONS_DIR);
        tokio::fs::create_dir_all(versions.join("not-a-timestamp"))
            .await
            .unwrap();

        assert_eq!(prune(&pin(), dir.path(), at(12, 0)).await.unwrap(), 0);
        assert!(versions.join("not-a-timestamp").exists());
    }

    #[tokio::test]
    async fn reports_size_and_counts() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Timm.kdbx", "12345").await;
        take(&pin(), dir.path(), at(12, 0)).await.unwrap();
        write(dir.path(), "Timm.kdbx", "1234567890").await;
        take(&pin(), dir.path(), at(12, 5)).await.unwrap();

        assert_eq!(size(dir.path()).await, 15);
        let counts = counts(dir.path()).await;
        assert_eq!(counts.get("Timm.kdbx"), Some(&2));
    }

    #[test]
    fn stamps_round_trip_and_sort() {
        let a = at(12, 0);
        let name = a.format(STAMP).to_string();
        assert_eq!(name, "2026-10-08T120000Z");
        assert_eq!(parse_stamp(&name), Some(a));
        assert_eq!(parse_stamp("nonsense"), None);

        // Lexicographic order matches chronological order, which is what makes
        // "latest" and pruning simple.
        let mut names = [
            "2026-10-08T120500Z",
            "2026-01-01T000000Z",
            "2026-10-08T120000Z",
        ];
        names.sort();
        assert_eq!(names[0], "2026-01-01T000000Z");
        assert_eq!(names[2], "2026-10-08T120500Z");
    }

    #[tokio::test]
    async fn an_empty_pin_takes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(take(&pin(), dir.path(), at(12, 0)).await.unwrap().files, 0);
    }
}
