use std::path::PathBuf;

pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("onedrive-mount")
}

pub fn config_file() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn data_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("~/.local/share"))
        .join("onedrive-mount")
}

pub fn status_file() -> PathBuf {
    data_dir().join("status.toml")
}

pub fn daemon_pid_file() -> PathBuf {
    data_dir().join("daemon.pid")
}

pub fn gui_pid_file() -> PathBuf {
    data_dir().join("gui.pid")
}

pub fn conflict_resolutions_file() -> PathBuf {
    data_dir().join("conflict-resolutions.toml")
}

pub fn sync_baseline_file(remote_name: &str, rule_name: &str) -> PathBuf {
    data_dir()
        .join("baseline")
        .join(remote_name)
        .join(format!("{rule_name}.toml"))
}

pub fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/"))
            .join(rest)
    } else if path == "~" {
        dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
    } else {
        PathBuf::from(path)
    }
}
