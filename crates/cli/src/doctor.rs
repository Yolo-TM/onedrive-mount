//! `onedrive-tui doctor` — check the things that actually break, before they do.
//!
//! Every check that can fail says what to do about it. This exists because the
//! old setup had no way to answer "why is it not working?" other than reading a
//! 1.4 GB log: `loginctl enable-linger` was documented only in the README and
//! never checked, the systemd unit silently referenced non-existent user units,
//! and the config validator covered five fields and never ran at startup.

use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;

use onedrive_core::config::Config;
use onedrive_core::paths::Paths;
use onedrive_core::state::Store;
use onedrive_core::{log as corelog, unit};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Verdict {
    Ok,
    Warn,
    Fail,
}

impl Verdict {
    const fn mark(self) -> &'static str {
        match self {
            Self::Ok => "ok  ",
            Self::Warn => "warn",
            Self::Fail => "FAIL",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Check {
    pub name: &'static str,
    pub verdict: Verdict,
    pub detail: String,
    pub fix: Option<String>,
}

impl Check {
    fn ok(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            verdict: Verdict::Ok,
            detail: detail.into(),
            fix: None,
        }
    }

    fn warn(name: &'static str, detail: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            name,
            verdict: Verdict::Warn,
            detail: detail.into(),
            fix: Some(fix.into()),
        }
    }

    fn fail(name: &'static str, detail: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            name,
            verdict: Verdict::Fail,
            detail: detail.into(),
            fix: Some(fix.into()),
        }
    }
}

/// Run every check.
pub fn run(paths: &Paths) -> Vec<Check> {
    vec![
        check_rclone(),
        check_fuse(),
        check_config(paths),
        check_state(paths),
        check_daemon(paths),
        check_unit(paths),
        check_linger(),
        check_logs(paths),
    ]
}

/// Worst verdict across all checks; drives the exit code.
pub fn worst(checks: &[Check]) -> Verdict {
    checks
        .iter()
        .map(|c| c.verdict)
        .max()
        .unwrap_or(Verdict::Ok)
}

pub fn render(checks: &[Check]) -> String {
    let mut out = String::new();
    for check in checks {
        let _ = writeln!(
            out,
            "[{}] {:<10} {}",
            check.verdict.mark(),
            check.name,
            check.detail
        );
        if let Some(fix) = &check.fix {
            let _ = writeln!(out, "            fix: {fix}");
        }
    }

    let worst = worst(checks);
    let _ = writeln!(
        out,
        "\n{}",
        match worst {
            Verdict::Ok => "all checks passed".to_string(),
            Verdict::Warn => format!(
                "{} warning(s)",
                checks.iter().filter(|c| c.verdict == Verdict::Warn).count()
            ),
            Verdict::Fail => format!(
                "{} failure(s), {} warning(s)",
                checks.iter().filter(|c| c.verdict == Verdict::Fail).count(),
                checks.iter().filter(|c| c.verdict == Verdict::Warn).count()
            ),
        }
    );
    out
}

// ---------------------------------------------------------------------------
// Individual checks
// ---------------------------------------------------------------------------

fn check_rclone() -> Check {
    match Command::new("rclone").arg("version").output() {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout);
            let version = text.lines().next().unwrap_or("rclone").trim().to_string();
            Check::ok("rclone", version)
        }
        Ok(out) => Check::fail(
            "rclone",
            format!("`rclone version` exited with {}", out.status),
            "reinstall rclone",
        ),
        Err(_) => Check::fail(
            "rclone",
            "not found in PATH",
            "install rclone (nixos: environment.systemPackages = [ pkgs.rclone ])",
        ),
    }
}

fn check_fuse() -> Check {
    // Either name is fine; fuse3 is what we prefer to unmount with.
    for binary in ["fusermount3", "fusermount"] {
        if which(binary).is_some() {
            return Check::ok("fuse", format!("{binary} available"));
        }
    }
    Check::fail(
        "fuse",
        "neither fusermount3 nor fusermount found in PATH",
        "install fuse3 (nixos: programs.fuse.userAllowOther = true; plus pkgs.fuse3)",
    )
}

fn check_config(paths: &Paths) -> Check {
    let path = paths.config_file();
    if !path.exists() {
        return Check::warn(
            "config",
            format!("no config at {}", path.display()),
            "onedrive-tui setup",
        );
    }

    match Config::load_validated(&path, paths) {
        Ok(config) => {
            let pins: usize = config.remotes.iter().map(|r| r.pins.len()).sum();
            Check::ok(
                "config",
                format!("{} remote(s), {pins} pin(s) — valid", config.remotes.len()),
            )
        }
        // The validator reports every problem at once, so this is the whole list.
        Err(e) => Check::fail(
            "config",
            e.msg.trim().to_string(),
            "edit the config and re-run",
        ),
    }
}

