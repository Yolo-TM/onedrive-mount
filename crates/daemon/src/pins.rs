//! The pin scheduler: one sync cycle, and when to run it.
//!
//! A cycle is always the same three steps in the same order, and the order is
//! the point:
//!
//! 1. **snapshot** — before anything can be overwritten or deleted;
//! 2. **bisync** — the transport;
//! 3. **reconcile** — establish the invariants rclone does not promise.
//!
//! Triggers are deliberately plural. The interval is a floor, not the mechanism:
//! a database save should reach OneDrive in seconds, and the most effective way
//! to avoid conflicts is to not leave two sides diverged for long. So a cycle
//! also runs on local change (debounced), on screen lock, before suspend, after
//! resume, and on reconnect.
//!
//! One cycle per pin at a time, enforced by owning the state here rather than by
//! a lock: the old scheduler spawned a task per rule with no mutual exclusion,
//! so two overlapping runs could copy the same tree in opposite directions.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use onedrive_core::config::{Config, PinConfig};
use onedrive_core::state::PinState;
use onedrive_core::{Code, log as corelog};
use tracing::{debug, info, warn};

use crate::app::App;
use crate::bisync::{self, Outcome};
use crate::notify::{Alert, Notifier};
use crate::{reconcile, snapshot};

/// Debounce window for local changes.
///
/// A password manager writes a temp file and renames; an editor may write
/// several times in a row. Syncing on the first event would upload a
/// half-written file, so wait for the writes to stop.
pub const DEBOUNCE: Duration = Duration::from_secs(5);

/// Why a cycle is running. Only used for the log, but it is the thing you want
/// to know when reading one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    Interval,
    LocalChange,
    ScreenLock,
    Suspend,
    Resume,
    Reconnect,
    Manual,
}

impl Trigger {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Interval => "interval",
            Self::LocalChange => "local change",
            Self::ScreenLock => "screen lock",
            Self::Suspend => "suspend",
            Self::Resume => "resume",
            Self::Reconnect => "reconnect",
            Self::Manual => "manual",
        }
    }
}

/// Tracks when each pin last ran, so the interval can be a floor rather than a
/// fixed tick.
#[derive(Debug, Default)]
pub struct Scheduler {
    last_run: HashMap<String, tokio::time::Instant>,
    pending: HashMap<String, tokio::time::Instant>,
}

impl Scheduler {
    /// Pins whose interval has elapsed.
    pub fn due(&self, config: &Config, now: tokio::time::Instant) -> Vec<(String, String)> {
        let mut due = Vec::new();
        for remote in config.enabled_remotes() {
            for pin in remote.pins.iter().filter(|p| p.enabled) {
                let elapsed = self
                    .last_run
                    .get(&pin.name)
                    .map(|t| now.duration_since(*t))
                    // Never run: due immediately.
                    .unwrap_or(Duration::MAX);
                if elapsed >= pin.interval.as_duration() {
                    due.push((remote.name.clone(), pin.name.clone()));
                }
            }
        }
        due
    }

    /// Note that a local change happened; the cycle runs once writes settle.
    pub fn note_change(&mut self, pin: &str, now: tokio::time::Instant) {
        self.pending.insert(pin.to_owned(), now);
    }

    /// Pins whose debounce window has expired.
    pub fn debounced(&mut self, now: tokio::time::Instant) -> Vec<String> {
        let ready: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, at)| now.duration_since(**at) >= DEBOUNCE)
            .map(|(name, _)| name.clone())
            .collect();
        for name in &ready {
            self.pending.remove(name);
        }
        ready
    }

    pub fn mark_run(&mut self, pin: &str, now: tokio::time::Instant) {
        self.last_run.insert(pin.to_owned(), now);
        // A completed cycle covers any change that arrived before it.
        self.pending.remove(pin);
    }

    /// Forget pins that are no longer configured.
    pub fn retain(&mut self, keep: &[String]) {
        self.last_run.retain(|name, _| keep.contains(name));
        self.pending.retain(|name, _| keep.contains(name));
    }
}

