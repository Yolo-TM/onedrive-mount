//! Installing and controlling the systemd user service.
//!
//! The old GUI wrote the unit from a `small_button` in the bottom-right corner
//! of a status bar, synchronously on the UI thread, with `systemctl enable --now`
//! blocking the window until the daemon had started. It also never dealt with
//! `loginctl enable-linger`, which the README told you to run yourself — so the
//! common outcome was a daemon that stopped at logout.
//!
//! Here it is one command that finishes the job, including linger.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use onedrive_rc_core::paths::Paths;
use onedrive_rc_core::unit::{self, UNIT_NAME};

/// `~/.config/systemd/user/onedrive-rcd.service`
///
/// Derived from the same home as everything else so a test `Paths` stays
/// self-contained rather than writing into the real user's systemd directory.
pub fn unit_path(paths: &Paths) -> PathBuf {
    paths.home().join(".config/systemd/user").join(UNIT_NAME)
}

/// Locate the daemon binary: next to this executable, else on `PATH`.
pub fn daemon_binary() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("finding our own path")?;
    if let Some(dir) = exe.parent() {
        let sibling = dir.join("onedrive-rcd");
        if sibling.is_file() {
            return sibling.canonicalize().context("resolving the daemon path");
        }
    }

    if let Some(found) = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|d| d.join("onedrive-rcd"))
        .find(|p| p.is_file())
    {
        return found.canonicalize().context("resolving the daemon path");
    }

    bail!(
        "cannot find the `onedrive-rcd` binary next to {} or on PATH",
        exe.display()
    )
}

pub fn install(paths: &Paths) -> Result<String> {
    let binary = daemon_binary()?;
    let path = unit_path(paths);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    let contents = unit::render(&binary);
    write_atomic(&path, &contents).with_context(|| format!("writing {}", path.display()))?;

    let mut log = format!("wrote {}\n", path.display());

    systemctl(&["daemon-reload"])?;
    log.push_str("reloaded the systemd user manager\n");

    systemctl(&["enable", "--now", UNIT_NAME])?;
    log.push_str(&format!("enabled and started {UNIT_NAME}\n"));

    // Without this the daemon dies at logout and background sync stops.
    match enable_linger() {
        Ok(true) => log.push_str("enabled linger (the daemon survives logout)\n"),
        Ok(false) => log.push_str("linger was already enabled\n"),
        Err(e) => log.push_str(&format!(
            "could not enable linger: {e}\n  run it yourself: loginctl enable-linger {}\n",
            std::env::var("USER").unwrap_or_else(|_| "$USER".into())
        )),
    }

    Ok(log)
}

pub fn uninstall(paths: &Paths) -> Result<String> {
    let mut log = String::new();

    // Ignore failures here: the point is to end up stopped and gone, and the
    // unit may legitimately not be loaded.
    if systemctl(&["disable", "--now", UNIT_NAME]).is_ok() {
        log.push_str(&format!("disabled and stopped {UNIT_NAME}\n"));
    }

    let path = unit_path(paths);
    match std::fs::remove_file(&path) {
        Ok(()) => log.push_str(&format!("removed {}\n", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            log.push_str(&format!("{} was not installed\n", path.display()));
        }
        Err(e) => return Err(e).with_context(|| format!("removing {}", path.display())),
    }

    let _ = systemctl(&["daemon-reload"]);
    log.push_str("reloaded the systemd user manager\n");
    log.push_str("note: linger is left enabled; disable it with `loginctl disable-linger`\n");
    Ok(log)
}

pub fn restart(paths: &Paths) -> Result<String> {
    if !unit_path(paths).exists() {
        bail!("{UNIT_NAME} is not installed\ninstall it with: onedrive-rc service install");
    }
    systemctl(&["restart", UNIT_NAME])?;
    Ok(format!("restarted {UNIT_NAME}\n"))
}

/// `systemctl --user status`, passed through for convenience.
pub fn status() -> Result<String> {
    let out = Command::new("systemctl")
        .args(["--user", "status", UNIT_NAME, "--no-pager"])
        .output()
        .context("running systemctl")?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Returns whether it changed anything.
fn enable_linger() -> Result<bool> {
    let user = std::env::var("USER").context("USER is not set")?;

    let already = Command::new("loginctl")
        .args(["show-user", &user])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("Linger=yes"))
        .unwrap_or(false);
    if already {
        return Ok(false);
    }

    let out = Command::new("loginctl")
        .args(["enable-linger", &user])
        .output()
        .context("running loginctl enable-linger")?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(true)
}

fn systemctl(args: &[&str]) -> Result<()> {
    let out = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .with_context(|| format!("running systemctl --user {}", args.join(" ")))?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        bail!(
            "systemctl --user {} failed: {}",
            args.join(" "),
            if stderr.is_empty() {
                out.status.to_string()
            } else {
                stderr
            }
        );
    }
    Ok(())
}

/// Write via a temp file and rename, so an interrupted install cannot leave a
/// half-written unit that systemd would then refuse to parse.
fn write_atomic(path: &Path, contents: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("service.tmp");
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_path_is_under_the_given_home() {
        let paths = Paths::rooted("/home/t");
        assert_eq!(
            unit_path(&paths),
            Path::new("/home/t/.config/systemd/user/onedrive-rcd.service")
        );
    }

    #[test]
    fn restart_without_an_installed_unit_explains_how_to_install() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path());

        let err = restart(&paths).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("not installed"), "{msg}");
        assert!(msg.contains("service install"), "{msg}");
    }

    #[test]
    fn uninstalling_what_was_never_installed_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path());

        // systemctl calls are tolerated to fail here; the file handling is what
        // must behave.
        let log = uninstall(&paths).expect("uninstall should be idempotent");
        assert!(log.contains("was not installed"), "{log}");
    }

    #[test]
    fn writes_atomically_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("onedrive-rcd.service");

        write_atomic(&path, "[Unit]\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[Unit]\n");
        assert!(!path.with_extension("service.tmp").exists());
    }

    /// The unit we would install must name a real binary and carry the
    /// watchdog/restart settings the plan calls for.
    #[test]
    fn the_rendered_unit_points_at_the_daemon_and_is_supervised() {
        let rendered = unit::render(Path::new("/opt/bin/onedrive-rcd"));
        assert!(rendered.contains("ExecStart=/opt/bin/onedrive-rcd"));
        assert!(rendered.contains("Type=notify"));
        assert!(rendered.contains("WatchdogSec=60"));
        assert!(rendered.contains("Restart=always"));
    }

    #[test]
    fn daemon_binary_lookup_either_finds_it_or_says_where_it_looked() {
        match daemon_binary() {
            Ok(path) => assert!(path.is_file(), "{}", path.display()),
            Err(e) => {
                let msg = format!("{e:#}");
                assert!(msg.contains("onedrive-rcd"), "{msg}");
                assert!(msg.contains("PATH"), "{msg}");
            }
        }
    }
}
