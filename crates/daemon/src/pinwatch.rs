//! inotify on pinned directories.
//!
//! The interval is a floor, not the mechanism. Waiting up to five minutes to
//! upload a password database that was just saved is both annoying and
//! *unsafe*: the longer the two sides stay diverged, the more likely a conflict
//! becomes. Noticing the write is the cheapest way to reduce conflicts.
//!
//! Debouncing happens in [`crate::pins::Scheduler`], not here, so this stays a
//! plain "something changed in this pin" signal and the policy lives in one
//! place.

use std::path::Path;

use notify::{EventKind, RecursiveMode, Watcher};
use onedrive_core::config::Config;
use onedrive_core::paths::Paths;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::bisync::VERSIONS_DIR;

/// Watch every enabled pin. Each message is a pin name whose directory changed.
///
/// The returned watchers must be kept alive; dropping them stops the watch.
pub fn watch(
    config: &Config,
    paths: &Paths,
) -> (mpsc::Receiver<String>, Vec<Box<dyn Watcher + Send>>) {
    let (tx, rx) = mpsc::channel::<String>(64);
    let mut watchers: Vec<Box<dyn Watcher + Send>> = Vec::new();

    for remote in config.enabled_remotes() {
        for pin in remote.pins.iter().filter(|p| p.enabled && p.sync_on_change) {
            let dir = pin.local_path(paths);
            if let Err(e) = std::fs::create_dir_all(&dir) {
                warn!(
                    pin = %pin.name,
                    path = %dir.display(),
                    error = %e,
                    "cannot watch this pin for changes; it will still sync on its interval"
                );
                continue;
            }

            match spawn(&dir, pin.name.clone(), tx.clone()) {
                Ok(watcher) => {
                    debug!(pin = %pin.name, path = %dir.display(), "watching for local changes");
                    watchers.push(watcher);
                }
                // Losing the watch costs promptness, not correctness: the
                // interval still runs. Worth a warning, not a failure.
                Err(e) => warn!(
                    pin = %pin.name,
                    error = %e,
                    "cannot watch this pin for changes; it will still sync on its interval"
                ),
            }
        }
    }

    (rx, watchers)
}

fn spawn(
    dir: &Path,
    pin: String,
    tx: mpsc::Sender<String>,
) -> notify::Result<Box<dyn Watcher + Send>> {
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(event) = res else { return };

        // Only content-changing events. Access times and metadata reads would
        // otherwise make merely *looking* at the folder trigger a sync.
        if !matches!(
            event.kind,
            EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
        ) {
            return;
        }

        // Ignore our own bookkeeping, or taking a snapshot would trigger the
        // sync that takes the next snapshot.
        if event.paths.iter().all(|p| is_ours(p)) {
            return;
        }

        // Non-blocking: a full channel means a signal is already queued, which
        // is as good as queueing another.
        let _ = tx.try_send(pin.clone());
    })?;

    // Recursive, because a pin can have subdirectories and a change in one is
    // still a change to the pin.
    watcher.watch(dir, RecursiveMode::Recursive)?;
    Ok(Box::new(watcher))
}

