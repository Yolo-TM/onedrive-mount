//! Shared daemon state and the supervisor loop.
//!
//! Two design rules here are direct responses to how the old daemon failed:
//!
//! 1. **Nothing blocking runs in the main loop.** The old `app.rs` awaited
//!    `tokio::fs::metadata()` on a FUSE mount point from inside its `select!`,
//!    so a hung mount froze config reload, signal handling, status writes and
//!    sync scheduling all at once. Here the loop only moves timers and
//!    channels; work that can block gets its own task and its own timeout.
//! 2. **An invalid config does not exit.** The old daemon returned `Err` from
//!    `main`, which under `Restart=on-failure` became a restart every 10s
//!    forever. We stay up, record the error where `status` and `doctor` can
//!    explain it, and wait for the file to be fixed — the watcher will pick it up.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::Result;
use chrono::Utc;
use onedrive_core::config::Config;
use onedrive_core::ipc::{
    self, IndexStatus, LogStatus, MountState, NetworkState, OutboxStatus, PinStatus, RemoteStatus,
    ReportSource, Request, Response, StatusReport,
};
use onedrive_core::paths::Paths;
use onedrive_core::state::{DaemonState, PinState, Store};
use onedrive_core::{Code, VERSION, log as corelog};
use tokio::sync::{mpsc, watch};
use tracing::{debug, error, info, warn};

use crate::mount::MountStatus;
use crate::sdnotify::Notifier;

/// How often the heartbeat column advances. `status` calls a daemon dead after
/// 60s of silence, so this leaves ample margin.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// Log retention runs on this cadence. Hourly is frequent enough that a runaway
/// log cannot reach the size the old one did, and cheap enough to ignore.
const PRUNE_INTERVAL: Duration = Duration::from_secs(3600);

/// Cap on rows kept in the `event` table for the TUI.
const EVENT_CAP: u32 = 2000;

pub struct App {
    pub paths: Paths,
    /// `rusqlite::Connection` is `Send` but not `Sync`. Every use is a short
    /// synchronous burst, and we never hold this across an await.
    store: Mutex<Store>,
    config: RwLock<Config>,
    config_error: RwLock<Option<String>>,
    network: RwLock<NetworkState>,
    /// Mount state, published by the supervisor task and read by status.
    ///
    /// A snapshot rather than the `Supervisor` itself: the supervisor owns
    /// process handles and must stay on one task, while status is answered from
    /// whichever task a client happens to land on.
    mounts: RwLock<HashMap<String, MountStatus>>,
    notifier: Notifier,
    reload_tx: mpsc::Sender<()>,
    /// Requests from the CLI that the supervisor task has to action.
    control_tx: mpsc::Sender<Control>,
}

/// Work that only the task owning the [`crate::mount::Supervisor`] can do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Control {
    /// Remount one remote, or all of them.
    Remount { remote: Option<String> },
    /// Sync one pin, or every enabled pin.
    SyncPins { pin: Option<String> },
    /// The pin set changed: rebuild the inotify watches and drop scheduler
    /// entries for pins that no longer exist.
    ReloadPins,
    /// Crawl every enabled remote and refresh the offline index.
    IndexRefresh,
    /// Replay the outbox now, for one remote or every enabled one.
    OutboxRetry { remote: Option<String> },
}

impl App {
    pub fn new(
        paths: Paths,
        store: Store,
        config: Config,
        config_error: Option<String>,
        notifier: Notifier,
        reload_tx: mpsc::Sender<()>,
        control_tx: mpsc::Sender<Control>,
    ) -> Self {
        Self {
            paths,
            store: Mutex::new(store),
            config: RwLock::new(config),
            config_error: RwLock::new(config_error),
            network: RwLock::new(NetworkState::Unknown),
            mounts: RwLock::new(HashMap::new()),
            notifier,
            reload_tx,
            control_tx,
        }
    }

    /// Publish the supervisor's view so `status` can report it.
    pub fn set_mount_status(&self, remote: &str, status: MountStatus) {
        self.mounts
            .write()
            .expect("mount lock")
            .insert(remote.to_owned(), status);
    }

    /// Forget remotes that are no longer configured.
    pub fn retain_mount_status(&self, keep: &[String]) {
        self.mounts
            .write()
            .expect("mount lock")
            .retain(|name, _| keep.contains(name));
    }

    fn mount_status(&self, remote: &str) -> MountStatus {
        self.mounts
            .read()
            .expect("mount lock")
            .get(remote)
            .cloned()
            .unwrap_or(MountStatus {
                state: MountState::Unmounted,
                since: None,
                error: None,
                code: None,
            })
    }

    pub fn config(&self) -> Config {
        self.config.read().expect("config lock").clone()
    }

    pub fn notifier(&self) -> &Notifier {
        &self.notifier
    }

    /// Run a closure against the store. Keeps lock handling in one place and
    /// makes it obvious at the call sites that this is a short critical section.
    pub fn with_store<T>(&self, f: impl FnOnce(&Store) -> Result<T>) -> Result<T> {
        let store = self.store.lock().expect("store lock");
        f(&store)
    }

    /// Record an operational event in both the log and the state database. The
    /// database copy is what the TUI shows; the log is the durable record.
    pub fn event(&self, level: corelog::Level, code: Option<Code>, subject: &str, msg: &str) {
        let now = Utc::now();
        if let Err(e) = self.with_store(|s| s.log_event(now, level.as_str(), code, subject, msg)) {
            warn!(error = %e, "could not record event");
        }
    }

    // -- config ------------------------------------------------------------

    /// Re-read and re-validate the config, keeping the previous one on failure.
    ///
    /// Returns whether the config actually changed.
    pub fn reload_config(&self) -> bool {
        let path = self.paths.config_file();

        // A missing config is a legitimate pre-setup state, not an error.
        if !path.exists() {
            info!(path = %path.display(), "no config file yet; waiting for setup");
            self.set_config_error(None);
            let mut current = self.config.write().expect("config lock");
            let changed = *current != Config::default();
            *current = Config::default();
            return changed;
        }

        match Config::load_validated(&path, &self.paths) {
            Ok(new) => {
                self.set_config_error(None);
                let mut current = self.config.write().expect("config lock");
                if *current == new {
                    debug!("config reloaded, no change");
                    return false;
                }
                *current = new;
                info!("config reloaded");
                true
            }
            Err(e) => {
                // Keep running on the last good config and make the problem
                // visible, rather than exiting into a systemd restart loop.
                error!(code = %e.code, subject = "config", "{}", e.msg);
                self.set_config_error(Some(e.msg.clone()));
                self.event(corelog::Level::Error, Some(e.code), "config", &e.msg);
                false
            }
        }
    }

    fn set_config_error(&self, error: Option<String>) {
        *self.config_error.write().expect("config error lock") = error.clone();
        if let Err(e) = self.with_store(|s| s.set_config_error(error.as_deref())) {
            warn!(error = %e, "could not record config error");
        }
    }

