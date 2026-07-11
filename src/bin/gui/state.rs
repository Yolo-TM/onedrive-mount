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
    pub config_dirty: bool,
    pub status: Option<DaemonStatus>,
    pub available_remotes: Vec<String>,
    pub selected_remote: Option<usize>,
    pub last_status_poll: Instant,
    pub daemon_active: bool,
    pub service_enabled: bool,
    pub save_toast: Option<(String, Instant)>,
    pub service_error: Option<String>,
    pub wizard: Option<Wizard>,
    pub log_tail: LogTailCache,
    remotes_loading: Arc<Mutex<Option<Vec<String>>>>,
}

impl State {
    pub fn new() -> Self {
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
            crate::config_io::LoadResult::ParseError(e) => (
                Default::default(),
                Some(format!("Failed to load config.toml: {e}")),
            ),
        };

        Self {
            config,
            config_dirty: false,
            status: None,
            available_remotes: vec![],
            selected_remote: None,
            last_status_poll: Instant::now() - std::time::Duration::from_secs(10),
            daemon_active: false,
            service_enabled: false,
            save_toast: None,
            service_error: rclone_error.or(load_error),
            wizard: None,
            log_tail: LogTailCache::new(),
            remotes_loading: remotes_result,
        }
    }

    pub fn poll_remotes_loading(&mut self) {
        let mut guard = self.remotes_loading.lock().unwrap();
        if let Some(remotes) = guard.take() {
            self.available_remotes = remotes;
        }
    }
}
