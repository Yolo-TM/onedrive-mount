//! Human-readable output.
//!
//! One screen, fixed columns, and — crucially — every unhealthy line explains
//! itself. The old GUI showed coloured dots with no way to find out why, and
//! surfaced failures via a substring search over `journalctl` output.

use std::fmt::Write as _;

use chrono::{DateTime, Duration, Utc};
use onedrive_core::ipc::{
    ConflictInfo, Health, MountState, NetworkState, OutboxInfo, PinStatus, ReportSource,
    StatusReport,
};

/// Status dot. These are the only three states anything can be in.
fn dot(health: Health) -> &'static str {
    match health {
        Health::Healthy => "●",
        Health::Degraded => "◐",
        Health::Broken => "○",
    }
}

/// `9d 4h`, `4m`, `12s` — compact and good enough for an at-a-glance screen.
pub fn ago(then: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let d = now - then;
    if d < Duration::zero() {
        return "in the future".into();
    }
    let secs = d.num_seconds();
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => {
            let (h, m) = (s / 3600, (s % 3600) / 60);
            if m == 0 {
                format!("{h}h")
            } else {
                format!("{h}h {m}m")
            }
        }
        s => {
            let (days, h) = (s / 86_400, (s % 86_400) / 3600);
            if h == 0 {
                format!("{days}d")
            } else {
                format!("{days}d {h}h")
            }
        }
    }
}

/// `1.2 GB`, `12 MB`, `840 kB`.
pub fn bytes(n: u64) -> String {
    const UNITS: [(&str, u64); 4] = [("GB", 1 << 30), ("MB", 1 << 20), ("kB", 1 << 10), ("B", 1)];
    for (unit, size) in UNITS {
        if n >= size {
            let whole = n / size;
            if whole >= 10 || size == 1 {
                return format!("{whole} {unit}");
            }
            return format!("{:.1} {unit}", n as f64 / size as f64);
        }
    }
    "0 B".into()
}

/// Thousands separators, because a bare `412330` is hard to read at a glance.
pub fn count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// One pin, as a single fixed-column line — `pin ls` and the TUI's Pins pane
/// both want exactly this, so it lives in one place.
pub fn pin_line(pin: &PinStatus, now: DateTime<Utc>) -> String {
    format!(
        "{:<16} {:<12} {:<28} {}",
        pin.name,
        pin.state,
        pin.local_path,
        pin.last_ok
            .map(|t| format!("synced {} ago", ago(t, now)))
            .unwrap_or_else(|| "never synced".into())
    )
}

/// One conflict, as a single line — the TUI's Conflicts pane, where the full
/// `conflicts()` block would not fit one row per item.
pub fn conflict_line(c: &ConflictInfo, now: DateTime<Utc>) -> String {
    format!(
        "[{}] {}  ({} ago, {} cop{})",
        c.id,
        c.file,
        ago(c.detected_at, now),
        c.copies.len(),
        if c.copies.len() == 1 { "y" } else { "ies" }
    )
}

/// One queued write, as a single line — the TUI's Outbox pane equivalent of
/// [`conflict_line`].
pub fn outbox_line(row: &OutboxInfo, now: DateTime<Utc>) -> String {
    let status = if row.state == "failed" {
        format!(
            " (failed: {})",
            row.last_err.as_deref().unwrap_or("unknown error")
        )
    } else {
        String::new()
    };
    format!(
        "[{}] {}:{}  queued {} ago{status}",
        row.id,
        row.remote,
        row.path,
        ago(row.queued_at, now)
    )
}

