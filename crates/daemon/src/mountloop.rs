//! The task that owns the mount supervisor.
//!
//! Separated from the main daemon loop on purpose. The supervisor holds process
//! handles and must stay on one task; the main loop must never block on
//! anything mount-related, since a hung FUSE mount freezing the event loop is
//! exactly the old daemon's finding 2. The two communicate only through
//! channels and an `RwLock`ed status snapshot.
//!
//! This task is also where lifecycle events land, because every one of them
//! (suspend, resume, network, lock) ultimately means "do something to the
//! mounts".

use std::sync::Arc;
use std::time::Duration;

use onedrive_rc_core::ipc::NetworkState;
use onedrive_rc_core::state::DaemonState;
use onedrive_rc_core::{Code, log as corelog};
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};

use crate::app::{App, Control};
use crate::lifecycle::{self, Event};
use crate::mount::Supervisor;
use crate::notify::{self, Notifier};
use crate::outbox;
use crate::pins::{self, Scheduler, Trigger};
use crate::pinwatch;

/// How often the supervisor reconciles: checks children, probes liveness, and
/// starts anything that should be running.
///
/// 15s is what the old daemon used, and the interval was never the problem —
/// the problem was that the probe could block forever and that a dead mount had
/// nobody responsible for restarting it. Both are fixed in `mount.rs`.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(15);

/// After resuming, wait this long before touching anything. The network stack
/// needs a moment, and probing into a half-restored interface just produces
/// noise and a spurious restart.
const RESUME_SETTLE: Duration = Duration::from_secs(3);

/// How often the pin scheduler is examined. Fine-grained relative to the pin
/// intervals so a debounced local change is acted on promptly rather than
/// waiting out the reconcile tick.
const PIN_TICK: Duration = Duration::from_secs(2);

/// How often to check whether any remote's index is due for a full crawl.
/// Coarse on purpose — crawl intervals are measured in hours — this is just
/// how promptly a newly-elapsed interval is noticed.
const INDEX_CHECK_INTERVAL: Duration = Duration::from_secs(600);

