// Watches conflict-resolutions.toml and sends parsed resolutions when it changes.
// Uses the same debounced inotify pattern as config_watcher.

use anyhow::Result;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use onedrive_mount::{paths::conflict_resolutions_file, resolution::ResolutionFile};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tracing::warn;

pub enum ResolutionEvent {
    Loaded(ResolutionFile),
}

pub struct ResolutionWatcher {
    _watcher: RecommendedWatcher,
}

impl ResolutionWatcher {
    pub fn new(sender: mpsc::Sender<ResolutionEvent>) -> Result<Self> {
        let path = conflict_resolutions_file();

        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }

        let pending: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>> = Arc::new(Mutex::new(None));
        let handle = tokio::runtime::Handle::current();
        let watch_path = path.clone();

        let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
            let Ok(event) = res else { return };

            if !matches!(event.kind, EventKind::Modify(_) | EventKind::Create(_)) {
                return;
            }

            if !event.paths.iter().any(|p| p == &watch_path) {
                return;
            }

            let mut guard = pending.lock().unwrap();
            if let Some(h) = guard.take() {
                h.abort();
            }

            let sender = sender.clone();
            let res_path = conflict_resolutions_file();
            *guard = Some(handle.spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                match ResolutionFile::load(&res_path) {
                    Some(rf) if !rf.resolutions.is_empty() => {
                        tracing::debug!(
                            count = rf.resolutions.len(),
                            "resolution file changed — sending to daemon"
                        );
                        let _ = sender.send(ResolutionEvent::Loaded(rf)).await;
                    }
                    Some(_) => {
                        // File is valid but empty — nothing to do
                    }
                    None => {
                        warn!("failed to parse conflict-resolutions.toml");
                    }
                }
            }));
        })?;

        if let Some(dir) = path.parent() {
            watcher.watch(dir, RecursiveMode::NonRecursive)?;
        }

        Ok(Self { _watcher: watcher })
    }
}