    pub fn config_error(&self) -> Option<String> {
        self.config_error.read().expect("config error lock").clone()
    }

    /// Mirror the configured pins into the database so `status` can list them
    /// before P2 ever runs one, and so removed pins stop being reported.
    pub fn sync_pins_to_store(&self) {
        let config = self.config();
        let mut names = Vec::new();

        let result = self.with_store(|store| {
            for remote in config.enabled_remotes() {
                for pin in remote.pins.iter().filter(|p| p.enabled) {
                    store.register_pin(
                        &pin.name,
                        &remote.name,
                        &pin.remote_path,
                        &pin.local_path(&self.paths).to_string_lossy(),
                    )?;
                    names.push(pin.name.clone());
                }
            }
            store.retain_pins(&names)?;
            Ok(())
        });

        if let Err(e) = result {
            warn!(error = %e, "could not record pins");
        }
    }

    /// Whether a pin by this name is configured and enabled.
    fn has_pin(&self, name: &str) -> bool {
        self.config()
            .enabled_remotes()
            .any(|r| r.pins.iter().any(|p| p.enabled && p.name == name))
    }

    fn has_any_pin(&self) -> bool {
        self.config()
            .enabled_remotes()
            .any(|r| r.pins.iter().any(|p| p.enabled))
    }

    /// Tell the supervisor task that the pin set has changed.
    ///
    /// Failing here is harmless: the watches stay as they were and the next
    /// interval tick still syncs, so this is a promptness concern only.
    pub fn notify_pins_changed(&self) {
        if self.control_tx.try_send(Control::ReloadPins).is_err() {
            debug!("could not queue a pin reload; the next tick will catch up");
        }
    }

    // -- status ------------------------------------------------------------

    /// Wired up to NetworkManager's D-Bus signals in P1; until then the state
    /// stays `Unknown`, which the health roll-up deliberately does not penalise.
    #[allow(dead_code)]
    pub fn set_network(&self, state: NetworkState) {
        *self.network.write().expect("network lock") = state;
    }

    /// Record a pin's live state, so `status` and the TUI can report it.
    pub fn set_pin_state(&self, pin: &str, state: PinState, err: Option<(Code, &str)>) {
        if let Err(e) = self.with_store(|s| s.set_pin_state(pin, state, Utc::now(), err)) {
            warn!(pin = %pin, error = %e, "could not record pin state");
        }
    }

    pub fn set_daemon_state(&self, state: DaemonState) {
        if let Err(e) = self.with_store(|s| s.set_daemon_state(state)) {
            warn!(error = %e, "could not record daemon state");
        }
    }

    /// Build the snapshot that `status`, `doctor` and the TUI all read.
    pub fn status_report(&self) -> Result<StatusReport> {
        let config = self.config();
        let now = Utc::now();

        let (daemon, pins, conflicts, outbox_pending, outbox_oldest, index) =
            self.with_store(|store| {
                Ok((
                    store.daemon()?,
                    store.pins()?,
                    store.open_conflicts()?.len() as u64,
                    store.outbox_len()?,
                    store.outbox_oldest()?,
                    store.index_stats()?,
                ))
            })?;

        let daemon = daemon.ok_or_else(|| anyhow::anyhow!("no daemon record"))?;

        // Index staleness is judged against the shortest configured crawl
        // interval, doubled to avoid flapping right at the boundary.
        let crawl_budget = config
            .enabled_remotes()
            .map(|r| r.index.full_crawl_interval.as_secs())
            .min()
            .map(|s| (s * 2) as i64);
        let index_stale = match (index.last_crawl, crawl_budget) {
            // Nothing to crawl means nothing to be stale about.
            (_, None) => false,
            (None, Some(_)) => !config.remotes.is_empty(),
            (Some(last), Some(budget)) => (now - last).num_seconds() > budget,
        };

        let outbox_stale =
            outbox_oldest.is_some_and(|oldest| (now - oldest).num_seconds() > 24 * 3600);

        let remotes = config
            .remotes
            .iter()
            .map(|r| {
                let mount = self.mount_status(&r.name);
                RemoteStatus {
                    name: r.name.clone(),
                    // The path the user browses. Until the overlay lands (P3)
                    // this is where rclone is mounted directly; after P3 it is
                    // odfs in front of the backing mount.
                    mount_point: r.mount_point(&self.paths).to_string_lossy().into_owned(),
                    state: mount.state,
                    since: mount.since,
                    error: mount.error,
                    overlay: r.overlay.enabled,
                }
            })
            .collect();

        let log_dir = self.paths.log_dir();
        Ok(StatusReport {
            protocol: ipc::PROTOCOL_VERSION,
            source: ReportSource::Daemon,
            version: VERSION.to_string(),
            pid: daemon.pid,
            started_at: daemon.started_at,
            heartbeat_at: daemon.heartbeat_at,
            heartbeat_age_secs: (now - daemon.heartbeat_at).num_seconds(),
            daemon_state: daemon.state.as_str().to_string(),
            config_error: self.config_error(),
            network: *self.network.read().expect("network lock"),
            remotes,
            pins: pins
                .into_iter()
                .map(|p| PinStatus {
                    name: p.name,
                    remote: p.remote,
                    local_path: p.local_path,
                    state: p.state.as_str().to_string(),
                    last_ok: p.last_ok,
                    last_err: p.last_err,
                    last_err_code: p.last_err_code,
                })
                .collect(),
            index: IndexStatus {
                files: index.files,
                dirs: index.dirs,
                last_crawl: index.last_crawl,
                stale: index_stale,
            },
            conflicts,
            outbox: OutboxStatus {
                pending: outbox_pending,
                oldest: outbox_oldest,
                stale: outbox_stale,
            },
            log: LogStatus {
                dir: log_dir.to_string_lossy().into_owned(),
                bytes: corelog::dir_size(&log_dir).unwrap_or(0),
            },
        })
    }

    /// One-line summary for `systemctl status`.
    fn status_line(&self) -> String {
        match self.status_report() {
            Ok(report) => {
                let (health, why) = report.health();
                if why.is_empty() {
                    format!(
                        "{}: {} remote(s), {} pin(s)",
                        health.as_str(),
                        report.remotes.len(),
                        report.pins.len()
                    )
                } else {
                    format!("{}: {}", health.as_str(), why.join("; "))
                }
            }
            Err(e) => format!("status unavailable: {e}"),
        }
    }

    // -- requests ----------------------------------------------------------

