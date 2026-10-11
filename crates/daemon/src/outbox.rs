//! Offline writes: staged visibly on disk, replayed only after re-verifying
//! the remote still matches the version the edit started from.
//!
//! The one risk this whole module exists to avoid: uploading a stale local
//! edit over something someone else changed while we were offline would be
//! the exact bug that started this rewrite, just reached through a new door.
//! So a replay never writes blind — it always checks the remote first, and a
//! mismatch becomes a conflict (both versions kept, nothing overwritten)
//! rather than a silent win for whichever side replays last.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use onedrive_core::config::RemoteConfig;
use onedrive_core::paths::{Paths, join_guarded};
use onedrive_core::state::{NodeRecord, OutboxRecord, Store};
use serde::Deserialize;
use tokio::process::Command;
use tracing::{info, warn};

use crate::app::App;
use crate::notify::{Alert, Notifier};
use crate::reconcile::{self, Side};

/// Where offline writes are staged — visible on purpose, so a pending upload
/// can be inspected (and, if needed, deleted) from a file manager.
pub fn stage_dir(remote: &RemoteConfig, paths: &Paths) -> PathBuf {
    remote.offline.outbox_dir(paths)
}

/// Stage a write for later replay.
///
/// Only queues a new row the first time a path is staged (see
/// `Store::queue_outbox`); a second offline edit to the same path before
/// replay just rewrites the staged bytes, still checked on replay against
/// whatever the remote looked like when the *first* edit started.
pub fn stage_write(
    store: &Store,
    remote_cfg: &RemoteConfig,
    paths: &Paths,
    path: &str,
    content: &[u8],
    baseline: Option<&NodeRecord>,
    now: DateTime<Utc>,
) -> Result<PathBuf> {
    let dir = stage_dir(remote_cfg, paths);
    let staged =
        join_guarded(&dir, path).with_context(|| format!("{path} escapes the outbox directory"))?;
    if let Some(parent) = staged.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&staged, content).with_context(|| format!("writing {}", staged.display()))?;

    store.queue_outbox(
        &remote_cfg.name,
        path,
        &staged.to_string_lossy(),
        baseline.map(|n| n.size),
        baseline.and_then(|n| n.quickxor.as_deref()),
        baseline.map(|n| n.mtime),
        now,
    )?;
    Ok(staged)
}

// ---------------------------------------------------------------------------
// Checking the remote
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum RemoteState {
    /// Nothing at that path — either it never existed, or it was deleted.
    Missing,
    Present {
        size: u64,
        quickxor: Option<String>,
    },
}

#[derive(Debug, Deserialize)]
struct Entry {
    #[serde(rename = "Size")]
    size: i64,
    #[serde(rename = "Hashes", default)]
    hashes: Option<Hashes>,
}

#[derive(Debug, Deserialize, Default)]
struct Hashes {
    quickxor: Option<String>,
}

/// What the remote currently has at `path` — the one network call a replay
/// makes per row, kept separate from any store access (same split as
/// `index::crawl_remote`/`apply_crawl`, for the same reason: the daemon's
/// `Store` lives behind a lock that cannot be held across an `.await`).
async fn check_remote(paths: &Paths, remote: &str, path: &str) -> Result<RemoteState> {
    let output = Command::new("rclone")
        .arg("lsjson")
        .arg("--no-traverse")
        .arg("--hash")
        .arg(format!("{remote}:{path}"))
        .arg(format!("--config={}", paths.rclone_config().display()))
        .arg("--low-level-retries=5")
        .arg("--timeout=30s")
        .arg("--contimeout=10s")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .context("spawning rclone lsjson (is it on PATH?)")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Verified against both a local `alias` stand-in and the real
        // onedrive-timm remote: a path matching no object and no directory
        // reports exactly this, whether the path never existed or was
        // deleted. Anything else is a check that failed, not an answer.
        if stderr.contains("directory not found") {
            return Ok(RemoteState::Missing);
        }
        anyhow::bail!("checking {remote}:{path}: {}", stderr.trim());
    }

    let entries: Vec<Entry> =
        serde_json::from_slice(&output.stdout).context("parsing rclone lsjson output")?;
    Ok(match entries.into_iter().next() {
        Some(e) => RemoteState::Present {
            size: e.size.max(0) as u64,
            quickxor: e.hashes.and_then(|h| h.quickxor),
        },
        None => RemoteState::Missing,
    })
}

