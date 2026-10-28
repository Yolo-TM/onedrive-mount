//! Talking to the daemon, and coping when it is not there.
//!
//! The old GUI had one failure mode here that made it untrustworthy: any problem
//! reading the daemon's status collapsed to `None` and rendered as a permanent
//! "Waiting for daemon status…", indistinguishable from a daemon that had never
//! started. So the fallback path is explicit: if the socket does not answer we
//! rebuild the report from the state database and mark it
//! [`ReportSource::Database`], which the health roll-up always treats as broken.
//! You get the last known state *and* an unambiguous statement that it is stale.

use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use onedrive_rc_core::config::Config;
use onedrive_rc_core::ipc::{
    self, ClientError, IndexStatus, LogStatus, MountState, NetworkState, OutboxStatus, PinStatus,
    RemoteStatus, ReportSource, Request, Response, StatusReport,
};
use onedrive_rc_core::paths::Paths;
use onedrive_rc_core::state::{PinState, Store};
use onedrive_rc_core::{VERSION, log as corelog};

const TIMEOUT: Duration = Duration::from_secs(5);

/// Send a request, returning `None` if the daemon is not running.
pub fn try_request(paths: &Paths, req: &Request) -> Result<Option<Response>> {
    match ipc::request(&paths.socket(), req, TIMEOUT) {
        Ok(resp) => Ok(Some(resp)),
        Err(ClientError::NotRunning(_)) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Send a request that requires a live daemon.
pub fn request(paths: &Paths, req: &Request) -> Result<Response> {
    try_request(paths, req)?.with_context(|| {
        format!(
            "onedrive-rcd is not running (no socket at {})\nstart it with: onedrive-rc service restart",
            paths.socket().display()
        )
    })
}

/// Fetch a status report, falling back to the state database.
pub fn status(paths: &Paths) -> Result<StatusReport> {
    match try_request(paths, &Request::Status)? {
        Some(Response::Status(report)) => Ok(*report),
        Some(Response::Err { code, msg, .. }) => {
            anyhow::bail!("daemon could not report status [{code}]: {msg}")
        }
        Some(other) => anyhow::bail!("unexpected reply from onedrive-rcd: {other:?}"),
        None => status_from_database(paths),
    }
}

/// Reconstruct what we can from the database when the daemon is silent.
fn status_from_database(paths: &Paths) -> Result<StatusReport> {
    let db = paths.state_db();
    if !db.exists() {
        anyhow::bail!(
            "onedrive-rcd is not running and has never run (no state at {})\n\
             set it up with: onedrive-rc setup",
            db.display()
        );
    }

    let store = Store::open_readonly(&db)?;
    let daemon = store.daemon()?.ok_or_else(|| {
        anyhow::anyhow!(
            "onedrive-rcd is not running and has no recorded state\n\
             set it up with: onedrive-rc setup"
        )
    })?;

    let now = Utc::now();
    let config = Config::load(&paths.config_file()).unwrap_or_default();

    let log_dir = paths.log_dir();
    Ok(StatusReport {
        protocol: ipc::PROTOCOL_VERSION,
        source: ReportSource::Database,
        version: daemon.version.clone(),
        pid: daemon.pid,
        started_at: daemon.started_at,
        heartbeat_at: daemon.heartbeat_at,
        heartbeat_age_secs: (now - daemon.heartbeat_at).num_seconds(),
        daemon_state: daemon.state.as_str().to_string(),
        config_error: daemon.config_error,
        // We cannot know this without the daemon, and guessing would be worse.
        network: NetworkState::Unknown,
        remotes: config
            .remotes
            .iter()
            .map(|r| RemoteStatus {
                name: r.name.clone(),
                mount_point: r.mount_point(paths).to_string_lossy().into_owned(),
                state: MountState::Unmounted,
                since: None,
                error: None,
                overlay: r.overlay.enabled,
            })
            .collect(),
        pins: store
            .pins()?
            .into_iter()
            .map(|p| PinStatus {
                name: p.name,
                remote: p.remote,
                local_path: p.local_path,
                // The daemon is not running, so "in-sync" or "syncing" is only
                // what it last wrote down, not something that is true now.
                // Bad news stays as recorded; good news is not vouched for.
                state: match p.state {
                    PinState::InSync | PinState::Syncing => PinState::Unknown,
                    other => other,
                }
                .as_str()
                .to_string(),
                last_ok: p.last_ok,
                last_err: p.last_err,
                last_err_code: p.last_err_code,
            })
            .collect(),
        index: {
            let stats = store.index_stats()?;
            IndexStatus {
                files: stats.files,
                dirs: stats.dirs,
                last_crawl: stats.last_crawl,
                stale: false,
            }
        },
        conflicts: store.open_conflicts()?.len() as u64,
        outbox: OutboxStatus {
            pending: store.outbox_len()?,
            oldest: store.outbox_oldest()?,
            stale: false,
        },
        log: LogStatus {
            dir: log_dir.to_string_lossy().into_owned(),
            bytes: corelog::dir_size(&log_dir).unwrap_or(0),
        },
    })
}

/// Whether a daemon answers, and what version it reports.
pub fn ping(paths: &Paths) -> Result<Option<(String, u32)>> {
    match try_request(paths, &Request::Ping)? {
        Some(Response::Pong { version, protocol }) => Ok(Some((version, protocol))),
        Some(other) => anyhow::bail!("unexpected reply to ping: {other:?}"),
        None => Ok(None),
    }
}

/// The version this CLI was built from, for mismatch reporting.
pub const fn cli_version() -> &'static str {
    VERSION
}

#[cfg(test)]
mod tests {
    use super::*;
    use onedrive_rc_core::ipc::Health;

    #[test]
    fn a_never_run_daemon_is_reported_as_such_with_a_next_step() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path());

        let err = status(&paths).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("has never run"), "{msg}");
        assert!(msg.contains("onedrive-rc setup"), "{msg}");
    }

    /// The important fallback: a daemon that died leaves a readable database, and
    /// we must present its last state while being explicit that it is not live.
    #[test]
    fn falls_back_to_the_database_and_marks_it_not_live() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path());

        {
            let store = Store::open(&paths.state_db()).unwrap();
            store
                .daemon_started(4242, env!("CARGO_PKG_VERSION"), Utc::now())
                .unwrap();
            store
                .register_pin("Keepass", "od", "Dokumente/Keepass", "/home/t/k")
                .unwrap();
        }

        let report = status(&paths).unwrap();
        assert_eq!(report.source, ReportSource::Database);
        assert_eq!(report.pid, 4242);
        assert_eq!(report.pins.len(), 1);
        assert_eq!(report.network, NetworkState::Unknown);

        let (health, why) = report.health();
        assert_eq!(
            health,
            Health::Broken,
            "a database-sourced report is never healthy"
        );
        assert!(why.iter().any(|w| w.contains("not answering")), "{why:?}");
    }

    /// A dead daemon cannot vouch for a pin it last saw "in-sync".
    #[test]
    fn the_fallback_does_not_present_a_stale_in_sync_pin_as_current() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path());
        {
            let store = Store::open(&paths.state_db()).unwrap();
            store
                .daemon_started(1, env!("CARGO_PKG_VERSION"), Utc::now())
                .unwrap();
            store.register_pin("Ok", "od", "a", "/h/a").unwrap();
            store.register_pin("Bad", "od", "b", "/h/b").unwrap();
            store
                .set_pin_state("Ok", PinState::InSync, Utc::now(), None)
                .unwrap();
            store
                .set_pin_state("Bad", PinState::Conflicted, Utc::now(), None)
                .unwrap();
        }

        let report = status(&paths).unwrap();
        let state = |n: &str| {
            report
                .pins
                .iter()
                .find(|p| p.name == n)
                .unwrap()
                .state
                .clone()
        };
        assert_eq!(state("Ok"), "unknown");
        assert!(
            report
                .pins
                .iter()
                .find(|p| p.name == "Ok")
                .unwrap()
                .last_ok
                .is_some()
        );
        assert_eq!(state("Bad"), "conflicted", "bad news stays as recorded");
    }

    #[test]
    fn the_fallback_report_includes_configured_remotes() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path());
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        std::fs::write(
            paths.config_file(),
            "[[remote]]\nname = \"od\"\nmount_point = \"~/OneDrive\"\n",
        )
        .unwrap();
        {
            let store = Store::open(&paths.state_db()).unwrap();
            store
                .daemon_started(1, env!("CARGO_PKG_VERSION"), Utc::now())
                .unwrap();
        }

        let report = status(&paths).unwrap();
        assert_eq!(report.remotes.len(), 1);
        assert_eq!(report.remotes[0].name, "od");
    }

    /// An unparseable config must not stop the CLI from reporting; that would
    /// take away the tool you need to diagnose the config.
    #[test]
    fn a_broken_config_does_not_stop_the_fallback_report() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path());
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        std::fs::write(paths.config_file(), "this is not = valid = toml").unwrap();
        {
            let store = Store::open(&paths.state_db()).unwrap();
            store
                .daemon_started(1, env!("CARGO_PKG_VERSION"), Utc::now())
                .unwrap();
            store
                .set_config_error(Some("2 problems in the config"))
                .unwrap();
        }

        let report = status(&paths).unwrap();
        assert!(report.remotes.is_empty());
        assert_eq!(
            report.config_error.as_deref(),
            Some("2 problems in the config")
        );
    }

    #[test]
    fn ping_reports_no_daemon_rather_than_erroring() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path());
        assert_eq!(ping(&paths).unwrap(), None);
    }

    #[test]
    fn requests_that_need_a_daemon_explain_how_to_start_one() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path());

        let err = request(&paths, &Request::Reload).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("not running"), "{msg}");
        assert!(msg.contains("service restart"), "{msg}");
    }
}
