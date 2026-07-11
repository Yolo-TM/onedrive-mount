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
use tracing::{error, warn};

const MAX_RETRIES: u32 = 3;
const RETRY_BASE_SECS: u64 = 30;

type RuleKey = (String, String);

pub struct SyncScheduler {
    handles: Vec<JoinHandle<()>>,
    cancel: CancellationToken,
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

                let (sync_now_tx, mut sync_now_rx) = mpsc::channel::<()>(1);
                let key = (remote_name.clone(), rule.name.clone());
                self.sync_now_txs.insert(key, sync_now_tx);

                let handle = tokio::spawn(async move {
                    let mut timer = tokio::time::interval(interval);
                    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

                    const ZERO_TRANSFER_WARN_THRESHOLD: u32 = 3;
                    let mut consecutive_zero_transfers: u32 = 0;

                    loop {
                        let next = chrono::Utc::now()
                            + chrono::Duration::from_std(interval).unwrap_or_default();
                        update_next_sync(&status_tx, &remote_name, &rule.name, next);

                        let triggered_manually = tokio::select! {
                            _ = cancel.cancelled() => break,
                            _ = timer.tick() => false,
                            Some(()) = sync_now_rx.recv() => true,
                        };

                        if triggered_manually {
                            tracing::debug!(remote = %remote_name, rule = %rule.name, "manual sync triggered");
                            timer.reset();
                        }

                        let is_blocked = {
                            let s = status_tx.borrow();
                            s.remotes
                                .iter()
                                .find(|r| r.name == remote_name)
                                .and_then(|r| r.sync_rules.iter().find(|sr| sr.name == rule.name))
                                .map(|sr| sr.state.is_blocked())
                                .unwrap_or(false)
                        };
                        if is_blocked {
                            tracing::info!(
                                remote = %remote_name,
                                rule = %rule.name,
                                "skipping sync — rule blocked on unresolved conflicts"
                            );
                            continue;
                        }

                        update_next_sync_clear(&status_tx, &remote_name, &rule.name);
                        status_tx.send_modify(|s| {
                            set_rule_state(s, &remote_name, &rule.name, SyncState::Running, None)
                        });

                        let result = run_with_retry(&remote_name, &rule, &cancel, &status_tx).await;

                        match result {
                            Some(Ok(outcome)) => {
                                tracing::debug!(
                                    remote = %remote_name,
                                    rule = %rule.name,
                                    files = outcome.files_transferred,
                                    bytes = outcome.bytes_transferred,
                                    "sync succeeded"
                                );

                                if outcome.files_transferred == 0 {
                                    consecutive_zero_transfers += 1;
                                    if consecutive_zero_transfers >= ZERO_TRANSFER_WARN_THRESHOLD
                                        && matches!(rule.sync_strategy, onedrive_mount::conflict::SyncStrategy::Bidirectional | onedrive_mount::conflict::SyncStrategy::NewestWins)
                                    {
                                        warn!(
                                            remote = %remote_name,
                                            rule = %rule.name,
                                            consecutive_cycles = consecutive_zero_transfers,
                                            "sync may be stuck — {} consecutive cycles with 0 files transferred",
                                            consecutive_zero_transfers
                                        );
                                    }
                                } else {
                                    consecutive_zero_transfers = 0;
                                }

                                status_tx.send_modify(|s| {
                                    if let Some(remote) =
                                        s.remotes.iter_mut().find(|r| r.name == remote_name)
                                        && let Some(sr) = remote
                                            .sync_rules
                                            .iter_mut()
                                            .find(|r| r.name == rule.name)
                                    {
                                        if !sr.state.is_blocked() {
                                            sr.state = SyncState::Succeeded;
                                        }
                                        sr.last_sync = Some(outcome.at);
                                        sr.files_transferred = Some(outcome.files_transferred);
                                        sr.bytes_transferred = Some(outcome.bytes_transferred);
                                    }
                                });
                            }
                            Some(Err(e)) => {
                                consecutive_zero_transfers = 0;
                                error!(remote = %remote_name, rule = %rule.name, error = %e, "sync failed after retries");
                                status_tx.send_modify(|s| {
                                    set_rule_state(
                                        s,
                                        &remote_name,
                                        &rule.name,
                                        SyncState::Failed {
                                            error: e.to_string(),
                                            at: chrono::Utc::now(),
                                        },
                                        None,
                                    )
                                });
                            }
                            None => {
                                status_tx.send_modify(|s| {
                                    set_rule_state(
                                        s,
                                        &remote_name,
                                        &rule.name,
                                        SyncState::Idle,
                                        None,
                                    )
                                });
                                break;
                            }
                        }
                    }
                });

                self.handles.push(handle);
            }
        }
    }

    pub fn trigger_sync_now(&self, remote_name: &str, rule_name: &str) -> bool {
        let key = (remote_name.to_string(), rule_name.to_string());
        if let Some(tx) = self.sync_now_txs.get(&key) {
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
        self.cancel = CancellationToken::new();
    }
}

async fn run_with_retry(
    remote_name: &str,
    rule: &onedrive_mount::config::SyncRule,
    cancel: &CancellationToken,
    status_tx: &watch::Sender<DaemonStatus>,
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

        match sync_executor::run(remote_name, rule, status_tx).await {
            Ok(outcome) => return Some(Ok(outcome)),
            Err(e) => {
                warn!(remote = %remote_name, rule = %rule.name, error = %e, "sync attempt failed");
                last_err = Some(e);
            }
        }
    }
    Some(Err(last_err.unwrap_or_else(|| {
        anyhow::anyhow!("sync failed with no error details")
    })))
}

fn set_rule_state(
    status: &mut DaemonStatus,
    remote_name: &str,
    rule_name: &str,
    state: SyncState,
    last_sync: Option<chrono::DateTime<chrono::Utc>>,
) {
    if let Some(remote) = status.remotes.iter_mut().find(|r| r.name == remote_name)
        && let Some(rule) = remote.sync_rules.iter_mut().find(|r| r.name == rule_name)
    {
        rule.state = state;
        if let Some(ts) = last_sync {
            rule.last_sync = Some(ts);
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
        if let Some(remote) = s.remotes.iter_mut().find(|r| r.name == remote_name)
            && let Some(rule) = remote.sync_rules.iter_mut().find(|r| r.name == rule_name)
        {
            rule.next_sync = Some(next);
        }
    });
}

fn update_next_sync_clear(
    status_tx: &watch::Sender<DaemonStatus>,
    remote_name: &str,
    rule_name: &str,
) {
    status_tx.send_modify(|s| {
        if let Some(remote) = s.remotes.iter_mut().find(|r| r.name == remote_name)
            && let Some(rule) = remote.sync_rules.iter_mut().find(|r| r.name == rule_name)
        {
            rule.next_sync = None;
        }
    });
}

pub fn parse_interval(s: &str) -> Option<Duration> {
    onedrive_mount::defaults::parse_interval_secs(s).map(Duration::from_secs)
}
