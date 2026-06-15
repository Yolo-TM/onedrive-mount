// Installs and manages the systemd user service for the daemon

use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn unit_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("systemd/user/onedrive-mountd.service")
}

fn unit_content(binary_path: &std::path::Path, extra_path_dirs: &[std::path::PathBuf]) -> String {
    // Systemd user services start with a minimal PATH that does not include
    // the user's Nix profile or any NixOS environment.systemPackages entries.
    // We resolve rclone and fusermount3 at install time and bake their parent
    // directories into the unit so the daemon can exec them by bare name.
    let base_path = "/run/current-system/sw/bin:/usr/local/bin:/usr/bin:/bin";
    let path = if extra_path_dirs.is_empty() {
        base_path.to_string()
    } else {
        let extra: Vec<String> = extra_path_dirs
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        format!("{}:{}", extra.join(":"), base_path)
    };

    format!(
        "[Unit]\n\
         Description=OneDrive Mount Daemon\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         After=NetworkManager-wait-online.service\n\
         \n\
         [Service]\n\
         ExecStart={}\n\
         Environment=PATH={}\n\
         Restart=on-failure\n\
         RestartSec=10s\n\
         Type=exec\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        binary_path.display(),
        path,
    )
}

/// Resolves the daemon binary path from the current executable's directory,
/// verifies it actually exists and is executable before writing the unit file.
fn daemon_binary_path() -> Result<PathBuf, String> {
    let exe =
        std::env::current_exe().map_err(|e| format!("cannot locate current executable: {e}"))?;
    let dir = exe.parent().ok_or("executable has no parent directory")?;
    let daemon = dir.join("onedrive-mountd");

    // Canonicalize so symlinks are resolved and the path in the unit is absolute
    let daemon = daemon
        .canonicalize()
        .map_err(|_| format!("daemon binary not found at {}: run 'cargo build --bin onedrive-mountd --features daemon' first", daemon.display()))?;

    // Basic sanity check: must be a regular file
    let meta = fs::metadata(&daemon).map_err(|e| format!("cannot stat daemon binary: {e}"))?;
    if !meta.is_file() {
        return Err(format!("{} is not a regular file", daemon.display()));
    }

    Ok(daemon)
}

/// Resolves a binary by searching PATH plus NixOS well-known locations,
/// canonicalizing symlinks so we get the real Nix store bin dir rather than
/// a profile symlink that may not be visible inside the systemd unit's PATH.
fn resolve_bin_dir(name: &str) -> Option<std::path::PathBuf> {
    let path_var = std::env::var("PATH").unwrap_or_default();
    // Always append NixOS system and user profile dirs so this works when the
    // GUI is launched from a .desktop file with a minimal desktop-session PATH.
    let search = format!(
        "{}:/run/current-system/sw/bin:/nix/var/nix/profiles/default/bin",
        path_var
    );
    for dir in search.split(':') {
        let candidate = std::path::Path::new(dir).join(name);
        if let Ok(resolved) = candidate.canonicalize() {
            if resolved.is_file() {
                return resolved.parent().map(|p| p.to_path_buf());
            }
        }
    }
    None
}

pub fn install() -> Result<(), String> {
    let binary = daemon_binary_path()?;

    // Collect the Nix store bin dirs for rclone and fusermount so the unit's
    // PATH covers them regardless of what systemd injects at runtime.
    let mut extra_dirs: Vec<std::path::PathBuf> = Vec::new();
    for bin in &["rclone", "fusermount3", "fusermount"] {
        if let Some(dir) = resolve_bin_dir(bin) {
            if !extra_dirs.contains(&dir) {
                extra_dirs.push(dir);
            }
        }
    }

    let path = unit_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    fs::write(&path, unit_content(&binary, &extra_dirs)).map_err(|e| e.to_string())?;
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

/// Returns the last error line from the daemon's journal, if any.
/// Used to surface crash reasons (e.g. rclone not found) when the service is inactive.
pub fn last_exit_error() -> Option<String> {
    let output = Command::new("journalctl")
        .args([
            "--user",
            "-u",
            "onedrive-mountd.service",
            "-n",
            "20",
            "--no-pager",
            "-o",
            "cat",
        ])
        .output()
        .ok()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .rfind(|l| l.contains("error:") || l.contains("Error") || l.contains("not found"))
        .map(|l| l.trim().to_string())
}

fn systemctl(args: &[&str]) -> Result<(), String> {
    let mut full_args = vec!["--user"];
    full_args.extend_from_slice(args);

    let output = Command::new("systemctl")
        .args(&full_args)
        .output()
        .map_err(|e| e.to_string())?;

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if stderr.is_empty() {
            Err(format!(
                "systemctl {} failed (exit {})",
                args.join(" "),
                output.status
            ))
        } else {
            Err(format!("systemctl {} failed: {}", args.join(" "), stderr))
        }
    }
}