pub async fn run(
    app: Arc<App>,
    mut control_rx: mpsc::Receiver<Control>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut supervisor = Supervisor::new(app.paths.clone());

    let mut reconcile = tokio::time::interval(RECONCILE_INTERVAL);
    reconcile.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut lifecycle_rx = lifecycle::subscribe().await;
    let mut notifier = Notifier::connect().await;
    let mut scheduler = Scheduler::default();

    // Watch the pinned directories so a save syncs in seconds rather than at
    // the next interval. Kept alive for as long as we run.
    let (mut pin_changes, mut _pin_watchers) = pinwatch::watch(&app.config(), &app.paths);

    let mut pin_tick = tokio::time::interval(PIN_TICK);
    pin_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut reassert = tokio::time::interval(notify::REASSERT_INTERVAL);
    reassert.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut index_tick = tokio::time::interval(INDEX_CHECK_INTERVAL);
    index_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Hold a sleep inhibitor so logind gives us a window before suspending.
    // Optional: without it we simply get no warning, and the resume path
    // re-probes everything anyway.
    let _inhibitor = match zbus::Connection::system().await {
        Ok(conn) => lifecycle::inhibit_sleep(&conn).await,
        Err(_) => None,
    };

    // `Unknown` until NetworkManager tells us otherwise. Treated as online so a
    // machine without NM still works.
    let mut online = true;
    let mut suspended = false;

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    break;
                }
            }

            _ = reconcile.tick() => {
                if suspended {
                    debug!("suspended; not touching mounts");
                    continue;
                }
                if !online {
                    // Mounting with no network just burns retries on DNS
                    // failures, which is most of what filled the old log.
                    debug!("offline; deferring mount work");
                    continue;
                }
                reconcile_now(&app, &mut supervisor).await;
            }

            _ = pin_tick.tick() => {
                if suspended || !online {
                    continue;
                }
                let config = app.config();
                let now = tokio::time::Instant::now();

                // Debounced local changes first: they are the most recent
                // intent, and a sync covers the interval anyway.
                for name in scheduler.debounced(now) {
                    if let Some((remote, pin)) = find_pin(&config, &name) {
                        pins::cycle(&app, &mut notifier, &remote, &pin, Trigger::LocalChange).await;
                        scheduler.mark_run(&name, tokio::time::Instant::now());
                    }
                }

                for (remote, name) in scheduler.due(&config, now) {
                    if let Some((remote, pin)) = find_pin(&config, &name) {
                        pins::cycle(&app, &mut notifier, &remote, &pin, Trigger::Interval).await;
                        scheduler.mark_run(&name, tokio::time::Instant::now());
                    } else {
                        debug!(remote = %remote, pin = %name, "pin vanished before its turn");
                    }
                }

                // Withdraw notifications whose conflict the user has dealt with.
                pins::resolve_finished(&app, &mut notifier).await;
            }

            Some(pin_name) = pin_changes.recv() => {
                debug!(pin = %pin_name, "local change noticed");
                scheduler.note_change(&pin_name, tokio::time::Instant::now());
            }

            _ = reassert.tick() => {
                // The conflict is still open, so say so again. This is what
                // makes the alert persist rather than fire once.
                pins::reassert(&app, &mut notifier).await;
            }

            Some(control) = control_rx.recv() => {
                match control {
                    Control::SyncPins { pin } => {
                        let config = app.config();
                        let targets: Vec<(String, _)> = match &pin {
                            Some(name) => find_pin(&config, name).into_iter().collect(),
                            None => config
                                .enabled_remotes()
                                .flat_map(|r| {
                                    r.pins
                                        .iter()
                                        .filter(|p| p.enabled)
                                        .map(move |p| (r.name.clone(), p.clone()))
                                })
                                .collect(),
                        };
                        if targets.is_empty() {
                            debug!(?pin, "no matching pin to sync");
                        }
                        for (remote, pin_config) in targets {
                            pins::cycle(&app, &mut notifier, &remote, &pin_config, Trigger::Manual)
                                .await;
                            scheduler.mark_run(&pin_config.name, tokio::time::Instant::now());
                        }
                    }

                    Control::ReloadPins => {
                        // The watched set follows the config, so rebuild it.
                        let config = app.config();
                        let (rx, watchers) = pinwatch::watch(&config, &app.paths);
                        pin_changes = rx;
                        _pin_watchers = watchers;
                        let names: Vec<String> = config
                            .enabled_remotes()
                            .flat_map(|r| r.pins.iter().map(|p| p.name.clone()))
                            .collect();
                        scheduler.retain(&names);
                        debug!(pins = names.len(), "pin watchers rebuilt");
                    }

                    Control::Remount { remote } => {
                        let config = app.config();
                        let targets: Vec<String> = match remote {
                            Some(name) => vec![name],
                            None => config.enabled_remotes().map(|r| r.name.clone()).collect(),
                        };
                        for name in targets {
                            supervisor.force_remount(&name).await;
                            app.set_mount_status(&name, supervisor.status(&name));
                        }
                        // Bring them straight back up rather than waiting for
                        // the next tick — the user asked for this.
                        reconcile_now(&app, &mut supervisor).await;
                    }

                    Control::IndexRefresh => crawl_all(&app).await,

                    Control::OutboxRetry { remote } => {
                        replay_outbox_all(&app, &mut notifier, remote.as_deref()).await;
                    }
                }
            }

            _ = index_tick.tick() => {
                if !suspended && online {
                    crawl_due_remotes(&app).await;
                }
            }

            Some(event) = lifecycle_rx.recv() => {
                match event {
                    Event::Suspending => {
                        suspended = true;
                        app.set_daemon_state(DaemonState::Suspending);
                        // Checkpoint only. A sync here would not fit inside
                        // logind's delay window, and being killed halfway
                        // through one is worse than not starting it.
                        if let Err(e) = app.with_store(|s| s.heartbeat(chrono::Utc::now())) {
                            debug!(error = %e, "could not checkpoint before suspend");
                        }
                        app.event(corelog::Level::Info, None, "lifecycle", "suspending");
                    }

                    Event::Resumed => {
                        suspended = false;
                        info!("resumed; re-checking mounts");
                        app.event(corelog::Level::Info, None, "lifecycle", "resumed from suspend");

                        // Let the network settle before judging anything.
                        tokio::time::sleep(RESUME_SETTLE).await;

                        // A mount that survived the suspend still has stale
                        // directory listings; dir_cache_time is 3 days.
                        reconcile_now(&app, &mut supervisor).await;
                        supervisor.refresh_all().await;
                        sync_all_pins(&app, &mut notifier, &mut scheduler, Trigger::Resume).await;
                    }

                    Event::NetworkOnline => {
                        if !online {
                            info!("network is up");
                            app.event(corelog::Level::Info, None, "lifecycle", "network is up");
                        }
                        online = true;
                        app.set_network(NetworkState::Online);
                        if !suspended {
                            reconcile_now(&app, &mut supervisor).await;
                            supervisor.refresh_all().await;
                            sync_all_pins(
                                &app,
                                &mut notifier,
                                &mut scheduler,
                                Trigger::Reconnect,
                            )
                            .await;
                            replay_outbox_all(&app, &mut notifier, None).await;
                        }
                    }

                    Event::NetworkOffline => {
                        if online {
                            info!(
                                code = Code::Offline.as_str(),
                                "network is down; pausing remote work"
                            );
                            app.event(
                                corelog::Level::Info,
                                Some(Code::Offline),
                                "lifecycle",
                                "network is down; remote work deferred",
                            );
                        }
                        online = false;
                        app.set_network(NetworkState::Offline);
                        // The mount is deliberately left up: reads of cached
                        // files keep working, and tearing it down would make
                        // the directory vanish from under the user.
                    }

                    Event::ScreenLocked => {
                        // The best available signal for "the user has finished",
                        // which makes it a good moment to push a database they
                        // just edited.
                        if !suspended && online {
                            sync_all_pins(&app, &mut notifier, &mut scheduler, Trigger::ScreenLock)
                                .await;
                        }
                    }

                    Event::ScreenUnlocked => debug!("screen unlocked"),
                }
            }
        }
    }

    info!("stopping mounts");
    let _ = &mut notifier;
    supervisor.stop_all().await;
    for remote in app.config().remotes.iter() {
        app.set_mount_status(&remote.name, supervisor.status(&remote.name));
    }
    debug!("mount supervisor stopped");
}

