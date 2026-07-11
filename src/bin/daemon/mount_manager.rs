use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use onedrive_mount::{
    config::{LogConfig, RemoteConfig},
    paths::expand_tilde,
    status::MountState,
};
use std::collections::HashMap;
use std::time::Duration;
use tokio::process::Child;
use tracing::{error, info, warn};

const RESTART_DELAYS: &[u64] = &[5, 30, 120, 300];

struct MountEntry {
    child: Child,
    mount_point: std::path::PathBuf,
    since: Option<DateTime<Utc>>,
    restart_count: u32,
    restart_not_before: Option<tokio::time::Instant>,
}

pub struct MountManager {
    mounts: HashMap<String, MountEntry>,
    log: LogConfig,
}

impl MountManager {
    pub fn new(log: LogConfig) -> Self {
        Self {
            mounts: HashMap::new(),
            log,
        }
    }

    pub async fn start(&mut self, remote: &RemoteConfig) -> Result<MountState> {
        let mount_point = expand_tilde(&remote.mount_point);

        if let Err(e) = tokio::fs::metadata(&mount_point).await
            && e.raw_os_error() == Some(107)
        {
            warn!(
                remote = %remote.name,
                path = %mount_point.display(),
                "mount point in broken FUSE state (ENOTCONN) — running fusermount -u"
            );
            crate::rclone::fusermount(&mount_point).await;
        }

        tokio::fs::create_dir_all(&mount_point)
            .await
            .context("creating mount point")?;

        if !self.mounts.contains_key(&remote.name) && is_fuse_mounted(&mount_point).await {
            warn!(
                remote = %remote.name,
                path = %mount_point.display(),
                "stale FUSE mount detected — running fusermount -u to clean up"
            );
            crate::rclone::fusermount(&mount_point).await;
        }

        if let Ok(mut entries) = tokio::fs::read_dir(&mount_point).await
            && entries.next_entry().await.ok().flatten().is_some()
        {
            warn!(
                remote = %remote.name,
                path = %mount_point.display(),
                "mount point is not empty — existing files will be hidden while mounted"
            );
        }

        let cmd = crate::rclone::mount_command(remote, &self.log);
        let child = tokio::process::Command::from(cmd)
            .spawn()
            .context("spawning rclone mount")?;

        info!(remote = %remote.name, path = %mount_point.display(), "rclone mount started");
        self.mounts.insert(
            remote.name.clone(),
            MountEntry {
                child,
                mount_point: mount_point.clone(),
                since: None,
                restart_count: 0,
                restart_not_before: None,
            },
        );

        Ok(MountState::Mounting)
    }

    pub async fn stop(&mut self, remote_name: &str) {
        let Some(mut entry) = self.mounts.remove(remote_name) else {
            return;
        };

        if let Err(e) = entry.child.kill().await {
            warn!(remote = %remote_name, error = %e, "kill failed");
        }
        let _ = entry.child.wait().await;

        crate::rclone::fusermount(&entry.mount_point).await;

        info!(remote = %remote_name, "rclone mount stopped");
    }

    pub async fn stop_all(&mut self) {
        let names: Vec<String> = self.mounts.keys().cloned().collect();
        for name in names {
            self.stop(&name).await;
        }
    }

    pub async fn health_check(&mut self, remote: &RemoteConfig) -> MountState {
        let mount_point = expand_tilde(&remote.mount_point);

        if let Some(entry) = self.mounts.get_mut(&remote.name) {
            match entry.child.try_wait() {
                Ok(Some(status)) => {
                    let restart_count = entry.restart_count;
                    let not_before = entry.restart_not_before;
                    warn!(remote = %remote.name, exit_status = %status, restart_count, "rclone mount exited unexpectedly");
                    self.mounts.remove(&remote.name);

                    if let Some(deadline) = not_before
                        && tokio::time::Instant::now() < deadline
                    {
                        let remaining = deadline - tokio::time::Instant::now();
                        warn!(remote = %remote.name, ?remaining, "backing off before remount");
                        return MountState::Failed {
                            error: format!("exited with {status}; backing off"),
                            at: Utc::now(),
                        };
                    }

                    let delay_secs = RESTART_DELAYS
                        .get(restart_count as usize)
                        .copied()
                        .unwrap_or(*RESTART_DELAYS.last().unwrap());
                    let state = self.remount(remote).await;
                    if let Some(entry) = self.mounts.get_mut(&remote.name) {
                        entry.restart_count = restart_count + 1;
                        entry.restart_not_before =
                            Some(tokio::time::Instant::now() + Duration::from_secs(delay_secs));
                    }
                    return state;
                }
                Ok(None) => {
                    if let Some(since) = entry.since {
                        return MountState::Mounted { since };
                    }
                }
                Err(e) => {
                    warn!(remote = %remote.name, error = %e, "could not check mount process status");
                }
            }
        }

        match tokio::fs::metadata(&mount_point).await {
            Ok(meta) if meta.is_dir() => {
                if is_fuse_mounted(&mount_point).await {
                    if let Some(entry) = self.mounts.get_mut(&remote.name) {
                        let is_new = entry.since.is_none();
                        let since = *entry.since.get_or_insert_with(Utc::now);
                        if is_new {
                            info!(remote = %remote.name, "rclone mount is ready");
                            entry.restart_count = 0;
                            entry.restart_not_before = None;
                        }
                        MountState::Mounted { since }
                    } else {
                        MountState::Mounted { since: Utc::now() }
                    }
                } else if self.mounts.contains_key(&remote.name) {
                    MountState::Mounting
                } else {
                    MountState::Unmounted
                }
            }
            _ => {
                if let Some(entry) = self.mounts.get_mut(&remote.name) {
                    match entry.child.try_wait() {
                        Ok(Some(status)) => {
                            warn!(remote = %remote.name, exit_status = %status, "mount point inaccessible and process exited — remounting");
                            self.mounts.remove(&remote.name);
                            self.remount(remote).await
                        }
                        Ok(None) => {
                            error!(remote = %remote.name, "mount point inaccessible — remounting");
                            crate::rclone::fusermount(&mount_point).await;
                            self.mounts.remove(&remote.name);
                            self.remount(remote).await
                        }
                        Err(e) => {
                            warn!(remote = %remote.name, error = %e, "could not check process status — treating as stopped");
                            self.mounts.remove(&remote.name);
                            MountState::Unmounted
                        }
                    }
                } else {
                    MountState::Unmounted
                }
            }
        }
    }

    async fn remount(&mut self, remote: &RemoteConfig) -> MountState {
        info!(remote = %remote.name, "remounting");
        match self.start(remote).await {
            Ok(state) => state,
            Err(e) => {
                error!(remote = %remote.name, error = %e, "remount failed");
                MountState::Failed {
                    error: e.to_string(),
                    at: Utc::now(),
                }
            }
        }
    }
}

async fn is_fuse_mounted(path: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt;

    let mount_dev = match tokio::fs::metadata(path).await {
        Ok(m) => m.dev(),
        Err(_) => return false,
    };
    let parent = path.parent().unwrap_or(path);
    let parent_dev = match tokio::fs::metadata(parent).await {
        Ok(m) => m.dev(),
        Err(_) => return false,
    };
    mount_dev != parent_dev
}