    pub async fn handle(&self, req: Request) -> Response {
        match req {
            Request::Ping => Response::Pong {
                version: VERSION.into(),
                protocol: ipc::PROTOCOL_VERSION,
            },
            Request::Status => match self.status_report() {
                Ok(report) => Response::Status(Box::new(report)),
                Err(e) => Response::err(Code::Internal, "status", e.to_string()),
            },
            Request::Reload => match self.reload_tx.try_send(()) {
                Ok(()) => Response::Ok {
                    msg: "reload queued".into(),
                },
                Err(_) => Response::Ok {
                    msg: "a reload is already pending".into(),
                },
            },
            Request::PinSync { name } => {
                // Same reasoning as the remount below: answer a typo rather
                // than dropping it into a channel nobody will match.
                if let Some(pin) = &name
                    && !self.has_pin(pin)
                {
                    return Response::err(
                        Code::Config,
                        "pins",
                        format!("no enabled pin named {pin:?} in the config"),
                    );
                }
                if name.is_none() && !self.has_any_pin() {
                    return Response::err(
                        Code::Config,
                        "pins",
                        "no pins are configured; add one with `onedrive-tui pin add`",
                    );
                }

                match self
                    .control_tx
                    .try_send(Control::SyncPins { pin: name.clone() })
                {
                    Ok(()) => Response::Ok {
                        msg: match name {
                            Some(pin) => format!("syncing {pin}"),
                            None => "syncing all pins".into(),
                        },
                    },
                    // A pin cycle can take a while, so a full channel is a
                    // normal state rather than a fault.
                    Err(mpsc::error::TrySendError::Full(_)) => Response::Ok {
                        msg: "a sync is already pending".into(),
                    },
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        Response::err(Code::Internal, "pins", "the pin scheduler is not running")
                    }
                }
            }
            Request::MountRemount { remote } => {
                // Validate here so a typo gets a useful answer rather than being
                // silently dropped by the supervisor.
                if let Some(name) = &remote
                    && self.config().remote(name).is_none()
                {
                    return Response::err(
                        Code::Config,
                        "mount",
                        format!("no remote named {name:?} in the config"),
                    );
                }
                match self.control_tx.try_send(Control::Remount {
                    remote: remote.clone(),
                }) {
                    Ok(()) => Response::Ok {
                        msg: match remote {
                            Some(name) => format!("remounting {name}"),
                            None => "remounting all remotes".into(),
                        },
                    },
                    Err(mpsc::error::TrySendError::Full(_)) => Response::Ok {
                        msg: "a remount is already pending".into(),
                    },
                    Err(mpsc::error::TrySendError::Closed(_)) => Response::err(
                        Code::Internal,
                        "mount",
                        "the mount supervisor is not running",
                    ),
                }
            }
            Request::ConflictList => match self.conflict_list().await {
                Ok(list) => Response::Conflicts { conflicts: list },
                Err(e) => Response::err(Code::Internal, "conflicts", format!("{e:#}")),
            },

            Request::ConflictResolve { id, keep } => self.resolve_conflict(id, keep).await,

            Request::OutboxList => match self.outbox_list() {
                Ok(rows) => Response::Outbox { rows },
                Err(e) => Response::err(Code::Internal, "outbox", format!("{e:#}")),
            },

            Request::OutboxRetry { remote } => {
                if let Some(name) = &remote
                    && self.config().remote(name).is_none()
                {
                    return Response::err(
                        Code::Config,
                        "outbox",
                        format!("no remote named {name:?} in the config"),
                    );
                }
                match self.control_tx.try_send(Control::OutboxRetry {
                    remote: remote.clone(),
                }) {
                    Ok(()) => Response::Ok {
                        msg: match remote {
                            Some(name) => format!("replaying the outbox for {name}"),
                            None => "replaying the outbox for every enabled remote".into(),
                        },
                    },
                    Err(mpsc::error::TrySendError::Full(_)) => Response::Ok {
                        msg: "a replay is already pending".into(),
                    },
                    Err(mpsc::error::TrySendError::Closed(_)) => Response::err(
                        Code::Internal,
                        "outbox",
                        "the mount supervisor is not running",
                    ),
                }
            }

            Request::OutboxDiscard { id } => self.discard_outbox(id),

            Request::IndexRefresh { .. } => {
                // Both `full` and incremental ask for the same thing today:
                // every browse already folds a live listing into the index
                // opportunistically (see `odfs`), so the only thing a
                // deliberate refresh adds is a full crawl of paths nobody has
                // browsed recently.
                if self.config().remotes.iter().all(|r| !r.enabled) {
                    return Response::err(Code::Config, "index", "no enabled remotes to crawl");
                }
                match self.control_tx.try_send(Control::IndexRefresh) {
                    Ok(()) => Response::Ok {
                        msg: "crawling every enabled remote".into(),
                    },
                    Err(mpsc::error::TrySendError::Full(_)) => Response::Ok {
                        msg: "a crawl is already pending".into(),
                    },
                    Err(mpsc::error::TrySendError::Closed(_)) => Response::err(
                        Code::Internal,
                        "index",
                        "the mount supervisor is not running",
                    ),
                }
            }
        }
    }

    /// Open conflicts, with the copies that are *currently* on disk rather than
    /// the one recorded when the conflict was first detected — repeated
    /// conflicts on the same file accumulate copies.
    async fn conflict_list(&self) -> Result<Vec<ipc::ConflictInfo>> {
        let rows = self.with_store(|s| s.open_conflicts())?;
        let mut out = Vec::with_capacity(rows.len());

        for row in rows {
            let folder = row.version_dir.clone().unwrap_or_default();
            let copies = if folder.is_empty() {
                Vec::new()
            } else {
                crate::resolve::copies(std::path::Path::new(&folder), &row.path)
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .map(|c| c.name)
                    .collect()
            };

            out.push(ipc::ConflictInfo {
                id: row.id,
                subject: row.subject,
                remote: row.remote,
                file: row.path,
                folder,
                copies,
                detected_at: row.detected_at,
            });
        }
        Ok(out)
    }

    /// Act on one conflict.
    ///
    /// Answered synchronously, unlike the other mutating requests: this is the
    /// only operation in the protocol that can delete a file, so the caller is
    /// told what actually happened rather than that it was queued.
    async fn resolve_conflict(&self, id: i64, keep: ipc::Keep) -> Response {
        let row = match self.with_store(|s| s.conflict(id)) {
            Ok(Some((row, false))) => row,
            Ok(Some((_, true))) => {
                return Response::err(
                    Code::Config,
                    "conflicts",
                    format!("conflict {id} is already resolved"),
                );
            }
            Ok(None) => {
                return Response::err(
                    Code::Config,
                    "conflicts",
                    format!("no conflict with id {id}"),
                );
            }
            Err(e) => return Response::err(Code::Internal, "conflicts", format!("{e:#}")),
        };

        // `both` keeps every file exactly as it is; all it changes is that we
        // stop alerting. Closing the row is the whole operation.
        if keep == ipc::Keep::Both {
            if let Err(e) = self.with_store(|s| s.resolve_conflict(id, Utc::now())) {
                return Response::err(Code::Internal, "conflicts", format!("{e:#}"));
            }
            info!(
                conflict = id,
                file = %row.path,
                "conflict closed at the user's request; both versions kept on disk"
            );
            self.event(
                corelog::Level::Info,
                None,
                &format!("pin/{}", row.subject),
                &format!("{}: conflict closed, both versions kept", row.path),
            );
            return Response::Ok {
                msg: format!(
                    "{}: both versions kept; no longer alerting about it",
                    row.path
                ),
            };
        }

        let folder = row.version_dir.clone().unwrap_or_default();
        if folder.is_empty() {
            return Response::err(
                Code::Internal,
                "conflicts",
                format!("conflict {id} has no recorded folder, so it cannot be resolved for you"),
            );
        }

        match crate::resolve::apply(std::path::Path::new(&folder), &row.path, keep, Utc::now())
            .await
        {
            Ok(applied) => {
                self.event(
                    corelog::Level::Info,
                    None,
                    &format!("pin/{}", row.subject),
                    &format!(
                        "{}: kept the {} version, removed {}",
                        row.path,
                        keep.as_str(),
                        if applied.removed.is_empty() {
                            "nothing".to_string()
                        } else {
                            applied.removed.join(", ")
                        }
                    ),
                );

                // Push the removal up to OneDrive. The conflict row is left
                // open on purpose: `pins::resolve_finished` closes it once it
                // can see the copies are gone, so the row is closed on fact.
                let _ = self.control_tx.try_send(Control::SyncPins {
                    pin: Some(row.subject.clone()),
                });

                let mut msg = format!("{}: kept the {} version", row.path, keep.as_str());
                if applied.promoted {
                    msg.push_str(" (promoted into the original name)");
                }
                if !applied.removed.is_empty() {
                    msg.push_str(&format!("\nremoved: {}", applied.removed.join(", ")));
                }
                msg.push_str(&format!(
                    "\nthe other version is still in {}",
                    applied.snapshot.display()
                ));
                msg.push_str("\nsyncing the pin so OneDrive matches");
                Response::Ok { msg }
            }
            Err(e) => Response::err(Code::Conflict, "conflicts", format!("{e:#}")),
        }
    }

    /// Every write still queued in the outbox, across every remote.
    fn outbox_list(&self) -> Result<Vec<ipc::OutboxInfo>> {
        let rows = self.with_store(|s| s.open_outbox())?;
        Ok(rows
            .into_iter()
            .map(|r| ipc::OutboxInfo {
                id: r.id,
                remote: r.remote,
                path: r.path,
                staged_path: r.staged_path,
                queued_at: r.queued_at,
                state: r.state,
                attempts: r.attempts,
                last_err: r.last_err,
            })
            .collect())
    }

    /// Drop one queued write and delete its staged file.
    ///
    /// Answered synchronously, same reasoning as `resolve_conflict`: this is
    /// the one outbox operation that deletes something, so the caller is told
    /// what actually happened rather than that it was queued.
    fn discard_outbox(&self, id: i64) -> Response {
        let row = match self.with_store(|s| s.outbox_record(id)) {
            Ok(Some(row)) => row,
            Ok(None) => {
                return Response::err(
                    Code::Config,
                    "outbox",
                    format!("no queued write with id {id}"),
                );
            }
            Err(e) => return Response::err(Code::Internal, "outbox", format!("{e:#}")),
        };
        if let Err(e) = self.with_store(|s| s.remove_outbox(id)) {
            return Response::err(Code::Internal, "outbox", format!("{e:#}"));
        }
        if let Err(e) = std::fs::remove_file(&row.staged_path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            warn!(error = %e, path = %row.staged_path, "could not remove the staged file");
        }
        info!(path = %row.path, "offline write discarded at the user's request");
        Response::Ok {
            msg: format!("discarded the queued write for {}", row.path),
        }
    }
}

