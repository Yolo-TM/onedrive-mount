use anyhow::Result;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use onedrive_mount::{config::Config, paths::config_file};
use tokio::sync::mpsc;
use tracing::warn;

pub enum ConfigEvent {
    Loaded(Config),
    ParseError(String),
}

pub struct ConfigWatcher {
    _watcher: RecommendedWatcher,
    _debounce_task: tokio::task::JoinHandle<()>,
}

impl ConfigWatcher {
    pub fn new(sender: mpsc::Sender<ConfigEvent>) -> Result<Self> {
        let path = config_file();

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

                // Now process the config change
                let cfg_path = config_file();
                match Config::load(&cfg_path) {
                    Ok(cfg) => {
                        let errors = cfg.validate();
                        if errors.is_empty() {
                            tracing::debug!("config file changed — sending to daemon");
                            let _ = sender.send(ConfigEvent::Loaded(cfg)).await;
                        } else {
                            let msg = errors.join("; ");
                            warn!(error = %msg, "config validation failed, keeping previous");
                            let _ = sender.send(ConfigEvent::ParseError(msg)).await;
                        }
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        warn!(error = %msg, "failed to parse updated config, keeping previous");
                        let _ = sender.send(ConfigEvent::ParseError(msg)).await;
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
