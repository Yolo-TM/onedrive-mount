use crate::{
    config_watcher::{ConfigEvent, ConfigWatcher},
    mount_manager::MountManager,
    resolution_executor,
    resolution_watcher::{ResolutionEvent, ResolutionWatcher},
    status_writer,
    sync_scheduler::SyncScheduler,
};
use anyhow::Result;
use chrono::Utc;
use onedrive_mount::{
    config::{Config, RemoteConfig},
    status::{DaemonStatus, MountState, RemoteStatus, SyncRuleStatus, SyncState},
};
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

pub async fn run(config: Config) -> Result<()> {
    let (status_tx, status_rx) = watch::channel(build_initial_status(&config));

    let (config_tx, mut config_rx) = mpsc::channel::<ConfigEvent>(4);
    let _watcher = ConfigWatcher::new(config_tx)?;

    let (resolution_tx, mut resolution_rx) = mpsc::channel::<ResolutionEvent>(4);
    let _resolution_watcher = ResolutionWatcher::new(resolution_tx)?;

    let cancel = CancellationToken::new();
    let status_writer_handle = status_writer::start(status_rx, cancel.clone());

    let mut mount_manager = MountManager::new(config.log.clone());
    let mut scheduler = SyncScheduler::new();

    for remote in config.remotes.iter().filter(|r| r.enabled) {
        startup_remote(&mut mount_manager, &status_tx, remote).await;
    }
    scheduler.start(&config.remotes, status_tx.clone());

    info!("daemon running");

    let mut health_tick = tokio::time::interval(Duration::from_secs(15));
    health_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut sync_now_sig = crate::signal::sync_now_listener();

    let mut current_config = config;
    loop {
        tokio::select! {
            _ = crate::signal::wait_for_shutdown() => {
                info!("shutdown signal received");
                break;
            }
            Some(event) = config_rx.recv() => {
                match event {
                    ConfigEvent::Loaded(new_config) => {
                        status_tx.send_modify(|s| s.config_error = None);
                        reload(&mut mount_manager, &mut scheduler, &status_tx, &current_config, &new_config).await;
                        current_config = new_config;
                    }
                    ConfigEvent::ParseError(msg) => {
                        warn!(error = %msg, "config parse error — keeping previous config");
                        status_tx.send_modify(|s| s.config_error = Some(msg));
                    }
                }
            }
            _ = health_tick.tick() => {
                check_mount_health(&mut mount_manager, &status_tx, &current_config).await;
            }
            _ = crate::signal::wait_for_sync_now(&mut sync_now_sig) => {
                info!("SIGUSR1 received — triggering immediate sync for all rules");
                for remote in current_config.remotes.iter().filter(|r| r.enabled) {
                    for rule in remote.sync_rules.iter().filter(|r| r.enabled) {
                        scheduler.trigger_sync_now(&remote.name, &rule.name);
                    }
                }
            }
            Some(ResolutionEvent::Loaded(rf)) = resolution_rx.recv() => {
                info!(count = rf.resolutions.len(), "processing conflict resolutions");
                let result = resolution_executor::apply(&rf.resolutions, &status_tx).await;

                let remaining = onedrive_mount::resolution::ResolutionFile {
                    resolutions: result.failed,
                };
                if let Err(e) = remaining.save(&onedrive_mount::paths::conflict_resolutions_file()) {
                    warn!(error = %e, "failed to save remaining conflict-resolutions.toml");
                }

                for (remote, rule) in &result.unblocked {
                    info!(remote = %remote, rule = %rule, "re-triggering sync after conflict resolution");
                    scheduler.trigger_sync_now(remote, rule);
                }
            }
        }
    }

    scheduler.stop().await;
    mount_manager.stop_all().await;
    cancel.cancel();
    let _ = status_writer_handle.await;

    Ok(())
}