/// Whether a path is something we created ourselves.
fn is_ours(path: &Path) -> bool {
    let text = path.to_string_lossy();

    // Snapshots, which we write immediately before every sync.
    if text.contains(&format!("/{VERSIONS_DIR}/")) || text.ends_with(VERSIONS_DIR) {
        return true;
    }
    // The access sentinel, written once at setup.
    if text.ends_with(crate::bisync::SENTINEL) {
        return true;
    }
    // rclone's in-flight temp files, and its fixed-name conflict copies — the
    // reconciliation pass renames those, and that rename must not look like a
    // user edit.
    if text.contains(".conflict-") || text.ends_with(".partial") {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Duration;

    /// Whether nothing was signalled within `ms`.
    ///
    /// "Nothing" covers two distinct shapes: the timeout expiring, and the
    /// channel being closed because `watch()` installed no watchers at all and
    /// so dropped the only sender. Both mean the same thing to the scheduler.
    async fn nothing_arrives(rx: &mut mpsc::Receiver<String>, ms: u64) -> bool {
        match tokio::time::timeout(Duration::from_millis(ms), rx.recv()).await {
            Err(_) => true,
            Ok(None) => true,
            Ok(Some(_)) => false,
        }
    }

    fn config_with(local: &Path, sync_on_change: bool) -> Config {
        toml::from_str(&format!(
            r#"
            [[remote]]
            name = "od"
            mount_point = "/tmp/mnt-does-not-matter"
            [[remote.pin]]
            name = "Keepass"
            remote_path = "Dokumente/Keepass"
            local_path = "{}"
            sync_on_change = {sync_on_change}
            "#,
            local.display()
        ))
        .unwrap()
    }

    #[tokio::test]
    async fn a_local_write_is_noticed() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path());
        let local = dir.path().join("pin");
        std::fs::create_dir_all(&local).unwrap();

        let (mut rx, _watchers) = watch(&config_with(&local, true), &paths);
        std::fs::write(local.join("Timm.kdbx"), "saved").unwrap();

        let got = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;
        assert_eq!(got.expect("should notice a write"), Some("Keepass".into()));
    }

    /// A save is write-temp-then-rename, so the rename has to count.
    #[tokio::test]
    async fn the_keepass_save_pattern_is_noticed() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path());
        let local = dir.path().join("pin");
        std::fs::create_dir_all(&local).unwrap();
        std::fs::write(local.join("Timm.kdbx"), "v1").unwrap();

        let (mut rx, _watchers) = watch(&config_with(&local, true), &paths);

        let tmp = local.join("Timm.kdbx.tmp");
        std::fs::write(&tmp, "v2").unwrap();
        std::fs::rename(&tmp, local.join("Timm.kdbx")).unwrap();

        let got = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;
        assert!(got.is_ok(), "a temp-then-rename save should be noticed");
    }

    #[tokio::test]
    async fn a_change_in_a_subdirectory_is_noticed() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path());
        let local = dir.path().join("pin");
        std::fs::create_dir_all(local.join("sub")).unwrap();

        let (mut rx, _watchers) = watch(&config_with(&local, true), &paths);
        std::fs::write(local.join("sub/deep.txt"), "x").unwrap();

        let got = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;
        assert!(got.is_ok(), "pins can have subdirectories");
    }

    /// Our own snapshot writes must not trigger the sync that would take the
    /// next snapshot.
    #[tokio::test]
    async fn our_own_snapshots_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path());
        let local = dir.path().join("pin");
        std::fs::create_dir_all(local.join(VERSIONS_DIR).join("2026-10-08T120000Z")).unwrap();

        let (mut rx, _watchers) = watch(&config_with(&local, true), &paths);
        std::fs::write(
            local
                .join(VERSIONS_DIR)
                .join("2026-10-08T120000Z/Timm.kdbx"),
            "snapshot",
        )
        .unwrap();

        assert!(
            nothing_arrives(&mut rx, 900).await,
            "a snapshot write must not trigger a sync"
        );
    }

    /// The reconciliation pass renames conflict copies; that must not read as a
    /// user edit, or every conflict would cause an extra sync.
    #[tokio::test]
    async fn conflict_copies_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path());
        let local = dir.path().join("pin");
        std::fs::create_dir_all(&local).unwrap();

        let (mut rx, _watchers) = watch(&config_with(&local, true), &paths);
        std::fs::write(local.join("Timm.kdbx.conflict-local"), "loser").unwrap();

        assert!(
            nothing_arrives(&mut rx, 900).await,
            "conflict copies are ours, not a user edit"
        );
    }

    #[tokio::test]
    async fn sync_on_change_false_is_respected() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path());
        let local = dir.path().join("pin");
        std::fs::create_dir_all(&local).unwrap();

        let (mut rx, watchers) = watch(&config_with(&local, false), &paths);
        assert!(watchers.is_empty(), "nothing should be watched");

        std::fs::write(local.join("Timm.kdbx"), "saved").unwrap();
        assert!(nothing_arrives(&mut rx, 900).await);
    }

    #[tokio::test]
    async fn a_pin_directory_that_does_not_exist_yet_is_created_and_watched() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path());
        let local = dir.path().join("not-yet");

        let (mut rx, watchers) = watch(&config_with(&local, true), &paths);
        assert_eq!(watchers.len(), 1);
        assert!(local.is_dir(), "the pin directory should have been created");

        std::fs::write(local.join("Timm.kdbx"), "x").unwrap();
        let got = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;
        assert!(got.is_ok());
    }

    #[test]
    fn our_own_files_are_recognised() {
        assert!(is_ours(&PathBuf::from(
            "/p/.versions/2026-10-08T120000Z/f.kdbx"
        )));
        assert!(is_ours(&PathBuf::from("/p/.versions")));
        assert!(is_ours(&PathBuf::from("/p/.onedrive-check")));
        assert!(is_ours(&PathBuf::from("/p/Timm.kdbx.conflict-local")));
        assert!(is_ours(&PathBuf::from(
            "/p/Timm.conflict-remote-2026-10-08T124530Z.kdbx"
        )));
        assert!(is_ours(&PathBuf::from("/p/x.partial")));

        // And a real edit is not.
        assert!(!is_ours(&PathBuf::from("/p/Timm.kdbx")));
        assert!(!is_ours(&PathBuf::from("/p/notes.txt")));
    }

    #[tokio::test]
    async fn a_disabled_remote_is_not_watched() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path());
        let config: Config = toml::from_str(
            r#"
            [[remote]]
            name = "od"
            mount_point = "/tmp/mnt"
            enabled = false
            [[remote.pin]]
            name = "Keepass"
            remote_path = "a"
            local_path = "/tmp/should-not-be-created-by-this-test"
            "#,
        )
        .unwrap();

        let (_rx, watchers) = watch(&config, &paths);
        assert!(watchers.is_empty());
        assert!(!Path::new("/tmp/should-not-be-created-by-this-test").exists());
    }
}
