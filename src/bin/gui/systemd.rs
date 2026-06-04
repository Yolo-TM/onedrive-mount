// Installs and manages the systemd user service for the daemon

use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn unit_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("systemd/user/onedrive-mountd.service")
}

fn unit_content(binary_path: &std::path::Path) -> String {
    format!(
        "[Unit]\n\
         Description=OneDrive rclone Daemon\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         ExecStart={}\n\
         Restart=on-failure\n\
         RestartSec=10s\n\
         Type=exec\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        binary_path.display()
    )
}

/// Resolves the daemon binary path from the current executable's directory,
/// verifies it actually exists and is executable before writing the unit file.
fn daemon_binary_path() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate current executable: {e}"))?;
    let dir = exe.parent().ok_or("executable has no parent directory")?;
    let daemon = dir.join("onedrive-mountd");

    // Canonicalize so symlinks are resolved and the path in the unit is absolute
    let daemon = daemon
        .canonicalize()
        .map_err(|_| format!("daemon binary not found at {}: run 'cargo build --bin onedrive-mountd --features daemon' first", daemon.display()))?;

    // Basic sanity check: must be a regular file
    let meta = fs::metadata(&daemon)
        .map_err(|e| format!("cannot stat daemon binary: {e}"))?;
    if !meta.is_file() {
        return Err(format!("{} is not a regular file", daemon.display()));
    }

    Ok(daemon)
}

pub fn install() -> Result<(), String> {
    let binary = daemon_binary_path()?;

    let path = unit_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    fs::write(&path, unit_content(&binary)).map_err(|e| e.to_string())?;
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", "--now", "onedrive-mountd.service"])
}

pub fn uninstall() -> Result<(), String> {
    systemctl(&["disable", "--now", "onedrive-mountd.service"])?;
    let path = unit_path();
    if path.exists() {
        fs::remove_file(&path).map_err(|e| e.to_string())?;
    }
    systemctl(&["daemon-reload"])
}

pub fn start() -> Result<(), String> {
    systemctl(&["start", "onedrive-mountd.service"])
}

pub fn is_active() -> bool {
    Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", "onedrive-mountd.service"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn is_enabled() -> bool {
    Command::new("systemctl")
        .args(["--user", "is-enabled", "--quiet", "onedrive-mountd.service"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn systemctl(args: &[&str]) -> Result<(), String> {
    let mut full_args = vec!["--user"];
    full_args.extend_from_slice(args);

    let status = Command::new("systemctl")
        .args(&full_args)
        .status()
        .map_err(|e| e.to_string())?;

    if status.success() {
        Ok(())
    } else {
        Err(format!("systemctl {} failed", args.join(" ")))
    }
}