async fn startup_remote(
    mounts: &mut MountManager,
    status_tx: &watch::Sender<DaemonStatus>,
    remote: &RemoteConfig,
) {
    let state = match mounts.start(remote).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(remote = %remote.name, error = %e, "failed to start mount");
            MountState::Failed {
                error: e.to_string(),
                at: Utc::now(),
            }
        }
    };

    status_tx.send_modify(|s| {
        if let Some(rs) = s.remotes.iter_mut().find(|r| r.name == remote.name) {
            rs.mount = state;
        }
    });
}

async fn reload(
    mounts: &mut MountManager,
    scheduler: &mut SyncScheduler,
    status_tx: &watch::Sender<DaemonStatus>,
    old: &Config,
    new: &Config,
) {
    info!("config reloaded — applying changes");

    scheduler.stop().await;

    for old_remote in &old.remotes {
        if !new.remotes.iter().any(|r| r.name == old_remote.name) {
            info!(remote = %old_remote.name, "remote removed — unmounting");
            mounts.stop(&old_remote.name).await;
            status_tx.send_modify(|s| s.remotes.retain(|r| r.name != old_remote.name));
        }
    }

    for new_remote in &new.remotes {
        if !new_remote.enabled {
            let was_enabled = old
                .remotes
                .iter()
                .find(|r| r.name == new_remote.name)
                .map(|r| r.enabled)
                .unwrap_or(false);
            if was_enabled {
                info!(remote = %new_remote.name, "remote disabled — unmounting");
            }
            mounts.stop(&new_remote.name).await;
            status_tx.send_modify(|s| {
                if let Some(rs) = s.remotes.iter_mut().find(|r| r.name == new_remote.name) {
                    rs.mount = MountState::Unmounted;
                }
            });
            continue;
        }

        let old_remote_entry = old.remotes.iter().find(|r| r.name == new_remote.name);
        let is_new = old_remote_entry.is_none();
        let was_disabled = old_remote_entry.map(|r| !r.enabled).unwrap_or(false);
        let changed = old_remote_entry
            .map(|old_remote| remote_config_changed(old_remote, new_remote))
            .unwrap_or(false);

        if is_new {
            info!(remote = %new_remote.name, "remote added — mounting");
            status_tx.send_modify(|s| {
                if !s.remotes.iter().any(|r| r.name == new_remote.name) {
                    s.remotes.push(RemoteStatus {
                        name: new_remote.name.clone(),
                        mount: MountState::Unmounted,
                        sync_rules: new_remote
                            .sync_rules
                            .iter()
                            .filter(|r| r.enabled)
                            .map(|r| SyncRuleStatus {
                                name: r.name.clone(),
                                last_sync: None,
                                next_sync: None,
                                state: SyncState::Idle,
                                files_transferred: None,
                                bytes_transferred: None,
                                conflicts: vec![],
                            })
                            .collect(),
                    });
                }
            });
            startup_remote(mounts, status_tx, new_remote).await;
        } else if was_disabled {
            info!(remote = %new_remote.name, "remote re-enabled — mounting");
            startup_remote(mounts, status_tx, new_remote).await;
        } else if changed {
            info!(remote = %new_remote.name, "remote config changed — remounting");
            mounts.stop(&new_remote.name).await;
            startup_remote(mounts, status_tx, new_remote).await;
        }

        if let Some(old_remote) = old.remotes.iter().find(|r| r.name == new_remote.name) {
            for old_rule in &old_remote.sync_rules {
                if !new_remote
                    .sync_rules
                    .iter()
                    .any(|r| r.name == old_rule.name)
                {
                    info!(remote = %new_remote.name, rule = %old_rule.name, "sync rule removed");
                }
            }
            for new_rule in &new_remote.sync_rules {
                let old_rule = old_remote
                    .sync_rules
                    .iter()
                    .find(|r| r.name == new_rule.name);
                match old_rule {
                    None => {
                        info!(remote = %new_remote.name, rule = %new_rule.name, "sync rule added")
                    }
                    Some(old) if !old.enabled && new_rule.enabled => {
                        info!(remote = %new_remote.name, rule = %new_rule.name, "sync rule enabled")
                    }
                    Some(old) if old.enabled && !new_rule.enabled => {
                        info!(remote = %new_remote.name, rule = %new_rule.name, "sync rule disabled")
                    }
                    _ => {}
                }
            }
        }
    }

    status_tx.send_modify(|s| {
        for remote in &new.remotes {
            if let Some(rs) = s.remotes.iter_mut().find(|r| r.name == remote.name) {
                rs.sync_rules = remote
                    .sync_rules
                    .iter()
                    .filter(|r| r.enabled)
                    .map(|r| {
                        let old_status = rs
                            .sync_rules
                            .iter()
                            .find(|s| s.name == r.name);
                        SyncRuleStatus {
                            name: r.name.clone(),
                            last_sync: old_status.and_then(|s| s.last_sync),
                            next_sync: None,
                            state: old_status
                                .filter(|s| s.state.is_blocked())
                                .map(|s| s.state.clone())
                                .unwrap_or(SyncState::Idle),
                            files_transferred: old_status
                                .and_then(|s| s.files_transferred),
                            bytes_transferred: old_status
                                .and_then(|s| s.bytes_transferred),
                            conflicts: old_status
                                .map(|s| s.conflicts.clone())
                                .unwrap_or_default(),
                        }
                    })
                    .collect();
            }
        }
    });

    scheduler.start(&new.remotes, status_tx.clone());
}

