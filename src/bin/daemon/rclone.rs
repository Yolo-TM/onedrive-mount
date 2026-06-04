// Constructs rclone command lines from typed config so callers never build strings by hand

use onedrive_mount::config::{LogConfig, RemoteConfig};
use std::process::Command;

/// Rejects strings that contain shell metacharacters or control characters.
/// Config values are passed directly as argv entries (not via a shell), so the
/// actual injection risk is low, but we still want to catch accidental garbage.
fn sanitize_flag(value: &str, field: &str) -> String {
    let bad: &[char] = &[';', '&', '|', '`', '$', '>', '<', '\n', '\r', '\0'];
    if value.chars().any(|c| bad.contains(&c)) {
        tracing::warn!(field, value, "config field contains unsafe characters — using empty string");
        String::new()
    } else {
        value.to_string()
    }
}

pub fn mount_command(remote: &RemoteConfig, log: &LogConfig) -> Command {
    let mut cmd = Command::new("rclone");
    cmd.arg("mount")
        .arg(format!("{}:", sanitize_flag(&remote.name, "remote.name")))
        .arg(onedrive_mount::paths::expand_tilde(&remote.mount_point))
        .arg(format!("--vfs-cache-mode={}", sanitize_flag(&remote.mount.vfs_cache_mode, "mount.vfs_cache_mode")))
        .arg(format!("--vfs-cache-max-age={}", sanitize_flag(&remote.mount.vfs_cache_max_age, "mount.vfs_cache_max_age")))
        .arg(format!("--vfs-cache-max-size={}", sanitize_flag(&remote.mount.vfs_cache_max_size, "mount.vfs_cache_max_size")))
        .arg(format!("--vfs-write-back={}", sanitize_flag(&remote.mount.vfs_write_back, "mount.vfs_write_back")))
        .arg(format!("--transfers={}", remote.mount.transfers))
        .arg(format!("--dir-cache-time={}", sanitize_flag(&remote.mount.dir_cache_time, "mount.dir_cache_time")))
        .arg(format!("--poll-interval={}", sanitize_flag(&remote.poll_interval, "poll_interval")))
        .arg(format!("--log-file={}", onedrive_mount::paths::expand_tilde(&log.file).display()))
        .arg(format!("--log-level={}", sanitize_flag(&log.level, "log.level")));

    for flag in &remote.mount.extra_flags {
        // Extra flags are passed as individual argv entries — still sanitize
        let clean = sanitize_flag(flag, "mount.extra_flags");
        if !clean.is_empty() {
            cmd.arg(clean);
        }
    }

    cmd
}

/// Copies files from `src` to `dst`, skipping files where the destination is newer.
pub fn copy_command(src: &str, dst: &str, patterns: &[String]) -> Command {
    let mut cmd = Command::new("rclone");
    cmd.arg("copy").arg(src).arg(dst).arg("--update");

    for p in patterns {
        cmd.arg("--include").arg(p);
    }

    cmd
}

/// Lists files that differ between `remote` and `local` for conflict detection.
/// Uses `--differ` to output only files that exist on both sides but have different content.
pub fn check_command(remote: &str, local: &str, patterns: &[String]) -> Command {
    let mut cmd = Command::new("rclone");
    // --differ writes conflicting file names to stderr; exit code is non-zero when any differ
    cmd.arg("check").arg(remote).arg(local).arg("--differ").arg("-");

    for p in patterns {
        cmd.arg("--include").arg(p);
    }

    cmd
}

pub fn fusermount_command(mount_point: &std::path::Path) -> Command {
    let mut cmd = Command::new("fusermount");
    cmd.arg("-u").arg(mount_point);
    cmd
}