// ---------------------------------------------------------------------------
// Supervisor loop
// ---------------------------------------------------------------------------

/// Run until told to stop. Only timers and channels are awaited here.
pub async fn run(
    app: Arc<App>,
    mut reload_rx: mpsc::Receiver<()>,
    mut config_changed: mpsc::Receiver<()>,
    shutdown_tx: watch::Sender<bool>,
) -> Result<()> {
    let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
    // If we are ever late, skip the backlog rather than firing in a burst.
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut prune = tokio::time::interval(PRUNE_INTERVAL);
    prune.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut watchdog = app.notifier.watchdog_interval().map(|d| {
        let mut t = tokio::time::interval(d);
        t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        t
    });

    app.set_daemon_state(if app.config().remotes.is_empty() {
        DaemonState::Starting
    } else {
        DaemonState::Online
    });
    app.notifier.status(&app.status_line());

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;

    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                let app = Arc::clone(&app);
                // Even this tiny write goes off the loop, so a stalled disk
                // cannot wedge signal handling.
                tokio::task::spawn_blocking(move || {
                    if let Err(e) = app.with_store(|s| s.heartbeat(Utc::now())) {
                        warn!(error = %e, "heartbeat failed");
                    }
                    app.notifier.status(&app.status_line());
                });
            }

            _ = async { watchdog.as_mut().unwrap().tick().await }, if watchdog.is_some() => {
                // Reaching this proves the event loop is still turning, which is
                // exactly what the watchdog is meant to attest.
                app.notifier.watchdog_ping();
            }

            _ = prune.tick() => {
                let app = Arc::clone(&app);
                tokio::task::spawn_blocking(move || prune_logs(&app));
            }

            Some(()) = config_changed.recv() => {
                apply_reload(&app);
            }

            Some(()) = reload_rx.recv() => {
                info!("reload requested");
                apply_reload(&app);
            }

            _ = sighup.recv() => {
                info!("SIGHUP: reloading config");
                app.notifier.reloading();
                apply_reload(&app);
                app.notifier.ready();
            }

            _ = sigterm.recv() => {
                info!("SIGTERM: shutting down");
                break;
            }

            _ = sigint.recv() => {
                info!("SIGINT: shutting down");
                break;
            }
        }
    }

    app.notifier.stopping();
    app.notifier.status("shutting down");
    app.set_daemon_state(DaemonState::Stopping);

    if let Err(e) = app.with_store(|s| s.daemon_stopped()) {
        warn!(error = %e, "could not record clean shutdown");
    }

    // Tell the server task to stop accepting and clean up its socket.
    let _ = shutdown_tx.send(true);
    Ok(())
}

fn apply_reload(app: &Arc<App>) {
    if app.reload_config() {
        app.sync_pins_to_store();
        // The watched directories and the scheduler's idea of which pins exist
        // both follow the config, so they have to be rebuilt here.
        app.notify_pins_changed();
        app.event(
            corelog::Level::Info,
            None,
            "config",
            "configuration reloaded",
        );
    }
    app.notifier.status(&app.status_line());
}