async fn check_mount_health(
    mounts: &mut MountManager,
    status_tx: &watch::Sender<DaemonStatus>,
    config: &Config,
) {
    for remote in &config.remotes {
        let state = mounts.health_check(remote).await;
        status_tx.send_modify(|s| {
            if let Some(rs) = s.remotes.iter_mut().find(|r| r.name == remote.name)
                && rs.mount != state
            {
                rs.mount = state;
            }
        });
    }
}

fn remote_config_changed(old: &RemoteConfig, new: &RemoteConfig) -> bool {
    old.name != new.name
        || old.mount_point != new.mount_point
        || old.r#type != new.r#type
        || old.poll_interval != new.poll_interval
        || old.mount.vfs_cache_mode != new.mount.vfs_cache_mode
        || old.mount.vfs_cache_max_size != new.mount.vfs_cache_max_size
        || old.mount.vfs_cache_max_age != new.mount.vfs_cache_max_age
        || old.mount.vfs_write_back != new.mount.vfs_write_back
        || old.mount.dir_cache_time != new.mount.dir_cache_time
        || old.mount.transfers != new.mount.transfers
        || old.mount.extra_flags != new.mount.extra_flags
}

fn build_initial_status(config: &Config) -> DaemonStatus {
    let prev = onedrive_mount::status::DaemonStatus::load(&onedrive_mount::paths::status_file());

    DaemonStatus {
        pid: std::process::id(),
        started_at: Some(Utc::now()),
        version: env!("CARGO_PKG_VERSION").to_string(),
        config_error: None,
        remotes: config
            .remotes
            .iter()
            .map(|r| RemoteStatus {
                name: r.name.clone(),
                mount: MountState::Unmounted,
                sync_rules: r
                    .sync_rules
                    .iter()
                    .filter(|rule| rule.enabled)
                    .map(|rule| {
                        let prev_rule = prev.as_ref().and_then(|p| {
                            p.remotes
                                .iter()
                                .find(|pr| pr.name == r.name)
                                .and_then(|pr| pr.sync_rules.iter().find(|sr| sr.name == rule.name))
                        });
                        let (state, conflicts) = match prev_rule {
                            Some(pr) if pr.state.is_blocked() => {
                                (pr.state.clone(), pr.conflicts.clone())
                            }
                            _ => (SyncState::Idle, vec![]),
                        };
                        SyncRuleStatus {
                            name: rule.name.clone(),
                            last_sync: prev_rule.and_then(|pr| pr.last_sync),
                            next_sync: None,
                            state,
                            files_transferred: prev_rule.and_then(|pr| pr.files_transferred),
                            bytes_transferred: prev_rule.and_then(|pr| pr.bytes_transferred),
                            conflicts,
                        }
                    })
                    .collect(),
            })
            .collect(),
    }
}
