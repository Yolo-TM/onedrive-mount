//! Watch the config file for edits.
//!
//! Keeps the approach from the old `config_watcher.rs`, which got this right:
//! watch the *parent directory* rather than the file, because an atomic
//! `write-temp-then-rename` save replaces the inode and a file watch would be
//! left watching the old one. Accepting `Create` as well as `Modify` is what
//! makes the rename visible.
//!
//! The debounce matters because editors and our own atomic save produce several
//! events per logical change.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use notify::{EventKind, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tracing::{debug, warn};

const DEBOUNCE: Duration = Duration::from_millis(200);

/// Start watching; each message means "the config changed, re-read it".
///
/// The returned `Watcher` must be kept alive for the watch to stay active.
pub fn watch(config_file: &Path) -> Result<(mpsc::Receiver<()>, Box<dyn Watcher + Send>)> {
    let dir = config_file
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    let target = config_file.to_path_buf();
    let (raw_tx, mut raw_rx) = mpsc::channel::<()>(16);

    // `notify` calls us from its own thread, so hop onto the async side with a
    // non-blocking send; dropping events under a burst is fine because the
    // debounce collapses them anyway.
    let mut watcher =
        notify::recommended_watcher(move |res: notify::Result<notify::Event>| match res {
            Ok(event) => {
                if !matches!(event.kind, EventKind::Modify(_) | EventKind::Create(_)) {
                    return;
                }
                if event.paths.contains(&target) {
                    let _ = raw_tx.try_send(());
                }
            }
            Err(e) => warn!(error = %e, "config watcher error"),
        })
        .context("creating the config watcher")?;

    watcher
        .watch(&dir, RecursiveMode::NonRecursive)
        .with_context(|| format!("watching {}", dir.display()))?;

    // Debounce: on the first event, wait a little and drain anything that
    // arrives in the meantime, then emit one message.
    let (tx, rx) = mpsc::channel::<()>(1);
    tokio::spawn(async move {
        while raw_rx.recv().await.is_some() {
            tokio::time::sleep(DEBOUNCE).await;
            while raw_rx.try_recv().is_ok() {}
            debug!("config change detected");
            // A full channel means a reload is already pending, which is as good
            // as sending another one.
            let _ = tx.try_send(());
        }
    });

    Ok((rx, Box::new(watcher)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An atomic save (write temp, rename over the target) must be noticed. This
    /// is the case a naive watch-the-file implementation misses.
    #[tokio::test]
    async fn notices_an_atomic_rename_save() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(&config, "# initial").unwrap();

        let (mut rx, _watcher) = watch(&config).unwrap();

        let tmp = dir.path().join("config.toml.tmp");
        std::fs::write(&tmp, "# replaced").unwrap();
        std::fs::rename(&tmp, &config).unwrap();

        let got = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;
        assert!(got.is_ok(), "an atomic save should produce a reload event");
    }

    #[tokio::test]
    async fn notices_an_in_place_edit() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(&config, "# initial").unwrap();

        let (mut rx, _watcher) = watch(&config).unwrap();
        std::fs::write(&config, "# edited").unwrap();

        let got = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;
        assert!(
            got.is_ok(),
            "an in-place edit should produce a reload event"
        );
    }

    /// Several writes in quick succession should collapse into roughly one
    /// reload, not one per write.
    #[tokio::test]
    async fn collapses_a_burst_of_writes() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(&config, "# initial").unwrap();

        let (mut rx, _watcher) = watch(&config).unwrap();
        for i in 0..20 {
            std::fs::write(&config, format!("# edit {i}")).unwrap();
        }

        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("at least one event")
            .expect("channel open");

        // Give the debounce time to settle, then confirm we are not sitting on a
        // backlog of one event per write.
        tokio::time::sleep(Duration::from_millis(600)).await;
        let mut extra = 0;
        while rx.try_recv().is_ok() {
            extra += 1;
        }
        assert!(
            extra <= 1,
            "burst should collapse, got {extra} extra events"
        );
    }

    #[tokio::test]
    async fn ignores_unrelated_files_in_the_same_directory() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(&config, "# initial").unwrap();

        let (mut rx, _watcher) = watch(&config).unwrap();
        std::fs::write(dir.path().join("notes.txt"), "unrelated").unwrap();

        let got = tokio::time::timeout(Duration::from_millis(800), rx.recv()).await;
        assert!(got.is_err(), "an unrelated file must not trigger a reload");
    }

    /// The daemon has to start even when the config file does not exist yet, and
    /// then notice it appearing.
    #[tokio::test]
    async fn watches_a_config_that_does_not_exist_yet() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");

        let (mut rx, _watcher) = watch(&config).unwrap();
        std::fs::write(&config, "# created later").unwrap();

        let got = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;
        assert!(
            got.is_ok(),
            "creating the config should produce a reload event"
        );
    }
}