/// Whether the remote still looks like what the edit started from.
///
/// Compared by hash when both sides have one — the R2 rule (§4) applied here
/// too: identity is a hash, never a timestamp. A baseline of "nothing" only
/// matches a remote that still has nothing; a remote that has since grown
/// something the index never saw counts as changed, not as agreement.
fn matches_baseline(row: &OutboxRecord, remote: &RemoteState) -> bool {
    match remote {
        RemoteState::Missing => row.base_size.is_none(),
        RemoteState::Present { size, quickxor } => match (&row.base_quickxor, quickxor) {
            (Some(a), Some(b)) => a == b,
            _ => row.base_size == Some(*size),
        },
    }
}

async fn upload(paths: &Paths, remote: &str, local: &Path, remote_path: &str) -> Result<()> {
    let output = Command::new("rclone")
        .arg("copyto")
        .arg(local)
        .arg(format!("{remote}:{remote_path}"))
        .arg("--checksum")
        .arg(format!("--config={}", paths.rclone_config().display()))
        .arg("--low-level-retries=5")
        .arg("--timeout=30s")
        .arg("--contimeout=10s")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .context("spawning rclone copyto (is it on PATH?)")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        anyhow::bail!("uploading {remote_path}: {stderr}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Replay
// ---------------------------------------------------------------------------

/// What a replay pass did, for the log line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReplayStats {
    pub uploaded: u64,
    pub conflicted: u64,
    pub failed: u64,
}

/// Replay every queued write for one remote.
///
/// Each row is independent: one failing (a check that errors, an upload that
/// errors) does not stop the rest from being tried.
///
/// Takes `&Arc<App>` rather than `&Store` directly: this function's own
/// `.await` points (the network checks and uploads below) would otherwise
/// need to carry a live `&Store` past them, and `Store` is `Send` but not
/// `Sync` (see `App`'s doc comment on its `store` field) — exactly the
/// combination that makes the containing future not `Send`, which is fatal
/// once this is reached from `mountloop::run`, itself spawned with
/// `tokio::spawn`. Routing every store access through `App::with_store`
/// keeps each one a short synchronous burst that never spans an `.await`.
pub async fn replay(
    app: &Arc<App>,
    notifier: &mut Notifier,
    remote_cfg: &RemoteConfig,
    paths: &Paths,
    now: DateTime<Utc>,
) -> Result<ReplayStats> {
    let rows: Vec<OutboxRecord> = app
        .with_store(|s| s.open_outbox())?
        .into_iter()
        .filter(|r| r.remote == remote_cfg.name)
        .collect();

    let mut stats = ReplayStats::default();
    for row in rows {
        match replay_one(app, notifier, remote_cfg, paths, &row, now).await {
            Ok(Outcome::Uploaded) => stats.uploaded += 1,
            Ok(Outcome::Conflicted) => stats.conflicted += 1,
            Err(e) => {
                stats.failed += 1;
                let msg = format!("{e:#}");
                warn!(remote = %remote_cfg.name, path = %row.path, "{msg}");
                app.with_store(|s| s.mark_outbox_failed(row.id, &msg))?;
            }
        }
    }
    Ok(stats)
}

enum Outcome {
    Uploaded,
    Conflicted,
}

async fn replay_one(
    app: &Arc<App>,
    notifier: &mut Notifier,
    remote_cfg: &RemoteConfig,
    paths: &Paths,
    row: &OutboxRecord,
    now: DateTime<Utc>,
) -> Result<Outcome> {
    let remote_state = check_remote(paths, &row.remote, &row.path).await?;
    let staged = PathBuf::from(&row.staged_path);

    if matches_baseline(row, &remote_state) {
        upload(paths, &row.remote, &staged, &row.path).await?;
        info!(remote = %row.remote, path = %row.path, "offline write replayed");
        app.with_store(|s| s.remove_outbox(row.id))?;
        // Uploaded as the canonical file; the staged copy has served its
        // purpose and would otherwise sit in the outbox folder forever.
        let _ = std::fs::remove_file(&staged);
        return Ok(Outcome::Uploaded);
    }

    // The remote moved while we were offline. Upload the staged edit as a
    // timestamped conflict copy — never overwrite what is there — and keep
    // the staged file in place, so the user's own edit stays visible and
    // recoverable locally as well as on the remote.
    let (dir, name) = match row.path.rsplit_once('/') {
        Some((d, n)) => (d, n),
        None => ("", row.path.as_str()),
    };
    let copy_name = reconcile::conflict_copy_name(name, Side::Local, now);
    let copy_path = if dir.is_empty() {
        copy_name
    } else {
        format!("{dir}/{copy_name}")
    };

    upload(paths, &row.remote, &staged, &copy_path).await?;
    warn!(
        remote = %row.remote,
        path = %row.path,
        copy = %copy_path,
        "the remote changed while this edit was offline; uploaded as a conflict copy"
    );
    app.with_store(|s| s.remove_outbox(row.id))?;

    let existing = app.with_store(|s| s.open_conflict_for("-", &row.path))?;
    let id = match existing {
        Some(id) => id,
        None => app.with_store(|s| {
            s.insert_conflict(
                "outbox",
                &row.remote,
                "-",
                &row.path,
                None,
                Some(&copy_path),
                None,
                now,
            )
        })?,
    };

    let alert = Alert::Conflict {
        pin: "an offline edit".to_string(),
        file: row.path.clone(),
        copies: vec![copy_path],
        folder: stage_dir(remote_cfg, paths).to_string_lossy().into_owned(),
        restored: false,
    };
    if let Ok(Some(notif_id)) = notifier.show(&alert, None).await {
        let _ = app.with_store(|s| s.set_conflict_notification(id, Some(notif_id)));
    }

    Ok(Outcome::Conflicted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use onedrive_core::config::Config;

    fn config(root: &std::path::Path) -> Config {
        toml::from_str(&format!(
            r#"
            [[remote]]
            name = "stand-in"
            mount_point = "{root}/OneDrive"
            "#,
            root = root.display(),
        ))
        .unwrap()
    }

    fn remote(root: &std::path::Path) -> RemoteConfig {
        config(root).remotes.into_iter().next().unwrap()
    }

    /// A minimal `App` for driving `replay`, which needs one to route its
    /// store access through (see `replay`'s doc comment for why).
    fn test_app(paths: &Paths, root: &std::path::Path) -> Arc<App> {
        let store = Store::open(&paths.state_db()).unwrap();
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel(1);
        Arc::new(App::new(
            paths.clone(),
            store,
            config(root),
            None,
            crate::sdnotify::Notifier::from_env(),
            tx,
            control_tx,
        ))
    }

    fn alias_conf(paths: &Paths, remote_dir: &std::path::Path) {
        std::fs::create_dir_all(paths.rclone_config().parent().unwrap()).unwrap();
        std::fs::write(
            paths.rclone_config(),
            format!(
                "[stand-in]\ntype = alias\nremote = {}\n",
                remote_dir.display()
            ),
        )
        .unwrap();
    }

    fn now() -> DateTime<Utc> {
        "2026-10-09T12:00:00Z".parse().unwrap()
    }

    #[test]
    fn staging_writes_the_file_and_queues_a_row() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path().join("home"));
        let store = Store::open_memory().unwrap();
        let r = remote(dir.path());

        let staged = stage_write(
            &store,
            &r,
            &paths,
            "Dokumente/notes.txt",
            b"hello",
            None,
            now(),
        )
        .unwrap();

        assert_eq!(std::fs::read(&staged).unwrap(), b"hello");
        assert!(staged.starts_with(paths.expand("~/OneDrive.pending")));
        let row = store
            .outbox_for("stand-in", "Dokumente/notes.txt")
            .unwrap()
            .unwrap();
        assert_eq!(row.base_size, None);
    }

    #[test]
    fn restaging_rewrites_the_bytes_without_touching_the_baseline() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path().join("home"));
        let store = Store::open_memory().unwrap();
        let r = remote(dir.path());
        let baseline = NodeRecord {
            path: "a.txt".into(),
            parent: "".into(),
            name: "a.txt".into(),
            is_dir: false,
            size: 1,
            mtime: now(),
            quickxor: Some("orig".into()),
        };

        stage_write(&store, &r, &paths, "a.txt", b"v1", Some(&baseline), now()).unwrap();
        let staged =
            stage_write(&store, &r, &paths, "a.txt", b"v2", Some(&baseline), now()).unwrap();

        assert_eq!(std::fs::read(&staged).unwrap(), b"v2");
        let row = store.outbox_for("stand-in", "a.txt").unwrap().unwrap();
        assert_eq!(
            row.base_quickxor.as_deref(),
            Some("orig"),
            "baseline is from the first stage"
        );
    }

    #[tokio::test]
    async fn a_new_file_with_no_conflict_replays_as_a_plain_upload() {
        let dir = tempfile::tempdir().unwrap();
        let remote_dir = dir.path().join("remote");
        std::fs::create_dir_all(&remote_dir).unwrap();
        let paths = Paths::rooted(dir.path().join("home"));
        alias_conf(&paths, &remote_dir);
        let r = remote(dir.path());
        let app = test_app(&paths, dir.path());
        let mut notifier = Notifier::connect().await;

        app.with_store(|s| stage_write(s, &r, &paths, "new.txt", b"brand new", None, now()))
            .unwrap();

        let stats = replay(&app, &mut notifier, &r, &paths, now())
            .await
            .unwrap();
        assert_eq!(
            stats,
            ReplayStats {
                uploaded: 1,
                conflicted: 0,
                failed: 0
            }
        );
        assert_eq!(
            std::fs::read(remote_dir.join("new.txt")).unwrap(),
            b"brand new"
        );
        assert!(app.with_store(|s| s.open_outbox()).unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_unchanged_remote_replays_as_a_plain_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let remote_dir = dir.path().join("remote");
        std::fs::create_dir_all(&remote_dir).unwrap();
        std::fs::write(remote_dir.join("a.txt"), "original").unwrap();
        let paths = Paths::rooted(dir.path().join("home"));
        alias_conf(&paths, &remote_dir);
        let r = remote(dir.path());
        let app = test_app(&paths, dir.path());
        let mut notifier = Notifier::connect().await;

        // The baseline this edit started from: what "original" actually hashes
        // to on the alias backend.
        let check = check_remote(&paths, "stand-in", "a.txt").await.unwrap();
        let RemoteState::Present { size, quickxor } = check else {
            panic!("must exist")
        };
        let baseline = NodeRecord {
            path: "a.txt".into(),
            parent: "".into(),
            name: "a.txt".into(),
            is_dir: false,
            size,
            mtime: now(),
            quickxor,
        };

        app.with_store(|s| {
            stage_write(
                s,
                &r,
                &paths,
                "a.txt",
                b"edited offline",
                Some(&baseline),
                now(),
            )
        })
        .unwrap();
        let stats = replay(&app, &mut notifier, &r, &paths, now())
            .await
            .unwrap();

        assert_eq!(
            stats,
            ReplayStats {
                uploaded: 1,
                conflicted: 0,
                failed: 0
            }
        );
        assert_eq!(
            std::fs::read(remote_dir.join("a.txt")).unwrap(),
            b"edited offline"
        );
        assert!(
            app.with_store(|s| s.open_outbox()).unwrap().is_empty(),
            "resolved rows are removed"
        );
    }

    /// The core safety case: the remote changed while the edit was staged.
    /// Nothing may be overwritten; both versions must still exist afterwards.
    #[tokio::test]
    async fn a_remote_that_changed_becomes_a_conflict_not_an_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let remote_dir = dir.path().join("remote");
        std::fs::create_dir_all(&remote_dir).unwrap();
        std::fs::write(remote_dir.join("a.txt"), "original").unwrap();
        let paths = Paths::rooted(dir.path().join("home"));
        alias_conf(&paths, &remote_dir);
        let r = remote(dir.path());
        let app = test_app(&paths, dir.path());
        let mut notifier = Notifier::connect().await;

        // Baseline recorded from "original", but the remote is edited again
        // before replay runs — simulating a change from elsewhere.
        let baseline = NodeRecord {
            path: "a.txt".into(),
            parent: "".into(),
            name: "a.txt".into(),
            is_dir: false,
            size: 8,
            mtime: now(),
            quickxor: Some("stale-hash-from-before-the-remote-edit".into()),
        };
        app.with_store(|s| {
            stage_write(
                s,
                &r,
                &paths,
                "a.txt",
                b"edited offline",
                Some(&baseline),
                now(),
            )
        })
        .unwrap();
        std::fs::write(remote_dir.join("a.txt"), "changed elsewhere").unwrap();

        let stats = replay(&app, &mut notifier, &r, &paths, now())
            .await
            .unwrap();
        assert_eq!(
            stats,
            ReplayStats {
                uploaded: 0,
                conflicted: 1,
                failed: 0
            }
        );

        // The remote's own canonical file is untouched...
        assert_eq!(
            std::fs::read_to_string(remote_dir.join("a.txt")).unwrap(),
            "changed elsewhere"
        );
        // ...and the offline edit is recoverable, both as an uploaded copy...
        let uploaded: Vec<_> = std::fs::read_dir(&remote_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        let copy_name = uploaded
            .iter()
            .find(|n| n.starts_with("a.conflict-local-"))
            .unwrap_or_else(|| panic!("no conflict copy among {uploaded:?}"));
        assert_eq!(
            std::fs::read_to_string(remote_dir.join(copy_name)).unwrap(),
            "edited offline"
        );
        // ...and in the outbox staging folder itself.
        let staged_path = paths.expand("~/OneDrive.pending").join("a.txt");
        assert_eq!(
            std::fs::read_to_string(&staged_path).unwrap(),
            "edited offline",
            "the staged copy must survive a conflict"
        );
        // resolved: removed from the outbox table, kept on disk

        let open = app.with_store(|s| s.open_conflicts()).unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].kind, "outbox");
        assert_eq!(open[0].path, "a.txt");
    }

    #[tokio::test]
    async fn a_remote_that_cannot_be_checked_leaves_the_row_queued() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path().join("home"));
        // No rclone.conf at all: the remote named "stand-in" does not exist.
        std::fs::create_dir_all(paths.rclone_config().parent().unwrap()).unwrap();
        std::fs::write(paths.rclone_config(), "").unwrap();
        let r = remote(dir.path());
        let app = test_app(&paths, dir.path());
        let mut notifier = Notifier::connect().await;

        app.with_store(|s| stage_write(s, &r, &paths, "a.txt", b"hello", None, now()))
            .unwrap();
        let stats = replay(&app, &mut notifier, &r, &paths, now())
            .await
            .unwrap();

        assert_eq!(
            stats,
            ReplayStats {
                uploaded: 0,
                conflicted: 0,
                failed: 1
            }
        );
        let row = app
            .with_store(|s| s.outbox_for("stand-in", "a.txt"))
            .unwrap()
            .unwrap();
        assert_eq!(row.state, "failed");
        assert_eq!(row.attempts, 1);
        assert!(row.last_err.is_some());
    }

    #[test]
    fn matches_baseline_treats_absence_as_a_baseline_too() {
        let no_baseline = OutboxRecord {
            id: 1,
            remote: "od".into(),
            path: "a".into(),
            staged_path: "/s/a".into(),
            base_size: None,
            base_quickxor: None,
            base_mtime: None,
            queued_at: now(),
            state: "queued".into(),
            attempts: 0,
            last_err: None,
        };
        assert!(matches_baseline(&no_baseline, &RemoteState::Missing));
        assert!(!matches_baseline(
            &no_baseline,
            &RemoteState::Present {
                size: 1,
                quickxor: None
            }
        ));

        let with_hash = OutboxRecord {
            base_size: Some(5),
            base_quickxor: Some("h1".into()),
            ..no_baseline
        };
        assert!(matches_baseline(
            &with_hash,
            &RemoteState::Present {
                size: 5,
                quickxor: Some("h1".into())
            }
        ));
        assert!(!matches_baseline(
            &with_hash,
            &RemoteState::Present {
                size: 5,
                quickxor: Some("h2".into())
            }
        ));
        assert!(!matches_baseline(&with_hash, &RemoteState::Missing));
    }
}
