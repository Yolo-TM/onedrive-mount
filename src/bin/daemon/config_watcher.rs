use anyhow::Result;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use onedrive_mount::{config::Config, paths::config_file};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tracing::warn;

pub enum ConfigEvent {
    Loaded(Config),
    ParseError(String),
}

pub struct ConfigWatcher {
    _watcher: RecommendedWatcher,
}

impl ConfigWatcher {
    pub fn new(sender: mpsc::Sender<ConfigEvent>) -> Result<Self> {
        let path = config_file();

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
            let cfg_path = config_file();
            *guard = Some(handle.spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
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
            }));
        })?;

        if let Some(dir) = path.parent() {
            watcher.watch(dir, RecursiveMode::NonRecursive)?;
        }

        Ok(Self { _watcher: watcher })
    }
}