/// The conflict list.
///
/// Written as a short recipe rather than a table, because the one thing someone
/// reading this needs is to know what to type next. The old program renamed a
/// password database aside and told nobody at all.
pub fn conflicts(list: &[ConflictInfo], now: DateTime<Utc>) -> String {
    if list.is_empty() {
        return "no conflicts\n".to_string();
    }

    let mut out = String::new();
    for conflict in list {
        let _ = writeln!(
            out,
            "[{}] {}  in {}  ({} ago)",
            conflict.id,
            conflict.file,
            shorten(&conflict.folder),
            ago(conflict.detected_at, now)
        );
        let _ = writeln!(out, "     {:<44} in use", conflict.file);
        for copy in &conflict.copies {
            let side = if copy.contains(".conflict-local-") {
                "edited here"
            } else {
                "from OneDrive"
            };
            let _ = writeln!(out, "     {copy:<44} {side}");
        }
        if conflict.copies.is_empty() {
            let _ = writeln!(
                out,
                "     (no extra copies on disk — it may already be merged)"
            );
        }
        let _ = writeln!(out);
    }

    let _ = writeln!(
        out,
        "Merge them yourself, delete the extra copy, and this clears on its own.\n\
         Or decide now:\n  \
         onedrive-tui conflicts resolve <id> --keep local|remote|both\n\
         `both` keeps every file and only stops the alert; `local`/`remote` \
         discard the other\nversion into .versions/ and sync the result."
    );
    out
}

/// The `onedrive-tui outbox` screen.
pub fn outbox(rows: &[OutboxInfo], now: DateTime<Utc>) -> String {
    if rows.is_empty() {
        return "outbox empty\n".to_string();
    }

    let mut out = String::new();
    for row in rows {
        let _ = writeln!(
            out,
            "[{}] {}:{}  queued {} ago",
            row.id,
            row.remote,
            row.path,
            ago(row.queued_at, now)
        );
        if row.state == "failed" {
            let _ = writeln!(
                out,
                "     failed, attempt {}: {}",
                row.attempts,
                row.last_err.as_deref().unwrap_or("unknown error")
            );
        }
        let _ = writeln!(out, "     staged at {}", row.staged_path);
    }
    let _ = writeln!(
        out,
        "\nIt replays automatically on reconnect, or now:\n  \
         onedrive-tui outbox retry [<remote>]\n\
         To drop one without uploading it:\n  \
         onedrive-tui outbox discard <id>"
    );
    out
}

/// The `onedrive-tui status` screen.
pub fn status(report: &StatusReport, now: DateTime<Utc>) -> String {
    let mut out = String::new();
    let (health, why) = report.health();

    if report.source == ReportSource::Database {
        let _ = writeln!(
            out,
            "⚠ onedrived is not answering. Showing its last recorded state from {} ago.\n",
            ago(report.heartbeat_at, now)
        );
    }

    let _ = writeln!(
        out,
        "{:<7} {}",
        "net",
        match report.network {
            NetworkState::Online => "online",
            NetworkState::Offline => "offline",
            NetworkState::Unknown => "unknown",
        }
    );

    for remote in &report.remotes {
        let (mark, note) = match remote.state {
            MountState::Mounted => (
                dot(Health::Healthy),
                remote
                    .since
                    .map(|s| format!("up {}", ago(s, now)))
                    .unwrap_or_else(|| "mounted".into()),
            ),
            MountState::Mounting => (dot(Health::Degraded), "mounting…".into()),
            MountState::Unmounted => (dot(Health::Degraded), "not mounted".into()),
            MountState::Failed => (
                dot(Health::Broken),
                remote.error.clone().unwrap_or_else(|| "failed".into()),
            ),
        };
        let _ = writeln!(
            out,
            "{:<7} {:<22} {mark} {}",
            "mount",
            shorten(&remote.mount_point),
            note
        );
    }

    let index_note = match report.index.last_crawl {
        Some(last) => format!("crawled {} ago", ago(last, now)),
        None => "never crawled".into(),
    };
    let _ = writeln!(
        out,
        "{:<7} {:<22} {} {index_note}",
        "index",
        format!("{} files", count(report.index.files)),
        dot(if report.index.stale {
            Health::Degraded
        } else {
            Health::Healthy
        }),
    );

    for pin in &report.pins {
        let pin_health = match pin.state.as_str() {
            "in-sync" => Health::Healthy,
            "failed" | "conflicted" => Health::Degraded,
            _ => Health::Degraded,
        };
        let note = match pin.last_ok {
            Some(ok) => format!("{:<12} {} ago", pin.state, ago(ok, now)),
            None => format!("{:<12} never synced", pin.state),
        };
        let _ = writeln!(
            out,
            "{:<7} {:<22} {} {note}",
            "pin",
            pin.name,
            dot(pin_health)
        );
    }

    let _ = writeln!(
        out,
        "{:<7} {}",
        "outbox",
        if report.outbox.pending == 0 {
            "empty".to_string()
        } else {
            format!(
                "{} pending, oldest {} ago",
                report.outbox.pending,
                report
                    .outbox
                    .oldest
                    .map(|o| ago(o, now))
                    .unwrap_or_else(|| "?".into())
            )
        }
    );

    if report.conflicts > 0 {
        let _ = writeln!(
            out,
            "{:<7} {:<22} {} see: onedrive-tui conflicts",
            "conflict",
            format!("{} unresolved", report.conflicts),
            dot(Health::Degraded),
        );
    }

    let _ = writeln!(
        out,
        "{:<7} {:<22} {}",
        "log",
        shorten(&report.log.dir),
        bytes(report.log.bytes)
    );

    if !why.is_empty() {
        let _ = writeln!(out, "\n{} {}", dot(health), health.as_str());
        for reason in why {
            // The config validator reports every problem in one multi-line
            // string; keep those lines inside the bullet rather than letting
            // them reset to the left margin.
            for (i, line) in reason.trim_end().lines().enumerate() {
                let _ = writeln!(out, "{}{line}", if i == 0 { "  - " } else { "    " });
            }
        }
    }

    out
}