/// Run one full cycle for a pin.
///
/// Returns whether anything changed that is worth reporting.
pub async fn cycle(
    app: &Arc<App>,
    notifier: &mut Notifier,
    remote: &str,
    pin: &PinConfig,
    trigger: Trigger,
) {
    let subject = format!("pin/{}", pin.name);
    let local = pin.local_path(&app.paths);
    let now = Utc::now();

    debug!(pin = %pin.name, trigger = trigger.as_str(), "sync cycle starting");
    app.set_pin_state(&pin.name, PinState::Syncing, None);

    // 1. Snapshot first. Anything bisync might overwrite or delete has to be
    //    recoverable before it runs, not after.
    match snapshot::take(pin, &local, now).await {
        Ok(taken) if taken.files > 0 => {
            debug!(
                pin = %pin.name,
                subject = %subject,
                files = taken.files,
                "snapshotted before syncing"
            );
        }
        Ok(_) => {}
        // A failed snapshot is a reason to be careful, not to skip the sync:
        // declining to sync would leave the two sides diverged, which is what
        // causes conflicts in the first place.
        Err(e) => warn!(
            pin = %pin.name,
            subject = %subject,
            error = %format!("{e:#}"),
            "could not snapshot before syncing; continuing"
        ),
    }

    // 2. The transport.
    let outcome = match bisync::run(
        pin,
        remote,
        &app.paths,
        app.config().daemon.rclone_log_level,
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(e) => {
            let msg = format!("{e:#}");
            warn!(pin = %pin.name, subject = %subject, "{msg}");
            app.set_pin_state(&pin.name, PinState::Failed, Some((Code::Internal, &msg)));
            return;
        }
    };

    // 3. Establish the invariants rclone does not promise. Runs whatever the
    //    outcome: a refused or failed run can still have left conflict files
    //    from an earlier partial run, and the canonical name must exist either
    //    way.
    let conflicts = match reconcile::run(&local, now).await {
        Ok(conflicts) => conflicts,
        Err(e) => {
            let msg = format!("reconciliation failed: {e:#}");
            warn!(pin = %pin.name, subject = %subject, "{msg}");
            app.set_pin_state(&pin.name, PinState::Failed, Some((Code::Internal, &msg)));
            return;
        }
    };

    if let Err(e) = snapshot::prune(pin, &local, now).await {
        debug!(pin = %pin.name, error = %e, "snapshot pruning failed");
    }

    record(app, notifier, remote, pin, trigger, outcome, conflicts).await;
}

/// Turn the outcome into state, log lines, events and alerts.
async fn record(
    app: &Arc<App>,
    notifier: &mut Notifier,
    remote: &str,
    pin: &PinConfig,
    trigger: Trigger,
    outcome: Outcome,
    conflicts: Vec<reconcile::Conflict>,
) {
    let subject = format!("pin/{}", pin.name);
    let local = pin.local_path(&app.paths);
    let folder = local.to_string_lossy().into_owned();

    // Conflicts first: they outlive any single run, and they are what the user
    // has to act on.
    if !conflicts.is_empty() {
        announce_conflicts(app, notifier, remote, pin, &conflicts, &folder).await;
    }

    match outcome {
        Outcome::Ok { stats, resynced } => {
            if conflicts.is_empty() {
                app.set_pin_state(&pin.name, PinState::InSync, None);
            } else {
                // Still usable — both versions are on disk and the canonical
                // name exists — but it needs a human, so do not claim in-sync.
                app.set_pin_state(&pin.name, PinState::Conflicted, None);
            }

            if stats.transfers > 0 || stats.deletes > 0 || resynced || !conflicts.is_empty() {
                info!(
                    pin = %pin.name,
                    subject = %subject,
                    trigger = trigger.as_str(),
                    transfers = stats.transfers,
                    deletes = stats.deletes,
                    conflicts = conflicts.len(),
                    resynced,
                    "sync complete"
                );
            } else {
                debug!(pin = %pin.name, subject = %subject, "nothing to do");
            }
        }

        // A refusal is the safety machinery working. It will not clear on its
        // own, so it is surfaced as prominently as a conflict.
        Outcome::Refused { code, msg } => {
            warn!(pin = %pin.name, subject = %subject, code = code.as_str(), "{msg}");
            app.set_pin_state(&pin.name, PinState::Failed, Some((code, &msg)));
            app.event(corelog::Level::Warn, Some(code), &subject, &msg);

            let alert = Alert::Refused {
                pin: pin.name.clone(),
                msg: msg.clone(),
                folder,
            };
            if let Err(e) = notifier.show(&alert, None).await {
                debug!(error = %e, "could not show a notification");
            }
        }

        Outcome::Failed { code, msg } => {
            app.set_pin_state(&pin.name, PinState::Failed, Some((code, &msg)));
            // Being offline is expected and self-correcting, so it is noted
            // rather than warned about — this is most of what filled the old log.
            if code == Code::Offline {
                debug!(pin = %pin.name, subject = %subject, code = code.as_str(), "{msg}");
            } else {
                warn!(pin = %pin.name, subject = %subject, code = code.as_str(), "{msg}");
                app.event(corelog::Level::Warn, Some(code), &subject, &msg);
            }
        }
    }
}

