use onedrive_mount::config::{LogConfig, RemoteConfig};
use std::process::Command;

fn sanitize_flag(value: &str, field: &str) -> String {
    let bad: &[char] = &[';', '&', '|', '`', '$', '>', '<', '\n', '\r', '\0'];
    if value.chars().any(|c| bad.contains(&c)) {
        tracing::warn!(
            field,
            value,
            "config field contains unsafe characters — using empty string"
        );
        String::new()
    } else {
        value.to_string()
    }
}

#[allow(clippy::suspicious_command_arg_space)]
fn add_filter_args(cmd: &mut Command, patterns: &[String]) {
    for p in patterns {
        cmd.arg("--filter").arg(format!("+ {p}"));
    }
    cmd.arg("--filter").arg("- *");
}

#[allow(clippy::suspicious_command_arg_space)]
fn add_filter_args_excluding_conflicts(cmd: &mut Command, patterns: &[String]) {
    cmd.arg("--filter").arg("- *.conflict-*");
    for p in patterns {
        cmd.arg("--filter").arg(format!("+ {p}"));
    }
    cmd.arg("--filter").arg("- *");
}

pub fn mount_command(remote: &RemoteConfig, log: &LogConfig) -> Command {
    let mut cmd = Command::new("rclone");
    cmd.arg("mount")
        .arg(format!("{}:", sanitize_flag(&remote.name, "remote.name")))
        .arg(onedrive_mount::paths::expand_tilde(&remote.mount_point))
        .arg(format!(
            "--vfs-cache-mode={}",
            sanitize_flag(&remote.mount.vfs_cache_mode, "mount.vfs_cache_mode")
        ))
        .arg(format!(
            "--vfs-cache-max-age={}",
            sanitize_flag(&remote.mount.vfs_cache_max_age, "mount.vfs_cache_max_age")
        ))
        .arg(format!(
            "--vfs-cache-max-size={}",
            sanitize_flag(&remote.mount.vfs_cache_max_size, "mount.vfs_cache_max_size")
        ))
        .arg(format!(
            "--vfs-write-back={}",
            sanitize_flag(&remote.mount.vfs_write_back, "mount.vfs_write_back")
        ))
        .arg(format!("--transfers={}", remote.mount.transfers))
        .arg(format!(
            "--dir-cache-time={}",
            sanitize_flag(&remote.mount.dir_cache_time, "mount.dir_cache_time")
        ))
        .arg(format!(
            "--poll-interval={}",
            sanitize_flag(&remote.poll_interval, "poll_interval")
        ))
        .arg(format!(
            "--log-file={}",
            onedrive_mount::paths::expand_tilde(&log.file).display()
        ))
        .arg(format!(
            "--log-level={}",
            sanitize_flag(&log.level, "log.level")
        ));

    for flag in &remote.mount.extra_flags {
        let clean = sanitize_flag(flag, "mount.extra_flags");
        if !clean.is_empty() {
            cmd.arg(clean);
        }
    }

    cmd
}

pub fn copy_command(
    src: &str,
    dst: &str,
    patterns: &[String],
    mode: CopyMode,
    exclude_conflicts: bool,
) -> Command {
    let mut cmd = Command::new("rclone");
    cmd.arg("copy").arg(src).arg(dst);

    match mode {
        CopyMode::Normal => {}
        CopyMode::Update => {
            cmd.arg("--update");
        }
        CopyMode::IgnoreExisting => {
            cmd.arg("--ignore-existing");
        }
    }

    if exclude_conflicts {
        add_filter_args_excluding_conflicts(&mut cmd, patterns);
    } else {
        add_filter_args(&mut cmd, patterns);
    }

    cmd
}

pub fn sync_command(src: &str, dst: &str, patterns: &[String]) -> Command {
    let mut cmd = Command::new("rclone");
    cmd.arg("sync").arg(src).arg(dst);

    add_filter_args(&mut cmd, patterns);

    cmd
}

pub fn check_command(remote: &str, local: &str, patterns: &[String]) -> Command {
    let mut cmd = Command::new("rclone");
    cmd.arg("check")
        .arg(remote)
        .arg(local)
        .arg("--differ")
        .arg("-");

    add_filter_args(&mut cmd, patterns);

    cmd
}

#[allow(dead_code)]
pub fn notify_conflicts(rule_name: &str, count: usize) -> bool {
    if std::env::var("DISPLAY").is_err() && std::env::var("WAYLAND_DISPLAY").is_err() {
        return false;
    }

    let summary = format!("Sync conflict — {rule_name}");
    let body = format!(
        "{count} file(s) need resolution in rule '{rule_name}'.\nOpen onedrive-mount to resolve."
    );

    let result = Command::new("notify-send")
        .arg("--app-name=onedrive-mount")
        .arg("--urgency=critical")
        .arg(&summary)
        .arg(&body)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();

    result.is_ok()
}

pub async fn fusermount(mount_point: &std::path::Path) {
    let _ = tokio::process::Command::new(fusermount_binary())
        .arg("-u")
        .arg(mount_point)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await;
}

fn fusermount_binary() -> &'static str {
    use std::sync::OnceLock;
    static BINARY: OnceLock<&'static str> = OnceLock::new();
    BINARY.get_or_init(|| {
        let has_fuse3 = std::process::Command::new("fusermount3")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if has_fuse3 {
            "fusermount3"
        } else {
            "fusermount"
        }
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyMode {
    Normal,
    Update,
    IgnoreExisting,
}