/// Reconcile once and publish the result where `status` can read it.
async fn reconcile_now(app: &Arc<App>, supervisor: &mut Supervisor) {
    let config = app.config();
    supervisor
        .reconcile(&config.remotes, config.daemon.rclone_log_level)
        .await;

    let names: Vec<String> = config.remotes.iter().map(|r| r.name.clone()).collect();
    for name in &names {
        app.set_mount_status(name, supervisor.status(name));
    }
    app.retain_mount_status(&names);

    // Keep the daemon state honest, so `systemctl status` and `onedrive-rc
    // status` agree about whether anything is wrong.
    let state = if config.remotes.is_empty() {
        DaemonState::Starting
    } else if supervisor.any_unhealthy() {
        DaemonState::Degraded
    } else {
        DaemonState::Online
    };
    app.set_daemon_state(state);

    // Surface a mount failure as an event, so it shows up in the TUI and in
    // `onedrive-rc log` rather than only in the status snapshot.
    for name in &names {
        let status = supervisor.status(name);
        if let Some(error) = &status.error {
            warn!(
                remote = %name,
                subject = %format!("mount/{name}"),
                code = status.code.map(|c| c.as_str()).unwrap_or(""),
                "{error}"
            );
        }
    }
}

/// Find a pin and its remote by name.
fn find_pin(
    config: &onedrive_rc_core::config::Config,
    name: &str,
) -> Option<(String, onedrive_rc_core::config::PinConfig)> {
    config.enabled_remotes().find_map(|remote| {
        remote
            .pins
            .iter()
            .find(|p| p.enabled && p.name == name)
            .map(|p| (remote.name.clone(), p.clone()))
    })
}

/// Sync every enabled pin, for the triggers that are not about one pin.
async fn sync_all_pins(
    app: &Arc<App>,
    notifier: &mut Notifier,
    scheduler: &mut Scheduler,
    trigger: Trigger,
) {
    let config = app.config();
    for remote in config.enabled_remotes() {
        for pin in remote.pins.iter().filter(|p| p.enabled) {
            pins::cycle(app, notifier, &remote.name, pin, trigger).await;
            scheduler.mark_run(&pin.name, tokio::time::Instant::now());
        }
    }
}

/// Replay queued offline writes: every enabled remote on reconnect, or just
/// `only` when asked for by name (`onedrive-rc outbox retry`).
async fn replay_outbox_all(app: &Arc<App>, notifier: &mut Notifier, only: Option<&str>) {
    let config = app.config();
    let paths = app.paths.clone();

    for remote in config
        .enabled_remotes()
        .filter(|r| only.is_none_or(|name| name == r.name))
    {
        match outbox::replay(app, notifier, remote, &paths, chrono::Utc::now()).await {
            Ok(stats) if stats.uploaded > 0 || stats.conflicted > 0 || stats.failed > 0 => {
                info!(
                    remote = %remote.name,
                    uploaded = stats.uploaded,
                    conflicted = stats.conflicted,
                    failed = stats.failed,
                    "outbox replay finished"
                );
            }
            Ok(_) => {}
            Err(e) => warn!(remote = %remote.name, error = %e, "outbox replay failed"),
        }
    }
}