/// Record conflicts and raise a notification that will keep coming back.
async fn announce_conflicts(
    app: &Arc<App>,
    notifier: &mut Notifier,
    remote: &str,
    pin: &PinConfig,
    conflicts: &[reconcile::Conflict],
    folder: &str,
) {
    // Group by the canonical file, because that is what the user thinks about —
    // one database with two versions, not two separate problems.
    let mut by_file: HashMap<&str, Vec<&reconcile::Conflict>> = HashMap::new();
    for conflict in conflicts {
        by_file
            .entry(conflict.path.as_str())
            .or_default()
            .push(conflict);
    }

    for (file, group) in by_file {
        let copies: Vec<String> = group.iter().map(|c| c.copy.clone()).collect();
        let restored = group.iter().any(|c| c.restored_canonical);

        // One row per (pin, file); a second conflict on the same file updates
        // the existing row rather than piling up.
        let existing = app
            .with_store(|s| s.open_conflict_for(&pin.name, file))
            .unwrap_or(None);

        let id = match existing {
            Some(id) => id,
            None => {
                match app.with_store(|s| {
                    s.insert_conflict(
                        "pin",
                        remote,
                        &pin.name,
                        file,
                        Some(file),
                        copies.first().map(String::as_str),
                        Some(folder),
                        Utc::now(),
                    )
                }) {
                    Ok(id) => id,
                    Err(e) => {
                        warn!(error = %e, "could not record a conflict");
                        continue;
                    }
                }
            }
        };

        app.event(
            corelog::Level::Warn,
            Some(Code::Conflict),
            &format!("pin/{}", pin.name),
            &format!("{file}: both versions kept ({})", copies.join(", ")),
        );

        let alert = Alert::Conflict {
            pin: pin.name.clone(),
            file: file.to_owned(),
            copies,
            folder: folder.to_owned(),
            restored,
        };

        // Reuse the stored notification id so a restart keeps replacing the same
        // notification instead of adding another.
        let previous = app
            .with_store(|s| s.open_conflicts())
            .ok()
            .and_then(|rows| {
                rows.into_iter()
                    .find(|r| r.id == id)
                    .and_then(|r| r.notif_id)
            });

        match notifier.show(&alert, previous).await {
            Ok(Some(notif_id)) => {
                if let Err(e) = app.with_store(|s| s.set_conflict_notification(id, Some(notif_id)))
                {
                    debug!(error = %e, "could not store the notification id");
                }
            }
            Ok(None) => {}
            Err(e) => debug!(error = %e, "could not show a conflict notification"),
        }
    }
}