fn check_state(paths: &Paths) -> Check {
    let db = paths.state_db();
    if !db.exists() {
        return Check::warn(
            "state",
            "no state database yet (the daemon creates it on first run)",
            "onedrive-tui service restart",
        );
    }
    match Store::open_readonly(&db) {
        Ok(store) => match store.daemon() {
            Ok(Some(d)) => Check::ok(
                "state",
                format!("readable, last daemon pid {} ({})", d.pid, d.state.as_str()),
            ),
            Ok(None) => Check::ok("state", "readable, no daemon has run yet"),
            Err(e) => Check::fail("state", format!("unreadable: {e}"), "see: onedrive-tui log"),
        },
        Err(e) => Check::fail(
            "state",
            format!("cannot open {}: {e}", db.display()),
            "stop the daemon and move the file aside to start fresh",
        ),
    }
}

fn check_daemon(paths: &Paths) -> Check {
    match crate::client::ping(paths) {
        Ok(Some((version, protocol))) => {
            if protocol != onedrive_core::ipc::PROTOCOL_VERSION {
                return Check::fail(
                    "daemon",
                    format!(
                        "running {version} speaking protocol {protocol}, but this CLI speaks {}",
                        onedrive_core::ipc::PROTOCOL_VERSION
                    ),
                    "reinstall so both binaries come from the same build",
                );
            }
            if version != crate::client::cli_version() {
                return Check::warn(
                    "daemon",
                    format!(
                        "running {version}, but this CLI is {}",
                        crate::client::cli_version()
                    ),
                    "onedrive-tui service restart",
                );
            }
            Check::ok("daemon", format!("running {version}"))
        }
        Ok(None) => Check::fail(
            "daemon",
            format!("not answering on {}", paths.socket().display()),
            "onedrive-tui service restart",
        ),
        Err(e) => Check::fail("daemon", e.to_string(), "onedrive-tui service restart"),
    }
}

fn check_unit(paths: &Paths) -> Check {
    let path = crate::service::unit_path(paths);
    if !path.exists() {
        return Check::warn(
            "service",
            format!("unit not installed at {}", path.display()),
            "onedrive-tui service install",
        );
    }

    let enabled = systemctl(&["is-enabled", unit::UNIT_NAME]);
    let active = systemctl(&["is-active", unit::UNIT_NAME]);
    match (enabled.as_deref(), active.as_deref()) {
        (Some("enabled"), Some("active")) => Check::ok("service", "enabled and active"),
        (Some("enabled"), _) => Check::fail(
            "service",
            format!(
                "enabled but not active ({})",
                active.as_deref().unwrap_or("unknown")
            ),
            "onedrive-tui service restart",
        ),
        (_, Some("active")) => Check::warn(
            "service",
            "running but not enabled, so it will not start at login",
            "onedrive-tui service install",
        ),
        _ => Check::warn(
            "service",
            "installed but neither enabled nor active",
            "onedrive-tui service install",
        ),
    }
}

/// Without linger the daemon dies at logout, so background sync silently stops.
/// This was documented in the README and never checked.
fn check_linger() -> Check {
    let user = std::env::var("USER").unwrap_or_default();
    match Command::new("loginctl").args(["show-user", &user]).output() {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout);
            if text.lines().any(|l| l.trim() == "Linger=yes") {
                Check::ok("linger", "enabled — the daemon survives logout")
            } else {
                Check::warn(
                    "linger",
                    "disabled — the daemon stops when you log out",
                    format!("loginctl enable-linger {user}"),
                )
            }
        }
        // Not being under logind at all is unusual but not our problem to fix.
        _ => Check::warn(
            "linger",
            "could not query loginctl",
            "check that you are running under a systemd user session",
        ),
    }
}

