// Constructs rclone command lines from typed config so callers never build strings by hand

use onedrive_mount::config::{LogConfig, RemoteConfig};
use std::process::Command;

/// Rejects strings that contain shell metacharacters or control characters.
/// Config values are passed directly as argv entries (not via a shell), so the
/// actual injection risk is low, but we still want to catch accidental garbage.
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

/// Appends `--filter` arguments for the given include patterns.
/// Uses `--filter "+ <pattern>"` for each pattern, then `--filter "- *"` to exclude everything
/// else. This avoids the indeterminate ordering problem of mixing `--include` and `--exclude`.
///
/// Note: rclone `--filter` takes a single argument with an embedded space (e.g. `"- *"`).
/// This is intentional, not a split-argument bug.
#[allow(clippy::suspicious_command_arg_space)]
fn add_filter_args(cmd: &mut Command, patterns: &[String]) {
    for p in patterns {
        cmd.arg("--filter").arg(format!("+ {p}"));
    }
    cmd.arg("--filter").arg("- *");
}

/// Appends filter args for bidirectional sync: includes the user patterns
/// but always excludes `.conflict-*` files so they stay local only.
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
        // Extra flags are passed as individual argv entries — still sanitize
        let clean = sanitize_flag(flag, "mount.extra_flags");
        if !clean.is_empty() {
            cmd.arg(clean);
        }
    }

    cmd
}

/// Copies files from `src` to `dst`.
/// `mode` controls update/ignore-existing behaviour.
/// `exclude_conflicts` adds a filter to keep `.conflict-*` files local only.
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

/// Syncs `src` to `dst`, making `dst` an exact replica of `src`.
/// Destructive: deletes files in `dst` that don't exist in `src`.
pub fn sync_command(src: &str, dst: &str, patterns: &[String]) -> Command {
    let mut cmd = Command::new("rclone");
    cmd.arg("sync").arg(src).arg(dst);

    add_filter_args(&mut cmd, patterns);

    cmd
}

/// Lists files that differ between `remote` and `local` for conflict detection.
/// Uses `--differ -` to write differing filenames to stdout (one per line, no prefix).
/// Exit code is non-zero when differences exist — that's expected, not an error.
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

/// Sends a desktop notification about sync conflicts. Fire-and-forget — the daemon
/// never blocks on this. Returns false if no DISPLAY is available.
#[allow(dead_code)] // Infrastructure for Phase 2 conflict detection wiring
pub fn notify_conflicts(rule_name: &str, count: usize) -> bool {
    // Check for a display server
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

    result.is_ok() // fire and forget — don't wait on the child
}

pub fn fusermount_command(mount_point: &std::path::Path) -> Command {
    // fuse3 systems use fusermount3; fuse2 systems use fusermount — try 3 first
    let binary = if std::process::Command::new("fusermount3")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
    {
        "fusermount3"
    } else {
        "fusermount"
    };
    let mut cmd = Command::new(binary);
    cmd.arg("-u").arg(mount_point);
    cmd
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyMode {
    /// No special flags — copy everything, overwrite if src is different.
    Normal,
    /// `--update` — skip files where the destination is newer.
    Update,
    /// `--ignore-existing` — skip files that already exist on the destination.
    IgnoreExisting,
}
