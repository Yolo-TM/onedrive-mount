pub fn remote_type() -> String {
    "onedrive".into()
}

pub fn poll_interval() -> String {
    "30s".into()
}

pub fn vfs_cache_mode() -> String {
    "full".into()
}

pub fn vfs_cache_max_age() -> String {
    "72h".into()
}

pub fn vfs_cache_max_size() -> String {
    "20G".into()
}

pub fn vfs_write_back() -> String {
    "5s".into()
}

pub fn transfers() -> u32 {
    8
}

pub fn dir_cache_time() -> String {
    "15m".into()
}

pub fn extra_flags() -> Vec<String> {
    vec![]
}

pub fn log_file() -> String {
    "~/.local/share/onedrive-mount/daemon.log".into()
}

pub fn log_level() -> String {
    "NOTICE".into()
}

pub fn sync_patterns() -> Vec<String> {
    vec!["*".into()]
}

pub fn sync_interval() -> String {
    "15m".into()
}

pub fn enabled() -> bool {
    true
}

pub fn rule_enabled() -> bool {
    false
}

pub fn parse_interval_secs(s: &str) -> Option<u64> {
    let s = s.trim();
    let secs = if let Some(n) = s.strip_suffix('s') {
        n.parse::<u64>().ok()?
    } else if let Some(n) = s.strip_suffix('m') {
        n.parse::<u64>().ok()? * 60
    } else {
        let n = s.strip_suffix('h')?;
        n.parse::<u64>().ok()? * 3600
    };
    if secs == 0 { None } else { Some(secs) }
}
