// Spawns one tokio task per sync rule, each sleeping until its configured interval elapses

use crate::sync_executor;
use onedrive_mount::{
    config::RemoteConfig,
    status::{DaemonStatus, SyncState},
};
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

/// Maximum number of consecutive retries before giving up until the next scheduled interval.
const MAX_RETRIES: u32 = 3;
/// Base delay for exponential backoff: 30s, 60s, 120s.
const RETRY_BASE_SECS: u64 = 30;

/// Key identifying a specific sync rule: (remote_name, rule_name)
type RuleKey = (String, String);

pub struct SyncScheduler {
    handles: Vec<JoinHandle<()>>,
    cancel: CancellationToken,
    /// Per-rule channels to trigger an immediate sync without restarting the scheduler
    sync_now_txs: HashMap<RuleKey, mpsc::Sender<()>>,
}

impl SyncScheduler {
    pub fn new() -> Self {
        Self {
            handles: Vec::new(),
            cancel: CancellationToken::new(),
            sync_now_txs: HashMap::new(),
        }
    }

    pub fn start(&mut self, remotes: &[RemoteConfig], status_tx: watch::Sender<DaemonStatus>) {
        for remote in remotes.iter().filter(|r| r.enabled) {
            for rule in remote.sync_rules.iter().filter(|r| r.enabled) {
                let interval = parse_interval(&rule.interval).unwrap_or(Duration::from_secs(900));
                let remote_name = remote.name.clone();
                let rule = rule.clone();
                let status_tx = status_tx.clone();
                let cancel = self.cancel.clone();

                // Channel for immediate sync triggers (Sync Now button)
                let (sync_now_tx, mut sync_now_rx) = mpsc::channel::<()>(1);
                let key = (remote_name.clone(), rule.name.clone());
                self.sync_now_txs.insert(key, sync_now_tx);

                let handle = tokio::spawn(async move {
                    let mut timer = tokio::time::interval(interval);
                    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

                    loop {
                        // Publish next scheduled run time before sleeping
                        let next = chrono::Utc::now() + chrono::Duration::from_std(interval).unwrap_or_default();
                        update_next_sync(&status_tx, &remote_name, &rule.name, next);

                        // Wait for either the interval to fire or a manual trigger
                        let triggered_manually = tokio::select! {
                            _ = cancel.cancelled() => break,
                            _ = timer.tick() => false,
                            Some(()) = sync_now_rx.recv() => true,
                        };

                        if triggered_manually {
                            tracing::debug!(remote = %remote_name, rule = %rule.name, "manual sync triggered");
                            // Reset the timer so the next automatic sync is a full interval away
                            timer.reset();
                        }

                        update_next_sync_clear(&status_tx, &remote_name, &rule.name);
                        status_tx.send_modify(|s| set_rule_state(s, &remote_name, &rule.name, SyncState::Running, None));

                        let result = run_with_retry(&remote_name, &rule, &cancel).await;

                        match result {
                            Some(Ok(outcome)) => {
                                tracing::debug!(remote = %remote_name, rule = %rule.name, "sync succeeded");
                                status_tx.send_modify(|s| set_rule_state(
                                    s, &remote_name, &rule.name,
                                    SyncState::Succeeded,
                                    Some(outcome.at),
                                ));
                            }
                            Some(Err(e)) => {
                                error!(remote = %remote_name, rule = %rule.name, error = %e, "sync failed after retries");
                                status_tx.send_modify(|s| set_rule_state(
                                    s, &remote_name, &rule.name,
                                    SyncState::Failed { error: e.to_string(), at: chrono::Utc::now() },
                                    None,
                                ));
                            }
                            None => break, // cancelled during retry sleep
                        }
                    }
                });

                self.handles.push(handle);
            }
        }
    }

    /// Triggers an immediate sync for a specific rule without affecting other rules.
    /// Returns false if the rule is not currently scheduled.
    pub fn trigger_sync_now(&self, remote_name: &str, rule_name: &str) -> bool {
        let key = (remote_name.to_string(), rule_name.to_string());
        if let Some(tx) = self.sync_now_txs.get(&key) {
            // try_send: if the channel is already full (a trigger is already pending) that's fine
            tx.try_send(()).is_ok()
        } else {
            false
        }
    }

    pub async fn stop(&mut self) {
        self.cancel.cancel();
        for handle in self.handles.drain(..) {
            let _ = handle.await;
        }
        self.sync_now_txs.clear();
        // Fresh token so the scheduler can be restarted after a config reload
        self.cancel = CancellationToken::new();
    }
}

/// Runs the sync, retrying up to MAX_RETRIES times with exponential backoff.
/// Returns None if cancelled during a retry sleep.
async fn run_with_retry(
    remote_name: &str,
    rule: &onedrive_mount::config::SyncRule,
    cancel: &CancellationToken,
) -> Option<anyhow::Result<sync_executor::SyncOutcome>> {
    let mut last_err = None;
    for attempt in 0..=MAX_RETRIES {
        if attempt > 0 {
            let delay = Duration::from_secs(RETRY_BASE_SECS * (1 << (attempt - 1)));
            warn!(remote = %remote_name, rule = %rule.name, attempt, ?delay, "retrying sync after backoff");
            tokio::select! {
                _ = cancel.cancelled() => return None,
                _ = tokio::time::sleep(delay) => {}
            }
        }

        match sync_executor::run(remote_name, rule).await {
            Ok(outcome) => return Some(Ok(outcome)),
            Err(e) => {
                warn!(remote = %remote_name, rule = %rule.name, error = %e, "sync attempt failed");
                last_err = Some(e);
            }
        }
    }
    Some(Err(last_err.unwrap_or_else(|| anyhow::anyhow!("sync failed with no error details"))))
}

fn set_rule_state(
    status: &mut DaemonStatus,
    remote_name: &str,
    rule_name: &str,
    state: SyncState,
    last_sync: Option<chrono::DateTime<chrono::Utc>>,
) {
    if let Some(remote) = status.remotes.iter_mut().find(|r| r.name == remote_name) {
        if let Some(rule) = remote.sync_rules.iter_mut().find(|r| r.name == rule_name) {
            rule.state = state;
            if let Some(ts) = last_sync {
                rule.last_sync = Some(ts);
            }
        }
    }
}

fn update_next_sync(
    status_tx: &watch::Sender<DaemonStatus>,
    remote_name: &str,
    rule_name: &str,
    next: chrono::DateTime<chrono::Utc>,
) {
    status_tx.send_modify(|s| {
        if let Some(remote) = s.remotes.iter_mut().find(|r| r.name == remote_name) {
            if let Some(rule) = remote.sync_rules.iter_mut().find(|r| r.name == rule_name) {
                rule.next_sync = Some(next);
            }
        }
    });
}

fn update_next_sync_clear(
    status_tx: &watch::Sender<DaemonStatus>,
    remote_name: &str,
    rule_name: &str,
) {
    status_tx.send_modify(|s| {
        if let Some(remote) = s.remotes.iter_mut().find(|r| r.name == remote_name) {
            if let Some(rule) = remote.sync_rules.iter_mut().find(|r| r.name == rule_name) {
                rule.next_sync = None;
            }
        }
    });
}

pub fn parse_interval(s: &str) -> Option<Duration> {
    onedrive_mount::defaults::parse_interval_secs(s).map(Duration::from_secs)
}