fn check_logs(paths: &Paths) -> Check {
    let dir = paths.log_dir();
    let size = corelog::dir_size(&dir).unwrap_or(0);
    let budget = Config::load(&paths.config_file())
        .map(|c| c.daemon.log_retention_size.as_bytes())
        .unwrap_or(200 << 20);

    if size > budget * 2 {
        Check::warn(
            "logs",
            format!(
                "{} in {} — well over the {} budget",
                crate::render::bytes(size),
                dir.display(),
                crate::render::bytes(budget)
            ),
            "the daemon prunes hourly; if this persists, check: onedrive-tui log --level warn",
        )
    } else {
        Check::ok(
            "logs",
            format!("{} in {}", crate::render::bytes(size), dir.display()),
        )
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn which(binary: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(binary))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Run `systemctl --user ...`, returning its trimmed stdout.
///
/// `is-enabled` and `is-active` exit non-zero for perfectly ordinary states
/// ("disabled", "inactive"), so the output matters more than the status.
fn systemctl(args: &[&str]) -> Option<String> {
    let out = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worst_verdict_drives_the_outcome() {
        let ok = Check::ok("a", "fine");
        let warn = Check::warn("b", "hmm", "do this");
        let fail = Check::fail("c", "broken", "do that");

        assert_eq!(worst(std::slice::from_ref(&ok)), Verdict::Ok);
        assert_eq!(worst(&[ok.clone(), warn.clone()]), Verdict::Warn);
        assert_eq!(worst(&[ok.clone(), warn.clone(), fail]), Verdict::Fail);
        assert_eq!(worst(&[]), Verdict::Ok);
    }

    #[test]
    fn every_non_ok_check_offers_a_fix() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path());

        for check in run(&paths) {
            if check.verdict != Verdict::Ok {
                assert!(
                    check.fix.is_some(),
                    "check {:?} is {:?} but offers no fix",
                    check.name,
                    check.verdict
                );
            }
        }
    }

    #[test]
    fn rendering_includes_marks_names_and_fixes() {
        let checks = vec![
            Check::ok("rclone", "rclone v1.75.1"),
            Check::fail("daemon", "not answering", "onedrive-tui service restart"),
        ];
        let out = render(&checks);
        assert!(out.contains("[ok  ] rclone"), "{out}");
        assert!(out.contains("[FAIL] daemon"), "{out}");
        assert!(out.contains("fix: onedrive-tui service restart"), "{out}");
        assert!(out.contains("1 failure(s)"), "{out}");
    }

    #[test]
    fn a_clean_run_says_so() {
        let out = render(&[Check::ok("a", "fine")]);
        assert!(out.contains("all checks passed"), "{out}");
    }

    /// A fresh machine with no config and no daemon should report actionable
    /// problems, not crash.
    #[test]
    fn reports_sensibly_on_a_fresh_install() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path());
        let checks = run(&paths);

        let config = checks.iter().find(|c| c.name == "config").unwrap();
        assert_eq!(config.verdict, Verdict::Warn);
        assert!(
            config.fix.as_deref().unwrap().contains("setup"),
            "{config:?}"
        );

        let daemon = checks.iter().find(|c| c.name == "daemon").unwrap();
        assert_eq!(daemon.verdict, Verdict::Fail, "no daemon is a failure");

        let state = checks.iter().find(|c| c.name == "state").unwrap();
        assert_eq!(state.verdict, Verdict::Warn);
    }

    #[test]
    fn an_invalid_config_is_reported_with_every_problem() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path());
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        std::fs::write(
            paths.config_file(),
            "[[remote]]\nname = \"\"\nmount_point = \"\"\n",
        )
        .unwrap();

        let check = check_config(&paths);
        assert_eq!(check.verdict, Verdict::Fail);
        assert!(check.detail.contains("2 problems"), "{check:?}");
    }

    #[test]
    fn a_valid_config_is_summarised() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path());
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        std::fs::write(
            paths.config_file(),
            "[[remote]]\nname = \"od\"\nmount_point = \"~/OneDrive\"\n\
             [[remote.pin]]\nname = \"K\"\nremote_path = \"a\"\nlocal_path = \"~/k\"\n",
        )
        .unwrap();

        let check = check_config(&paths);
        assert_eq!(check.verdict, Verdict::Ok, "{check:?}");
        assert!(check.detail.contains("1 remote(s), 1 pin(s)"), "{check:?}");
    }

    #[test]
    fn an_oversized_log_directory_is_flagged() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path());
        std::fs::create_dir_all(paths.log_dir()).unwrap();
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        std::fs::write(
            paths.config_file(),
            "[daemon]\nlog_retention_size = \"1M\"\n",
        )
        .unwrap();
        std::fs::write(paths.log_dir().join("onedrived.jsonl"), vec![b'x'; 3 << 20]).unwrap();

        let check = check_logs(&paths);
        assert_eq!(check.verdict, Verdict::Warn, "{check:?}");
        assert!(check.detail.contains("over the 1.0 MB budget"), "{check:?}");
    }

    #[test]
    fn finds_executables_on_path_and_not_otherwise() {
        assert!(which("sh").is_some(), "sh should be on PATH");
        assert!(which("definitely-not-a-real-binary-xyz").is_none());
    }

    /// These run against the real machine, so they assert shape rather than
    /// outcome — but they must never panic.
    #[test]
    fn environment_checks_do_not_panic() {
        let rclone = check_rclone();
        assert_eq!(rclone.name, "rclone");
        let fuse = check_fuse();
        assert_eq!(fuse.name, "fuse");
        let linger = check_linger();
        assert_eq!(linger.name, "linger");
        assert!(linger.verdict != Verdict::Fail, "linger is never fatal");
    }
}
