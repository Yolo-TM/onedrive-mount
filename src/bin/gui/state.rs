// All mutable GUI state in one place so views receive only what they need

use crate::{rclone_config_wizard::Wizard, views::log_config::LogTailCache};

fn rclone_available() -> bool {
    std::process::Command::new("rclone")
        .arg("version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
use onedrive_mount::{config::Config, status::DaemonStatus};
use std::{
    sync::{Arc, Mutex},
    time::Instant,
};

pub struct State {
    pub config: Config,
    /// Tracks unsaved changes so the save button can be highlighted
    pub config_dirty: bool,
    pub status: Option<DaemonStatus>,
    /// Populated by a background thread at startup; updated after wizard completes
    pub available_remotes: Vec<String>,
    /// Which remote is open in the editor (index into config.remotes)
    pub selected_remote: Option<usize>,
    pub last_status_poll: Instant,
    /// Cached systemd active/enabled states — refreshed alongside the status poll (every 2s)
    pub daemon_active: bool,
    pub service_enabled: bool,
    /// Toast message shown after a successful save (cleared after ~3s)
    pub save_toast: Option<(String, Instant)>,
    pub service_error: Option<String>,
    /// Active when the user clicked "Add rclone remote"
    pub wizard: Option<Wizard>,
    pub log_tail: LogTailCache,
    /// Background thread result for list_remotes (None while loading)
    remotes_loading: Arc<Mutex<Option<Vec<String>>>>,
}

impl State {
    pub fn new() -> Self {
        // Kick off rclone listremotes in a background thread so the UI doesn't block
        let remotes_result: Arc<Mutex<Option<Vec<String>>>> = Arc::new(Mutex::new(None));
        let remotes_clone = remotes_result.clone();
        std::thread::spawn(move || {
            let remotes = crate::rclone_query::list_remotes();
            *remotes_clone.lock().unwrap() = Some(remotes);
        });

        let rclone_error = if !rclone_available() {
            Some("'rclone' not found in PATH. Install rclone to use this application.".into())
        } else {
            None
        };

        let (config, load_error) = match crate::config_io::load_result() {
            crate::config_io::LoadResult::Ok(c) => (c, None),
            crate::config_io::LoadResult::Missing => (Default::default(), None),
            crate::config_io::LoadResult::ParseError(e) => {
                (Default::default(), Some(format!("Failed to load config.toml: {e}")))
            }
        };

        Self {
            config,
            config_dirty: false,
            status: None,
            available_remotes: vec![],
            selected_remote: None,
            last_status_poll: Instant::now() - std::time::Duration::from_secs(10), // trigger on first frame
            daemon_active: false,
            service_enabled: false,
            save_toast: None,
            service_error: rclone_error.or(load_error),
            wizard: None,
            log_tail: LogTailCache::new(),
            remotes_loading: remotes_result,
        }
    }

    /// Checks if the background list_remotes has finished and stores the result.
    pub fn poll_remotes_loading(&mut self) {
        let mut guard = self.remotes_loading.lock().unwrap();
        if let Some(remotes) = guard.take() {
            self.available_remotes = remotes;
        }
    }
}
