// Manages one rclone mount process per remote, including health checks and restarts

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

/// Backoff delays for successive mount restarts: 5s, 30s, 120s, 300s, then cap at 300s.
const RESTART_DELAYS: &[u64] = &[5, 30, 120, 300];

struct MountEntry {
    child: Child,
    mount_point: std::path::PathBuf,
    /// The timestamp when this mount process first became healthy (Mounted state).
    /// None while still in the Mounting phase.
    since: Option<DateTime<Utc>>,
    /// How many times this remote has been restarted by the health checker.
    /// Reset to 0 on a successful mount. Used to compute backoff delay.
    restart_count: u32,
    /// When the next restart is allowed (None = immediately).
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

        // If the mount point is in a broken FUSE state (ENOTCONN — "Transport endpoint
        // is not connected"), create_dir_all will fail. Clean it up first.
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

        // After a crash the FUSE endpoint may still be registered even though
        // the rclone process is gone — rclone will refuse to mount with
        // "directory already mounted". Detect this by checking if the mount
        // point's device ID differs from its parent (i.e. something is mounted
        // there) without an rclone process we know about, and clean it up.
        if !self.mounts.contains_key(&remote.name) && is_fuse_mounted(&mount_point).await {
            warn!(
                remote = %remote.name,
                path = %mount_point.display(),
                "stale FUSE mount detected — running fusermount -u to clean up"
            );
            crate::rclone::fusermount(&mount_point).await;
        }

        // Warn if the mount point already contains files — rclone will mount on top of them,
        // making the existing content temporarily inaccessible.
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
                // restart_count and restart_not_before are set by the caller (health_check)
                // after insertion so that backoff accumulates correctly across crashes.
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

        // After killing rclone the FUSE endpoint is left in a broken state
        // ("Transport endpoint is not connected") until explicitly unmounted.
        // Run fusermount -u to clean it up so the mount point is usable again.
        crate::rclone::fusermount(&entry.mount_point).await;

        info!(remote = %remote_name, "rclone mount stopped");
    }

    pub async fn stop_all(&mut self) {
        let names: Vec<String> = self.mounts.keys().cloned().collect();
        for name in names {
            self.stop(&name).await;
        }
    }

    /// Checks whether the mount point is accessible and updates the status accordingly.
    /// On failure, unmounts the stale fuse entry and restarts the rclone process.
    /// Called periodically from the main loop.
    pub async fn health_check(&mut self, remote: &RemoteConfig) -> MountState {
        let mount_point = expand_tilde(&remote.mount_point);

        // Check process liveness first via try_wait — this is cheap and doesn't
        // touch the FUSE layer, avoiding spurious Attr calls in the rclone log.
        if let Some(entry) = self.mounts.get_mut(&remote.name) {
            match entry.child.try_wait() {
                Ok(Some(status)) => {
                    let restart_count = entry.restart_count;
                    let not_before = entry.restart_not_before;
                    warn!(remote = %remote.name, exit_status = %status, restart_count, "rclone mount exited unexpectedly");
                    self.mounts.remove(&remote.name);

                    // Enforce backoff — if we're not past the cooldown yet, stay failed.
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

                    // Compute next backoff and store on the new entry via remount
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
                    // Process still running — if already confirmed mounted, trust it.
                    // Only stat the mount point while still in Mounting phase to detect
                    // when FUSE becomes ready.
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
                // A FUSE mount is only truly ready when its device ID differs from the parent
                // directory's device ID. Until rclone connects to the remote, the mountpoint
                // is just an ordinary directory on the same filesystem as its parent.
                if is_fuse_mounted(&mount_point).await {
                    // Mount is healthy — record the since timestamp once and keep it stable
                    if let Some(entry) = self.mounts.get_mut(&remote.name) {
                        let is_new = entry.since.is_none();
                        let since = *entry.since.get_or_insert_with(Utc::now);
                        if is_new {
                            info!(remote = %remote.name, "rclone mount is ready");
                            // Reset backoff on first confirmed healthy mount
                            entry.restart_count = 0;
                            entry.restart_not_before = None;
                        }
                        MountState::Mounted { since }
                    } else {
                        MountState::Mounted { since: Utc::now() }
                    }
                } else if self.mounts.contains_key(&remote.name) {
                    // Process is running but FUSE not yet ready
                    MountState::Mounting
                } else {
                    MountState::Unmounted
                }
            }
            _ => {
                if let Some(entry) = self.mounts.get_mut(&remote.name) {
                    // Mountpoint inaccessible while we have a tracked process — stale/broken mount
                    match entry.child.try_wait() {
                        Ok(Some(status)) => {
                            // Process already exited — clean up and remount
                            warn!(remote = %remote.name, exit_status = %status, "mount point inaccessible and process exited — remounting");
                            self.mounts.remove(&remote.name);
                            self.remount(remote).await
                        }
                        Ok(None) => {
                            // Process still running but mount point gone — kill and remount
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

    /// Tears down any existing process and spawns a fresh rclone mount.
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

/// Returns true when `path` is the root of a FUSE mount — i.e. its device ID
/// differs from its parent directory's device ID. This is the reliable way to
/// distinguish "rclone has connected and mounted the remote" from "the mountpoint
/// directory exists but rclone hasn't finished initialising yet".
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