/// Re-assert notifications for conflicts that are still open.
///
/// This is what makes the alert persist: dismissing it does not make the problem
/// invisible, because it comes back until the conflict is actually resolved.
pub async fn reassert(app: &Arc<App>, notifier: &mut Notifier) {
    let Ok(open) = app.with_store(|s| s.open_conflicts()) else {
        return;
    };
    if open.is_empty() {
        return;
    }

    debug!(count = open.len(), "re-asserting conflict notifications");
    for row in open {
        let alert = Alert::Conflict {
            pin: row.subject.clone(),
            file: row.path.clone(),
            copies: row.loser_copy.clone().into_iter().collect(),
            folder: row.version_dir.clone().unwrap_or_default(),
            restored: false,
        };
        match notifier.show(&alert, row.notif_id).await {
            Ok(Some(id)) if Some(id) != row.notif_id => {
                let _ = app.with_store(|s| s.set_conflict_notification(row.id, Some(id)));
            }
            _ => {}
        }
    }
}

/// Resolve conflicts whose extra copies the user has dealt with.
///
/// Deliberately conservative: a conflict is only closed when the copy is gone
/// *and* the canonical file is present. Closing it while the copy still exists
/// would withdraw the notification for a problem that is still there.
pub async fn resolve_finished(app: &Arc<App>, notifier: &mut Notifier) {
    withdraw_resolved(app, notifier).await;

    let Ok(open) = app.with_store(|s| s.open_conflicts()) else {
        return;
    };

    for row in open {
        let Some(copy) = &row.loser_copy else {
            continue;
        };
        let folder = row.version_dir.clone().unwrap_or_default();
        if folder.is_empty() {
            continue;
        }
        let copy_path = Path::new(&folder).join(copy);
        let canonical = Path::new(&folder).join(&row.path);

        if copy_path.exists() || !canonical.exists() {
            continue;
        }

        info!(
            pin = %row.subject,
            file = %row.path,
            "conflict resolved: the extra copy is gone and the original is in place"
        );
        let _ = app.with_store(|s| s.resolve_conflict(row.id, Utc::now()));
        if let Some(id) = row.notif_id {
            let key = format!("conflict:{}:{}", row.subject, row.path);
            if let Err(e) = notifier.close(&key, id).await {
                debug!(error = %e, "could not withdraw a notification");
            }
        }
    }
}