/// Replace `$HOME` with `~` so paths fit the column.
fn shorten(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && path.starts_with(&home) => {
            format!("~{}", &path[home.len()..])
        }
        _ => path.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use onedrive_core::ipc::{
        IndexStatus, LogStatus, OutboxStatus, PinStatus, RemoteStatus, StatusReport,
    };

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 8, 12, 0, 0).unwrap()
    }

    fn conflict() -> ConflictInfo {
        ConflictInfo {
            id: 3,
            subject: "Keepass".into(),
            remote: "od".into(),
            file: "Timm.kdbx".into(),
            folder: "/home/t/onedrive-keepass".into(),
            copies: vec![
                "Timm.conflict-remote-2026-10-08T104213Z.kdbx".into(),
                "Timm.conflict-local-2026-10-08T114500Z.kdbx".into(),
            ],
            detected_at: now() - Duration::minutes(18),
        }
    }

    #[test]
    fn no_conflicts_says_exactly_that() {
        assert_eq!(conflicts(&[], now()), "no conflicts\n");
    }

    /// The listing has to name both the id to type and the command to type it
    /// into — this is the screen someone reaches after a notification.
    #[test]
    fn a_conflict_listing_is_actionable() {
        let out = conflicts(&[conflict()], now());

        assert!(out.contains("[3]"), "{out}");
        assert!(out.contains("Timm.kdbx"), "{out}");
        assert!(out.contains("18m ago"), "{out}");
        assert!(
            out.contains("Timm.conflict-remote-2026-10-08T104213Z.kdbx"),
            "{out}"
        );
        assert!(out.contains("conflicts resolve <id> --keep"), "{out}");

        // Which side each copy came from, in words rather than in a suffix.
        assert!(out.contains("from OneDrive"), "{out}");
        assert!(out.contains("edited here"), "{out}");

        // And the manual merge is offered first, because for a .kdbx that is
        // the right answer.
        let merge = out.find("Merge them yourself").expect("{out}");
        let decide = out.find("--keep").expect("{out}");
        assert!(merge < decide, "{out}");
    }

    #[test]
    fn a_conflict_with_no_copies_left_says_so_rather_than_printing_nothing() {
        let mut c = conflict();
        c.copies.clear();
        let out = conflicts(&[c], now());
        assert!(out.contains("already be merged"), "{out}");
    }

    fn outbox_row() -> OutboxInfo {
        OutboxInfo {
            id: 5,
            remote: "od".into(),
            path: "notes.txt".into(),
            staged_path: "/home/t/OneDrive.pending/notes.txt".into(),
            queued_at: now() - Duration::minutes(4),
            state: "queued".into(),
            attempts: 0,
            last_err: None,
        }
    }

    #[test]
    fn an_empty_outbox_says_exactly_that() {
        assert_eq!(outbox(&[], now()), "outbox empty\n");
    }

    #[test]
    fn a_queued_write_is_actionable() {
        let out = outbox(&[outbox_row()], now());
        assert!(out.contains("[5]"), "{out}");
        assert!(out.contains("od:notes.txt"), "{out}");
        assert!(out.contains("4m ago"), "{out}");
        assert!(out.contains("/home/t/OneDrive.pending/notes.txt"), "{out}");
        assert!(out.contains("outbox retry"), "{out}");
        assert!(out.contains("outbox discard <id>"), "{out}");
        assert!(!out.contains("failed"), "{out}");
    }

    #[test]
    fn a_failed_write_shows_its_error() {
        let mut row = outbox_row();
        row.state = "failed".into();
        row.attempts = 2;
        row.last_err = Some("connection refused".into());
        let out = outbox(&[row], now());
        assert!(
            out.contains("failed, attempt 2: connection refused"),
            "{out}"
        );
    }

    #[test]
    fn pin_line_shows_the_name_state_path_and_last_sync() {
        let pin = PinStatus {
            name: "Keepass".into(),
            remote: "od".into(),
            local_path: "/home/t/onedrive-keepass".into(),
            state: "in-sync".into(),
            last_ok: Some(now() - Duration::minutes(2)),
            last_err: None,
            last_err_code: None,
        };
        let line = pin_line(&pin, now());
        assert!(line.contains("Keepass"), "{line}");
        assert!(line.contains("in-sync"), "{line}");
        assert!(line.contains("/home/t/onedrive-keepass"), "{line}");
        assert!(line.contains("synced 2m ago"), "{line}");
    }

    #[test]
    fn pin_line_says_never_synced_rather_than_a_bogus_time() {
        let pin = PinStatus {
            name: "Keepass".into(),
            remote: "od".into(),
            local_path: "/home/t/onedrive-keepass".into(),
            state: "pending".into(),
            last_ok: None,
            last_err: None,
            last_err_code: None,
        };
        assert!(pin_line(&pin, now()).contains("never synced"));
    }

    #[test]
    fn conflict_line_fits_on_one_row() {
        let line = conflict_line(&conflict(), now());
        assert!(line.contains("[3]"), "{line}");
        assert!(line.contains("Timm.kdbx"), "{line}");
        assert!(line.contains("18m ago"), "{line}");
        assert!(line.contains("2 copies"), "{line}");
        assert!(!line.contains('\n'), "{line}");
    }

    #[test]
    fn outbox_line_fits_on_one_row_and_shows_failures() {
        let line = outbox_line(&outbox_row(), now());
        assert!(line.contains("[5]"), "{line}");
        assert!(line.contains("od:notes.txt"), "{line}");
        assert!(!line.contains("failed"), "{line}");

        let mut failed = outbox_row();
        failed.state = "failed".into();
        failed.last_err = Some("connection refused".into());
        let line = outbox_line(&failed, now());
        assert!(line.contains("failed: connection refused"), "{line}");
    }

    fn report() -> StatusReport {
        StatusReport {
            protocol: onedrive_core::ipc::PROTOCOL_VERSION,
            source: ReportSource::Daemon,
            version: "0.3.0".into(),
            pid: 1234,
            started_at: now() - Duration::days(9) - Duration::hours(4),
            heartbeat_at: now() - Duration::seconds(3),
            heartbeat_age_secs: 3,
            daemon_state: "online".into(),
            config_error: None,
            network: NetworkState::Online,
            remotes: vec![RemoteStatus {
                name: "od".into(),
                mount_point: "/home/t/OneDrive".into(),
                state: MountState::Mounted,
                since: Some(now() - Duration::days(9) - Duration::hours(4)),
                error: None,
                overlay: true,
            }],
            pins: vec![PinStatus {
                name: "Keepass".into(),
                remote: "od".into(),
                local_path: "/home/t/onedrive-keepass".into(),
                state: "in-sync".into(),
                last_ok: Some(now() - Duration::minutes(2)),
                last_err: None,
                last_err_code: None,
            }],
            index: IndexStatus {
                files: 412_330,
                dirs: 9_001,
                last_crawl: Some(now() - Duration::minutes(4)),
                stale: false,
            },
            conflicts: 0,
            outbox: OutboxStatus::default(),
            log: LogStatus {
                dir: "/home/t/.local/state/onedrive/log".into(),
                bytes: 12 << 20,
            },
        }
    }

    #[test]
    fn renders_the_documented_screen() {
        let out = status(&report(), now());
        assert!(out.contains("net     online"), "{out}");
        assert!(out.contains("mount") && out.contains("● up 9d 4h"), "{out}");
        assert!(out.contains("412,330 files"), "{out}");
        assert!(out.contains("crawled 4m ago"), "{out}");
        assert!(out.contains("pin") && out.contains("Keepass"), "{out}");
        assert!(out.contains("2m ago"), "{out}");
        assert!(out.contains("outbox  empty"), "{out}");
        assert!(out.contains("12 MB"), "{out}");
        // Healthy means no explanation block.
        assert!(!out.contains("healthy"), "{out}");
    }

    #[test]
    fn explains_itself_when_unhealthy() {
        let mut r = report();
        r.conflicts = 1;
        r.pins[0].state = "conflicted".into();

        let out = status(&r, now());
        assert!(out.contains("degraded"), "{out}");
        assert!(out.contains("1 unresolved conflict"), "{out}");
        assert!(out.contains("onedrive-tui conflicts"), "{out}");
        assert!(out.contains("pin Keepass is conflicted"), "{out}");
    }

    /// A stale report must be labelled as such on the very first line.
    #[test]
    fn a_database_report_is_labelled_loudly() {
        let mut r = report();
        r.source = ReportSource::Database;
        r.heartbeat_at = now() - Duration::minutes(30);
        r.heartbeat_age_secs = 1800;

        let out = status(&r, now());
        assert!(out.starts_with("⚠ onedrived is not answering"), "{out}");
        assert!(out.contains("30m ago"), "{out}");
        assert!(out.contains("broken"), "{out}");
    }

    #[test]
    fn a_never_synced_pin_says_so_rather_than_showing_a_bogus_time() {
        let mut r = report();
        r.pins[0].last_ok = None;
        r.pins[0].state = "unknown".into();
        let out = status(&r, now());
        assert!(out.contains("never synced"), "{out}");
    }

    #[test]
    fn a_failed_mount_shows_its_error() {
        let mut r = report();
        r.remotes[0].state = MountState::Failed;
        r.remotes[0].error = Some("rclone exited with status 1".into());
        let out = status(&r, now());
        assert!(out.contains("rclone exited with status 1"), "{out}");
    }

    #[test]
    fn a_pending_outbox_is_summarised() {
        let mut r = report();
        r.outbox = OutboxStatus {
            pending: 3,
            oldest: Some(now() - Duration::hours(2)),
            stale: false,
        };
        let out = status(&r, now());
        assert!(out.contains("3 pending, oldest 2h ago"), "{out}");
    }

    #[test]
    fn formats_durations_at_every_scale() {
        assert_eq!(ago(now(), now()), "0s");
        assert_eq!(ago(now() - Duration::seconds(45), now()), "45s");
        assert_eq!(ago(now() - Duration::minutes(4), now()), "4m");
        assert_eq!(ago(now() - Duration::hours(3), now()), "3h");
        assert_eq!(ago(now() - Duration::minutes(150), now()), "2h 30m");
        assert_eq!(
            ago(now() - Duration::days(9) - Duration::hours(4), now()),
            "9d 4h"
        );
        assert_eq!(ago(now() - Duration::days(9), now()), "9d");
        // Clock skew should read as odd, not as a huge negative number.
        assert_eq!(ago(now() + Duration::hours(1), now()), "in the future");
    }

    #[test]
    fn formats_bytes_readably() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(12 << 20), "12 MB");
        assert_eq!(bytes(1536 << 20), "1.5 GB");
        // The size the old unrotated rclone log actually reached.
        assert_eq!(bytes(1_473_305_477), "1.4 GB");
        assert_eq!(bytes(40 << 30), "40 GB");
    }

    #[test]
    fn formats_counts_with_separators() {
        assert_eq!(count(0), "0");
        assert_eq!(count(42), "42");
        assert_eq!(count(1_000), "1,000");
        assert_eq!(count(412_330), "412,330");
        assert_eq!(count(1_234_567), "1,234,567");
    }
}