fn prune_logs(app: &Arc<App>) {
    let config = app.config();
    let dir = app.paths.log_dir();

    match corelog::prune(
        &dir,
        config.daemon.log_retention_days,
        config.daemon.log_retention_size.as_bytes(),
        Utc::now().date_naive(),
    ) {
        Ok(pruned) if !pruned.removed.is_empty() => {
            info!(
                files = pruned.removed.len(),
                bytes_freed = pruned.bytes_freed,
                bytes_kept = pruned.bytes_kept,
                "pruned old log files"
            );
        }
        Ok(_) => debug!("log retention: nothing to prune"),
        Err(e) => warn!(error = %e, "log retention failed"),
    }

    if let Err(e) = app.with_store(|s| s.prune_events(EVENT_CAP)) {
        warn!(error = %e, "event pruning failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use onedrive_core::ipc::Health;

    fn test_app(home: &std::path::Path, config: Config) -> Arc<App> {
        let paths = Paths::rooted(home);
        let store = Store::open(&paths.state_db()).unwrap();
        store
            .daemon_started(std::process::id(), VERSION, Utc::now())
            .unwrap();
        let (tx, _rx) = mpsc::channel(1);
        let (control_tx, _control_rx) = mpsc::channel(1);
        Arc::new(App::new(
            paths,
            store,
            config,
            None,
            Notifier::from_env(),
            tx,
            control_tx,
        ))
    }

    fn documented_config() -> Config {
        toml::from_str(
            r#"
            [[remote]]
            name = "od"
            mount_point = "~/OneDrive"
            [[remote.pin]]
            name = "Keepass"
            remote_path = "Dokumente/Keepass"
            local_path = "~/onedrive-keepass"
            "#,
        )
        .unwrap()
    }

    #[test]
    fn status_report_is_self_consistent() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_app(dir.path(), documented_config());
        app.sync_pins_to_store();

        let report = app.status_report().unwrap();
        assert_eq!(report.protocol, ipc::PROTOCOL_VERSION);
        assert_eq!(report.source, ReportSource::Daemon);
        assert_eq!(report.pid, std::process::id());
        assert_eq!(report.remotes.len(), 1);
        assert_eq!(report.remotes[0].name, "od");
        assert!(report.remotes[0].overlay);
        assert_eq!(report.pins.len(), 1);
        assert_eq!(report.pins[0].name, "Keepass");
        assert_eq!(report.conflicts, 0);
        assert!(report.heartbeat_age_secs < 5);
        assert!(report.config_error.is_none());
    }

    /// P0 has no mount supervisor, so a configured remote reads as unmounted,
    /// which must be reported as degraded rather than quietly healthy.
    #[test]
    fn an_unmounted_remote_is_degraded() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_app(dir.path(), documented_config());
        let (health, why) = app.status_report().unwrap().health();
        assert_eq!(health, Health::Degraded);
        assert!(why.iter().any(|w| w.contains("not mounted")), "{why:?}");
    }

    #[test]
    fn an_empty_config_has_nothing_to_be_stale_about() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_app(dir.path(), Config::default());

        let report = app.status_report().unwrap();
        assert!(report.remotes.is_empty());
        assert!(!report.index.stale, "no remotes means no index to be stale");
        assert_eq!(report.health().0, Health::Healthy);
    }

    /// With a remote configured but no crawl ever done, the index is stale.
    #[test]
    fn a_never_crawled_index_is_stale_once_a_remote_exists() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_app(dir.path(), documented_config());
        assert!(app.status_report().unwrap().index.stale);

        app.with_store(|s| s.meta_set("last_full_crawl", &Utc::now().timestamp().to_string()))
            .unwrap();
        assert!(!app.status_report().unwrap().index.stale);
    }

    #[test]
    fn pins_are_mirrored_into_the_store_and_removals_are_pruned() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_app(dir.path(), documented_config());

        app.sync_pins_to_store();
        assert_eq!(app.with_store(|s| s.pins()).unwrap().len(), 1);

        // Reconfigure with no pins at all.
        *app.config.write().unwrap() = toml::from_str(
            r#"
            [[remote]]
            name = "od"
            mount_point = "~/OneDrive"
            "#,
        )
        .unwrap();
        app.sync_pins_to_store();
        assert!(app.with_store(|s| s.pins()).unwrap().is_empty());
    }

    #[test]
    fn disabled_pins_and_remotes_are_not_registered() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_app(
            dir.path(),
            toml::from_str(
                r#"
                [[remote]]
                name = "od"
                mount_point = "~/OneDrive"
                [[remote.pin]]
                name = "On"
                remote_path = "a"
                local_path = "~/a"
                [[remote.pin]]
                name = "Off"
                remote_path = "b"
                local_path = "~/b"
                enabled = false
                "#,
            )
            .unwrap(),
        );

        app.sync_pins_to_store();
        let pins = app.with_store(|s| s.pins()).unwrap();
        assert_eq!(pins.len(), 1);
        assert_eq!(pins[0].name, "On");
    }

    /// The core of the "does not exit on a bad config" rule: an invalid file is
    /// recorded, the previous config survives, and we keep running.
    #[test]
    fn an_invalid_config_is_recorded_without_losing_the_good_one() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_app(dir.path(), documented_config());
        let good = app.config();

        std::fs::create_dir_all(app.paths.config_dir()).unwrap();
        std::fs::write(
            app.paths.config_file(),
            "[[remote]]\nname = \"\"\nmount_point = \"\"\n",
        )
        .unwrap();

        assert!(!app.reload_config(), "an invalid config is not a change");
        assert_eq!(app.config(), good, "the last good config must survive");

        let err = app.config_error().expect("the error should be recorded");
        assert!(err.contains("problems in the config"), "{err}");

        let report = app.status_report().unwrap();
        assert_eq!(report.health().0, Health::Broken);
        assert!(report.config_error.is_some());
    }

    #[test]
    fn a_valid_config_clears_a_previous_error() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_app(dir.path(), Config::default());
        std::fs::create_dir_all(app.paths.config_dir()).unwrap();

        std::fs::write(app.paths.config_file(), "nonsense = ").unwrap();
        app.reload_config();
        assert!(app.config_error().is_some());

        std::fs::write(
            app.paths.config_file(),
            "[[remote]]\nname = \"od\"\nmount_point = \"~/OneDrive\"\n",
        )
        .unwrap();
        assert!(app.reload_config(), "this is a real change");
        assert!(app.config_error().is_none());
        assert_eq!(app.config().remotes.len(), 1);
    }

    #[test]
    fn a_missing_config_is_a_valid_pre_setup_state() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_app(dir.path(), Config::default());

        assert!(!app.paths.config_file().exists());
        assert!(!app.reload_config());
        assert!(
            app.config_error().is_none(),
            "not having set up yet is not an error"
        );
    }

    #[test]
    fn reloading_an_unchanged_config_reports_no_change() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_app(dir.path(), Config::default());
        std::fs::create_dir_all(app.paths.config_dir()).unwrap();

        let text = "[[remote]]\nname = \"od\"\nmount_point = \"~/OneDrive\"\n";
        std::fs::write(app.paths.config_file(), text).unwrap();

        assert!(app.reload_config(), "first load is a change");
        assert!(!app.reload_config(), "an identical reload is not");
    }

    #[tokio::test]
    async fn ping_and_status_are_answered() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_app(dir.path(), documented_config());

        match app.handle(Request::Ping).await {
            Response::Pong { protocol, version } => {
                assert_eq!(protocol, ipc::PROTOCOL_VERSION);
                assert_eq!(version, VERSION);
            }
            other => panic!("unexpected {other:?}"),
        }

        assert!(matches!(
            app.handle(Request::Status).await,
            Response::Status(_)
        ));
    }

    #[tokio::test]
    async fn an_index_refresh_is_queued_for_the_supervisor() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path());
        let store = Store::open(&paths.state_db()).unwrap();
        store.daemon_started(1, VERSION, Utc::now()).unwrap();
        let (reload_tx, _reload_rx) = mpsc::channel(1);
        let (control_tx, mut control_rx) = mpsc::channel(4);
        let app = App::new(
            paths,
            store,
            documented_config(),
            None,
            Notifier::from_env(),
            reload_tx,
            control_tx,
        );

        match app.handle(Request::IndexRefresh { full: true }).await {
            Response::Ok { msg } => assert!(msg.contains("crawling"), "{msg}"),
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(control_rx.recv().await, Some(Control::IndexRefresh));
    }

    #[tokio::test]
    async fn an_index_refresh_with_no_enabled_remotes_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_app(dir.path(), Config::default());

        match app.handle(Request::IndexRefresh { full: false }).await {
            Response::Err { code, msg, .. } => {
                assert_eq!(code, Code::Config);
                assert!(msg.contains("no enabled remotes"), "{msg}");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_pin_sync_is_queued_for_the_supervisor() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path());
        let store = Store::open(&paths.state_db()).unwrap();
        store.daemon_started(1, VERSION, Utc::now()).unwrap();

        let (reload_tx, _reload_rx) = mpsc::channel(1);
        let (control_tx, mut control_rx) = mpsc::channel(4);
        let app = App::new(
            paths,
            store,
            documented_config(),
            None,
            Notifier::from_env(),
            reload_tx,
            control_tx,
        );

        match app
            .handle(Request::PinSync {
                name: Some("Keepass".into()),
            })
            .await
        {
            Response::Ok { msg } => assert!(msg.contains("syncing Keepass"), "{msg}"),
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(
            control_rx.recv().await,
            Some(Control::SyncPins {
                pin: Some("Keepass".into())
            })
        );

        // No name: every enabled pin.
        app.handle(Request::PinSync { name: None }).await;
        assert_eq!(
            control_rx.recv().await,
            Some(Control::SyncPins { pin: None })
        );
    }

    /// Asking for a pin that is not configured must say so rather than looking
    /// like it worked — this is the command the user reaches for when they are
    /// already worried about whether their database synced.
    #[tokio::test]
    async fn syncing_an_unknown_pin_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_app(dir.path(), documented_config());

        match app
            .handle(Request::PinSync {
                name: Some("nope".into()),
            })
            .await
        {
            Response::Err { code, msg, .. } => {
                assert_eq!(code, Code::Config);
                assert!(msg.contains("no enabled pin named \"nope\""), "{msg}");
            }
            other => panic!("expected a rejection, got {other:?}"),
        }

        // A disabled pin is not syncable either, for the same reason.
        let app = test_app(
            dir.path(),
            toml::from_str(
                r#"
                [[remote]]
                name = "od"
                mount_point = "~/OneDrive"
                [[remote.pin]]
                name = "Off"
                remote_path = "b"
                local_path = "~/b"
                enabled = false
                "#,
            )
            .unwrap(),
        );
        assert!(matches!(
            app.handle(Request::PinSync {
                name: Some("Off".into())
            })
            .await,
            Response::Err {
                code: Code::Config,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn syncing_with_no_pins_configured_explains_rather_than_succeeding() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_app(dir.path(), Config::default());

        match app.handle(Request::PinSync { name: None }).await {
            Response::Err { code, msg, .. } => {
                assert_eq!(code, Code::Config);
                assert!(msg.contains("no pins are configured"), "{msg}");
                assert!(msg.contains("pin add"), "the hint should say what to do");
            }
            other => panic!("expected a rejection, got {other:?}"),
        }
    }

    /// A config change has to rebuild the inotify watches, or a newly added pin
    /// would only ever sync on its interval.
    #[test]
    fn reloading_the_config_asks_for_the_pin_watches_to_be_rebuilt() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path());
        let store = Store::open(&paths.state_db()).unwrap();
        store.daemon_started(1, VERSION, Utc::now()).unwrap();

        let (reload_tx, _reload_rx) = mpsc::channel(1);
        let (control_tx, mut control_rx) = mpsc::channel(4);
        let app = Arc::new(App::new(
            paths,
            store,
            Config::default(),
            None,
            Notifier::from_env(),
            reload_tx,
            control_tx,
        ));

        std::fs::create_dir_all(app.paths.config_dir()).unwrap();
        std::fs::write(
            app.paths.config_file(),
            "[[remote]]\nname = \"od\"\nmount_point = \"~/OneDrive\"\n",
        )
        .unwrap();

        apply_reload(&app);
        assert_eq!(control_rx.try_recv(), Ok(Control::ReloadPins));

        // An unchanged config is not a change, so nothing is queued.
        apply_reload(&app);
        assert!(control_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn remount_is_queued_for_the_supervisor() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path());
        let store = Store::open(&paths.state_db()).unwrap();
        store.daemon_started(1, VERSION, Utc::now()).unwrap();

        let (reload_tx, _reload_rx) = mpsc::channel(1);
        let (control_tx, mut control_rx) = mpsc::channel(4);
        let app = App::new(
            paths,
            store,
            documented_config(),
            None,
            Notifier::from_env(),
            reload_tx,
            control_tx,
        );

        match app
            .handle(Request::MountRemount {
                remote: Some("od".into()),
            })
            .await
        {
            Response::Ok { msg } => assert!(msg.contains("remounting od"), "{msg}"),
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(
            control_rx.recv().await,
            Some(Control::Remount {
                remote: Some("od".into())
            })
        );

        // No remote named: means all of them.
        app.handle(Request::MountRemount { remote: None }).await;
        assert_eq!(
            control_rx.recv().await,
            Some(Control::Remount { remote: None })
        );
    }

    /// A typo should be answered, not silently dropped on the floor.
    #[tokio::test]
    async fn remounting_an_unknown_remote_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_app(dir.path(), documented_config());

        match app
            .handle(Request::MountRemount {
                remote: Some("nope".into()),
            })
            .await
        {
            Response::Err { code, msg, .. } => {
                assert_eq!(code, Code::Config);
                assert!(msg.contains("no remote named \"nope\""), "{msg}");
            }
            other => panic!("expected a rejection, got {other:?}"),
        }
    }

    #[test]
    fn mount_status_defaults_to_unmounted_and_can_be_published() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_app(dir.path(), documented_config());

        assert_eq!(app.mount_status("od").state, MountState::Unmounted);

        app.set_mount_status(
            "od",
            MountStatus {
                state: MountState::Mounted,
                since: Some(Utc::now()),
                error: None,
                code: None,
            },
        );
        assert_eq!(app.mount_status("od").state, MountState::Mounted);

        // The status report picks it up, so `status` stops claiming unmounted.
        let report = app.status_report().unwrap();
        assert_eq!(report.remotes[0].state, MountState::Mounted);
        assert!(report.remotes[0].since.is_some());

        // Removing the remote from the config drops its mount status.
        app.retain_mount_status(&[]);
        assert_eq!(app.mount_status("od").state, MountState::Unmounted);
    }

    #[tokio::test]
    async fn reload_requests_are_queued_and_coalesced() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path());
        let store = Store::open(&paths.state_db()).unwrap();
        store.daemon_started(1, VERSION, Utc::now()).unwrap();

        let (tx, mut rx) = mpsc::channel(1);
        let (control_tx, _control_rx) = mpsc::channel(1);
        let app = App::new(
            paths,
            store,
            Config::default(),
            None,
            Notifier::from_env(),
            tx,
            control_tx,
        );

        assert!(matches!(
            app.handle(Request::Reload).await,
            Response::Ok { .. }
        ));
        // The channel holds one; a second must not be reported as an error.
        match app.handle(Request::Reload).await {
            Response::Ok { msg } => assert!(msg.contains("already pending"), "{msg}"),
            other => panic!("unexpected {other:?}"),
        }
        assert!(rx.recv().await.is_some());
    }

    // -- conflicts ---------------------------------------------------------

    /// Lay out a pin folder the way a real conflict leaves it, and record it.
    fn with_conflict(app: &Arc<App>) -> (i64, std::path::PathBuf) {
        let folder = app.paths.home().join("onedrive-keepass");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("Timm.kdbx"), "LOCAL").unwrap();
        std::fs::write(
            folder.join("Timm.conflict-remote-2026-10-08T124530Z.kdbx"),
            "REMOTE",
        )
        .unwrap();

        let id = app
            .with_store(|s| {
                s.insert_conflict(
                    "pin",
                    "od",
                    "Keepass",
                    "Timm.kdbx",
                    Some("Timm.kdbx"),
                    Some("Timm.conflict-remote-2026-10-08T124530Z.kdbx"),
                    Some(&folder.to_string_lossy()),
                    Utc::now(),
                )
            })
            .unwrap();
        (id, folder)
    }

    #[tokio::test]
    async fn conflicts_are_listed_with_the_copies_currently_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_app(dir.path(), documented_config());
        let (id, folder) = with_conflict(&app);

        // A second conflict on the same file left another copy behind, which
        // the database row does not know about.
        std::fs::write(
            folder.join("Timm.conflict-local-2026-10-09T090000Z.kdbx"),
            "LOCAL-2",
        )
        .unwrap();

        match app.handle(Request::ConflictList).await {
            Response::Conflicts { conflicts: list } => {
                assert_eq!(list.len(), 1);
                assert_eq!(list[0].id, id);
                assert_eq!(list[0].file, "Timm.kdbx");
                assert_eq!(list[0].subject, "Keepass");
                assert_eq!(
                    list[0].copies,
                    vec![
                        "Timm.conflict-remote-2026-10-08T124530Z.kdbx".to_string(),
                        "Timm.conflict-local-2026-10-09T090000Z.kdbx".to_string(),
                    ],
                    "both copies, oldest first"
                );
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_empty_conflict_list_is_an_empty_list_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_app(dir.path(), documented_config());
        assert_eq!(
            app.handle(Request::ConflictList).await,
            Response::Conflicts { conflicts: vec![] }
        );
    }

    /// The headline case for a `.kdbx`: the user merges by hand and just wants
    /// the alert to stop. Nothing on disk may change.
    #[tokio::test]
    async fn keeping_both_closes_the_row_and_touches_no_files() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_app(dir.path(), documented_config());
        let (id, folder) = with_conflict(&app);

        match app
            .handle(Request::ConflictResolve {
                id,
                keep: ipc::Keep::Both,
            })
            .await
        {
            Response::Ok { msg } => assert!(msg.contains("both versions kept"), "{msg}"),
            other => panic!("unexpected {other:?}"),
        }

        assert!(app.with_store(|s| s.open_conflicts()).unwrap().is_empty());
        assert!(folder.join("Timm.kdbx").exists());
        assert!(
            folder
                .join("Timm.conflict-remote-2026-10-08T124530Z.kdbx")
                .exists(),
            "`both` must not delete anything"
        );
    }

    #[tokio::test]
    async fn keeping_one_side_does_the_file_work_and_queues_a_sync() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path());
        let store = Store::open(&paths.state_db()).unwrap();
        store.daemon_started(1, VERSION, Utc::now()).unwrap();
        let (reload_tx, _reload_rx) = mpsc::channel(1);
        let (control_tx, mut control_rx) = mpsc::channel(4);
        let app = Arc::new(App::new(
            paths,
            store,
            documented_config(),
            None,
            Notifier::from_env(),
            reload_tx,
            control_tx,
        ));
        let (id, folder) = with_conflict(&app);

        match app
            .handle(Request::ConflictResolve {
                id,
                keep: ipc::Keep::Remote,
            })
            .await
        {
            Response::Ok { msg } => {
                assert!(msg.contains("kept the remote version"), "{msg}");
                assert!(
                    msg.contains(".versions"),
                    "it must say where the other went"
                );
            }
            other => panic!("unexpected {other:?}"),
        }

        assert_eq!(
            std::fs::read_to_string(folder.join("Timm.kdbx")).unwrap(),
            "REMOTE"
        );
        assert!(
            !folder
                .join("Timm.conflict-remote-2026-10-08T124530Z.kdbx")
                .exists()
        );

        // The row stays open: `pins::resolve_finished` closes it once it can
        // see for itself that the copies are gone.
        assert_eq!(app.with_store(|s| s.open_conflicts()).unwrap().len(), 1);

        // And the removal has to reach OneDrive.
        assert_eq!(
            control_rx.recv().await,
            Some(Control::SyncPins {
                pin: Some("Keepass".into())
            })
        );
    }

    #[tokio::test]
    async fn resolving_a_conflict_twice_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_app(dir.path(), documented_config());
        let (id, _) = with_conflict(&app);

        app.handle(Request::ConflictResolve {
            id,
            keep: ipc::Keep::Both,
        })
        .await;

        match app
            .handle(Request::ConflictResolve {
                id,
                keep: ipc::Keep::Both,
            })
            .await
        {
            Response::Err { msg, .. } => assert!(msg.contains("already resolved"), "{msg}"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn resolving_an_unknown_conflict_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_app(dir.path(), documented_config());

        match app
            .handle(Request::ConflictResolve {
                id: 99,
                keep: ipc::Keep::Local,
            })
            .await
        {
            Response::Err { code, msg, .. } => {
                assert_eq!(code, Code::Config);
                assert!(msg.contains("no conflict with id 99"), "{msg}");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    /// R3 reaching the protocol: when the requested side is not on disk, the
    /// request fails and nothing is touched.
    #[tokio::test]
    async fn asking_to_keep_a_side_that_is_not_there_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_app(dir.path(), documented_config());
        let (id, folder) = with_conflict(&app);
        // Only the canonical file is left; there is nothing to choose between.
        std::fs::remove_file(folder.join("Timm.conflict-remote-2026-10-08T124530Z.kdbx")).unwrap();

        match app
            .handle(Request::ConflictResolve {
                id,
                keep: ipc::Keep::Remote,
            })
            .await
        {
            Response::Err { code, msg, .. } => {
                assert_eq!(code, Code::Conflict);
                assert!(msg.contains("no extra versions"), "{msg}");
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(folder.join("Timm.kdbx")).unwrap(),
            "LOCAL"
        );
        assert_eq!(
            app.with_store(|s| s.open_conflicts()).unwrap().len(),
            1,
            "a failed resolution must leave the conflict open"
        );
    }

    // -- outbox --------------------------------------------------------

    #[tokio::test]
    async fn an_empty_outbox_list_is_an_empty_list_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_app(dir.path(), documented_config());
        assert_eq!(
            app.handle(Request::OutboxList).await,
            Response::Outbox { rows: vec![] }
        );
    }

    #[tokio::test]
    async fn the_outbox_is_listed_as_queued_rows() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_app(dir.path(), documented_config());
        let id = app
            .with_store(|s| {
                s.queue_outbox(
                    "od",
                    "notes.txt",
                    "/home/t/OneDrive.pending/notes.txt",
                    Some(5),
                    None,
                    None,
                    Utc::now(),
                )
            })
            .unwrap();

        match app.handle(Request::OutboxList).await {
            Response::Outbox { rows } => {
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0].id, id);
                assert_eq!(rows[0].remote, "od");
                assert_eq!(rows[0].path, "notes.txt");
                assert_eq!(rows[0].state, "queued");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn discarding_a_queued_write_removes_the_row_and_the_staged_file() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_app(dir.path(), documented_config());
        let staged = dir.path().join("staged.txt");
        std::fs::write(&staged, "edited offline").unwrap();
        let id = app
            .with_store(|s| {
                s.queue_outbox(
                    "od",
                    "notes.txt",
                    &staged.to_string_lossy(),
                    None,
                    None,
                    None,
                    Utc::now(),
                )
            })
            .unwrap();

        match app.handle(Request::OutboxDiscard { id }).await {
            Response::Ok { msg } => assert!(msg.contains("notes.txt"), "{msg}"),
            other => panic!("unexpected {other:?}"),
        }

        assert!(
            app.with_store(|s| s.open_outbox()).unwrap().is_empty(),
            "the row must be gone"
        );
        assert!(!staged.exists(), "the staged file must be gone too");
    }

    #[tokio::test]
    async fn discarding_an_unknown_id_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_app(dir.path(), documented_config());

        match app.handle(Request::OutboxDiscard { id: 99 }).await {
            Response::Err { code, msg, .. } => {
                assert_eq!(code, Code::Config);
                assert!(msg.contains("no queued write with id 99"), "{msg}");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn retrying_the_outbox_queues_a_control_message() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path());
        let store = Store::open(&paths.state_db()).unwrap();
        store.daemon_started(1, VERSION, Utc::now()).unwrap();
        let (reload_tx, _reload_rx) = mpsc::channel(1);
        let (control_tx, mut control_rx) = mpsc::channel(4);
        let app = Arc::new(App::new(
            paths,
            store,
            documented_config(),
            None,
            Notifier::from_env(),
            reload_tx,
            control_tx,
        ));

        match app
            .handle(Request::OutboxRetry {
                remote: Some("od".into()),
            })
            .await
        {
            Response::Ok { msg } => assert!(msg.contains("od"), "{msg}"),
            other => panic!("unexpected {other:?}"),
        }

        assert_eq!(
            control_rx.recv().await,
            Some(Control::OutboxRetry {
                remote: Some("od".into())
            })
        );
    }

    #[tokio::test]
    async fn retrying_an_unknown_remote_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_app(dir.path(), documented_config());

        match app
            .handle(Request::OutboxRetry {
                remote: Some("nope".into()),
            })
            .await
        {
            Response::Err { code, msg, .. } => {
                assert_eq!(code, Code::Config);
                assert!(msg.contains("no remote named"), "{msg}");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn events_are_recorded_for_the_tui() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_app(dir.path(), Config::default());

        app.event(
            corelog::Level::Warn,
            Some(Code::Offline),
            "pin/Keepass",
            "deferred",
        );
        let events = app.with_store(|s| s.recent_events(10)).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].code, Some(Code::Offline));
        assert_eq!(events[0].subject, "pin/Keepass");
        assert_eq!(events[0].level, "WARN");
    }

    #[test]
    fn log_pruning_enforces_retention() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_app(dir.path(), Config::default());

        let log_dir = app.paths.log_dir();
        std::fs::create_dir_all(&log_dir).unwrap();
        std::fs::write(log_dir.join("onedrived.jsonl.2020-01-01"), vec![b'x'; 1000]).unwrap();
        std::fs::write(log_dir.join("onedrived.jsonl"), vec![b'x'; 10]).unwrap();

        prune_logs(&app);
        assert!(!log_dir.join("onedrived.jsonl.2020-01-01").exists());
        assert!(log_dir.join("onedrived.jsonl").exists());
    }

    #[test]
    fn the_status_line_summarises_health_for_systemctl() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_app(dir.path(), Config::default());
        let line = app.status_line();
        assert!(line.starts_with("healthy:"), "{line}");

        let app = test_app(dir.path(), documented_config());
        let line = app.status_line();
        assert!(line.starts_with("degraded:"), "{line}");
        assert!(line.contains("not mounted"), "{line}");
    }
}