/// Take down notifications for conflicts that were resolved somewhere else.
///
/// `onedrive-tui conflicts resolve` closes the row synchronously so it can
/// report a real result, but only this task holds the D-Bus connection. The
/// `notif_id` left on the resolved row is the handover; clearing it is what
/// makes this run once per resolution rather than every tick.
async fn withdraw_resolved(app: &Arc<App>, notifier: &mut Notifier) {
    let Ok(rows) = app.with_store(|s| s.conflicts_awaiting_withdrawal()) else {
        return;
    };

    for (id, subject, path, notif_id) in rows {
        let key = format!("conflict:{subject}:{path}");
        if let Err(e) = notifier.close(&key, notif_id).await {
            debug!(error = %e, "could not withdraw a notification");
        }
        // Cleared whether or not the close succeeded: a notification we cannot
        // withdraw is not a reason to keep retrying every two seconds.
        let _ = app.with_store(|s| s.set_conflict_notification(id, None));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use onedrive_core::config::Config;

    fn config(interval: &str) -> Config {
        toml::from_str(&format!(
            r#"
            [[remote]]
            name = "od"
            mount_point = "~/OneDrive"
            [[remote.pin]]
            name = "Keepass"
            remote_path = "Dokumente/Keepass"
            local_path = "~/onedrive-keepass"
            interval = "{interval}"
            "#
        ))
        .unwrap()
    }

    #[test]
    fn a_pin_that_has_never_run_is_due_immediately() {
        let s = Scheduler::default();
        let due = s.due(&config("5m"), tokio::time::Instant::now());
        assert_eq!(due, vec![("od".to_string(), "Keepass".to_string())]);
    }

    #[test]
    fn the_interval_is_a_floor() {
        let mut s = Scheduler::default();
        let config = config("5m");
        let t0 = tokio::time::Instant::now();

        s.mark_run("Keepass", t0);
        assert!(s.due(&config, t0).is_empty(), "just ran");
        assert!(
            s.due(&config, t0 + Duration::from_secs(299)).is_empty(),
            "not yet"
        );
        assert_eq!(s.due(&config, t0 + Duration::from_secs(300)).len(), 1);
    }

    #[test]
    fn a_disabled_pin_is_never_due() {
        let config: Config = toml::from_str(
            r#"
            [[remote]]
            name = "od"
            mount_point = "~/OneDrive"
            [[remote.pin]]
            name = "Off"
            remote_path = "a"
            local_path = "~/a"
            enabled = false
            "#,
        )
        .unwrap();
        assert!(
            Scheduler::default()
                .due(&config, tokio::time::Instant::now())
                .is_empty()
        );
    }

    #[test]
    fn a_pin_on_a_disabled_remote_is_never_due() {
        let config: Config = toml::from_str(
            r#"
            [[remote]]
            name = "od"
            mount_point = "~/OneDrive"
            enabled = false
            [[remote.pin]]
            name = "On"
            remote_path = "a"
            local_path = "~/a"
            "#,
        )
        .unwrap();
        assert!(
            Scheduler::default()
                .due(&config, tokio::time::Instant::now())
                .is_empty()
        );
    }

    /// A password manager writes a temp file then renames, and an editor may
    /// write several times. Syncing on the first event would upload a
    /// half-written file.
    #[test]
    fn local_changes_are_debounced() {
        let mut s = Scheduler::default();
        let t0 = tokio::time::Instant::now();

        s.note_change("Keepass", t0);
        assert!(s.debounced(t0).is_empty(), "not immediately");
        assert!(
            s.debounced(t0 + DEBOUNCE - Duration::from_millis(1))
                .is_empty(),
            "not quite yet"
        );
        assert_eq!(s.debounced(t0 + DEBOUNCE), vec!["Keepass".to_string()]);
        assert!(
            s.debounced(t0 + DEBOUNCE * 2).is_empty(),
            "and only fires once"
        );
    }

    /// A burst of writes should extend the window, not fire on the first.
    #[test]
    fn a_burst_of_writes_collapses_into_one_sync() {
        let mut s = Scheduler::default();
        let t0 = tokio::time::Instant::now();

        for i in 0..10 {
            s.note_change("Keepass", t0 + Duration::from_millis(i * 400));
        }
        let last = t0 + Duration::from_millis(9 * 400);

        assert!(
            s.debounced(last).is_empty(),
            "the window restarts on each write"
        );
        assert_eq!(s.debounced(last + DEBOUNCE).len(), 1, "then fires once");
    }

    #[test]
    fn a_completed_run_clears_a_pending_change() {
        let mut s = Scheduler::default();
        let t0 = tokio::time::Instant::now();

        s.note_change("Keepass", t0);
        s.mark_run("Keepass", t0 + Duration::from_secs(1));
        assert!(
            s.debounced(t0 + DEBOUNCE * 2).is_empty(),
            "the run already covered that change"
        );
    }

    #[test]
    fn removed_pins_are_forgotten() {
        let mut s = Scheduler::default();
        let t0 = tokio::time::Instant::now();
        s.mark_run("Keepass", t0);
        s.note_change("Other", t0);

        s.retain(&["Keepass".to_string()]);
        assert!(s.last_run.contains_key("Keepass"));
        assert!(!s.pending.contains_key("Other"));
    }

    #[test]
    fn triggers_name_themselves_for_the_log() {
        for (trigger, text) in [
            (Trigger::Interval, "interval"),
            (Trigger::LocalChange, "local change"),
            (Trigger::ScreenLock, "screen lock"),
            (Trigger::Suspend, "suspend"),
            (Trigger::Resume, "resume"),
            (Trigger::Reconnect, "reconnect"),
            (Trigger::Manual, "manual"),
        ] {
            assert_eq!(trigger.as_str(), text);
        }
    }

    #[test]
    fn the_debounce_is_long_enough_for_a_save_but_short_enough_to_be_prompt() {
        assert!(
            DEBOUNCE >= Duration::from_secs(2),
            "a save must finish first"
        );
        assert!(
            DEBOUNCE <= Duration::from_secs(15),
            "but a database save should reach OneDrive promptly"
        );
    }
}