/// Crawl every enabled remote, unconditionally. What `onedrive-rc index
/// refresh` asks for.
async fn crawl_all(app: &Arc<App>) {
    let config = app.config();
    for remote in config.enabled_remotes() {
        crawl_one(app, remote).await;
    }
}

/// Crawl only the remotes whose `full_crawl_interval` has actually elapsed.
///
/// Staleness is judged against one shared `last_full_crawl` timestamp rather
/// than one per remote — the same simplification `App::status_report` already
/// makes for the same reason: a single-remote setup is the common case, and a
/// per-remote clock is more bookkeeping than today's index actually needs.
async fn crawl_due_remotes(app: &Arc<App>) {
    let config = app.config();
    let Some(shortest) = config
        .enabled_remotes()
        .map(|r| r.index.full_crawl_interval.as_secs())
        .min()
    else {
        return;
    };

    let last_crawl = app
        .with_store(|s| s.index_stats())
        .ok()
        .and_then(|i| i.last_crawl);
    let due = match last_crawl {
        None => true,
        Some(last) => (chrono::Utc::now() - last).num_seconds() > shortest as i64,
    };
    if due {
        crawl_all(app).await;
    }
}

async fn crawl_one(app: &Arc<App>, remote: &onedrive_rc_core::config::RemoteConfig) {
    let subject = format!("index/{}", remote.name);
    match crate::index::crawl_remote(remote, &app.paths).await {
        Ok(nodes) => {
            let result = app.with_store(|s| crate::index::apply_crawl(s, &remote.name, &nodes));
            match result {
                Ok(stats) => {
                    debug!(remote = %remote.name, ?stats, "index crawl complete");
                    app.event(
                        corelog::Level::Info,
                        None,
                        &subject,
                        &format!(
                            "crawl complete: {} files, {} dirs, {} removed",
                            stats.files, stats.dirs, stats.removed
                        ),
                    );
                }
                Err(e) => warn!(remote = %remote.name, error = %e, "could not apply the crawl"),
            }
        }
        Err(e) => {
            let msg = format!("{e:#}");
            warn!(remote = %remote.name, subject = %subject, "{msg}");
            app.event(corelog::Level::Warn, Some(Code::Internal), &subject, &msg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use onedrive_rc_core::VERSION;
    use onedrive_rc_core::config::Config;
    use onedrive_rc_core::paths::Paths;
    use onedrive_rc_core::state::Store;

    use crate::sdnotify::Notifier;

    fn test_app(home: &std::path::Path, config: Config) -> (Arc<App>, mpsc::Receiver<Control>) {
        let paths = Paths::rooted(home);
        let store = Store::open(&paths.state_db()).unwrap();
        store
            .daemon_started(std::process::id(), VERSION, chrono::Utc::now())
            .unwrap();
        let (reload_tx, _reload_rx) = mpsc::channel(1);
        let (control_tx, control_rx) = mpsc::channel(1);
        (
            Arc::new(App::new(
                paths,
                store,
                config,
                None,
                Notifier::from_env(),
                reload_tx,
                control_tx,
            )),
            control_rx,
        )
    }

    #[test]
    fn resume_settles_before_judging_a_mount() {
        // Shorter than the probe timeout, so a resume cannot be mistaken for a
        // hang, but long enough for an interface to come back.
        assert!(RESUME_SETTLE < Duration::from_secs(10));
        assert!(RESUME_SETTLE >= Duration::from_secs(1));
    }

    #[tokio::test]
    async fn reconciling_an_empty_config_reports_starting() {
        let dir = tempfile::tempdir().unwrap();
        let (app, _rx) = test_app(dir.path(), Config::default());
        let mut supervisor = Supervisor::new(app.paths.clone());

        reconcile_now(&app, &mut supervisor).await;

        let daemon = app.with_store(|s| s.daemon()).unwrap().unwrap();
        assert_eq!(daemon.state, DaemonState::Starting);
    }

    /// A configured remote that is not mounted must read as degraded, not
    /// online — that honesty is what makes `status` worth trusting.
    #[tokio::test]
    async fn an_unmounted_remote_marks_the_daemon_degraded() {
        let dir = tempfile::tempdir().unwrap();
        let config: Config = toml::from_str(
            r#"
            [[remote]]
            name = "od"
            mount_point = "~/OneDrive"
            "#,
        )
        .unwrap();
        let (app, _rx) = test_app(dir.path(), config);
        let mut supervisor = Supervisor::new(app.paths.clone());

        // `rclone` will fail for a remote that does not exist in rclone.conf,
        // which is the point: the failure has to be visible.
        reconcile_now(&app, &mut supervisor).await;

        let report = app.status_report().unwrap();
        assert_eq!(report.remotes.len(), 1);
        assert_ne!(
            report.remotes[0].state,
            onedrive_rc_core::ipc::MountState::Mounted,
            "a remote that cannot mount must not claim to be mounted"
        );
    }

    #[tokio::test]
    async fn the_loop_stops_when_told_to() {
        let dir = tempfile::tempdir().unwrap();
        let (app, control_rx) = test_app(dir.path(), Config::default());
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let handle = tokio::spawn(run(app, control_rx, shutdown_rx));
        // Give it a moment to reach its select!, then ask it to stop.
        tokio::time::sleep(Duration::from_millis(200)).await;
        shutdown_tx.send(true).unwrap();

        let stopped = tokio::time::timeout(Duration::from_secs(30), handle).await;
        assert!(stopped.is_ok(), "the mount loop must shut down promptly");
    }

    // -- index crawling ------------------------------------------------------

    /// An app configured with a real local `alias` remote — the same trick
    /// the safety matrix uses: no credentials or network needed, same code
    /// paths as OneDrive. The rclone.conf is written after construction,
    /// against the `Paths` the app itself derived from `home`.
    fn app_with_alias_remote(
        home: &std::path::Path,
        remote_dir: &std::path::Path,
    ) -> (Arc<App>, mpsc::Receiver<Control>) {
        let config: Config = toml::from_str(&format!(
            r#"
            [[remote]]
            name = "stand-in"
            mount_point = "{home}/OneDrive"
            "#,
            home = home.display(),
        ))
        .unwrap();
        let (app, control_rx) = test_app(home, config);

        std::fs::create_dir_all(app.paths.rclone_config().parent().unwrap()).unwrap();
        std::fs::write(
            app.paths.rclone_config(),
            format!(
                "[stand-in]\ntype = alias\nremote = {}\n",
                remote_dir.display()
            ),
        )
        .unwrap();

        (app, control_rx)
    }

    #[tokio::test]
    async fn crawl_all_populates_the_index_for_every_enabled_remote() {
        let dir = tempfile::tempdir().unwrap();
        let remote_dir = dir.path().join("remote");
        std::fs::create_dir_all(&remote_dir).unwrap();
        std::fs::write(remote_dir.join("notes.txt"), "hi").unwrap();

        let (app, _control_rx) = app_with_alias_remote(&dir.path().join("home"), &remote_dir);

        crawl_all(&app).await;

        let stats = app.with_store(|s| s.index_stats()).unwrap();
        assert_eq!(stats.files, 1);
        assert!(stats.last_crawl.is_some());
    }

    #[tokio::test]
    async fn a_never_crawled_remote_is_always_due() {
        let dir = tempfile::tempdir().unwrap();
        let remote_dir = dir.path().join("remote");
        std::fs::create_dir_all(&remote_dir).unwrap();

        let (app, _control_rx) = app_with_alias_remote(&dir.path().join("home"), &remote_dir);

        crawl_due_remotes(&app).await;
        assert!(
            app.with_store(|s| s.index_stats())
                .unwrap()
                .last_crawl
                .is_some()
        );
    }

    #[tokio::test]
    async fn a_recently_crawled_remote_is_not_crawled_again() {
        let dir = tempfile::tempdir().unwrap();
        let remote_dir = dir.path().join("remote");
        std::fs::create_dir_all(&remote_dir).unwrap();
        std::fs::write(remote_dir.join("notes.txt"), "hi").unwrap();

        let (app, _control_rx) = app_with_alias_remote(&dir.path().join("home"), &remote_dir);

        crawl_all(&app).await;
        let first = app.with_store(|s| s.index_stats()).unwrap().last_crawl;

        // A deletion that a second crawl would notice, if one ran.
        std::fs::remove_file(remote_dir.join("notes.txt")).unwrap();
        crawl_due_remotes(&app).await;

        let stats = app.with_store(|s| s.index_stats()).unwrap();
        assert_eq!(stats.last_crawl, first, "the interval has not elapsed yet");
        assert_eq!(stats.files, 1, "so the stale entry must still be there");
    }
}
