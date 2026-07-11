use anyhow::Result;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use onedrive_mount::{paths::conflict_resolutions_file, resolution::ResolutionFile};
use tokio::sync::mpsc;
use tracing::warn;

pub enum ResolutionEvent {
    Loaded(ResolutionFile),
}

pub struct ResolutionWatcher {
    _watcher: RecommendedWatcher,
    _debounce_task: tokio::task::JoinHandle<()>,
}

impl ResolutionWatcher {
    pub fn new(sender: mpsc::Sender<ResolutionEvent>) -> Result<Self> {
        let path = conflict_resolutions_file();

        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }

        // Use a tokio mpsc channel to send notifications from the OS notify thread
        // to an async task, avoiding Handle::spawn which panics on shutdown.
        // try_send is safe to call from a non-async (OS thread) context.
        let (notify_tx, mut notify_rx) = mpsc::channel::<()>(4);

        let watch_path = path.clone();

        let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
            let Ok(event) = res else { return };

            if !matches!(event.kind, EventKind::Modify(_) | EventKind::Create(_)) {
                return;
            }

            if !event.paths.iter().any(|p| p == &watch_path) {
                return;
            }

            // Signal the async debounce task; ignore errors (buffer full or runtime gone)
            let _ = notify_tx.try_send(());
        })?;

        if let Some(dir) = path.parent() {
            watcher.watch(dir, RecursiveMode::NonRecursive)?;
        }

        // Spawn an async task that receives notifications and debounces them.
        let debounce_task = tokio::spawn(async move {
            loop {
                // Wait for a notification
                if notify_rx.recv().await.is_none() {
                    break; // channel closed
                }

                // Debounce: wait 200ms, draining any additional notifications
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                while notify_rx.try_recv().is_ok() {}

                // Now process the resolution file change
                let res_path = conflict_resolutions_file();
                match ResolutionFile::load(&res_path) {
                    Some(rf) if !rf.resolutions.is_empty() => {
                        tracing::debug!(
                            count = rf.resolutions.len(),
                            "resolution file changed — sending to daemon"
                        );
                        let _ = sender.send(ResolutionEvent::Loaded(rf)).await;
                    }
                    Some(_) => {}
                    None => {
                        warn!("failed to parse conflict-resolutions.toml");
                    }
                }
            }
        });

        Ok(Self {
            _watcher: watcher,
            _debounce_task: debounce_task,
        })
    }
}
