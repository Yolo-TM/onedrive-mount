// Manages one rclone mount process per remote, including health checks and restarts

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use onedrive_mount::{
    config::{LogConfig, RemoteConfig},
    paths::expand_tilde,
    status::MountState,
};
use std::collections::HashMap;
use tokio::process::Child;
use tracing::{error, info, warn};

struct MountEntry {
    child: Child,
    mount_point: std::path::PathBuf,
    /// The timestamp when this mount process first became healthy (Mounted state).
    /// None while still in the Mounting phase.
    since: Option<DateTime<Utc>>,
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
        tokio::fs::create_dir_all(&mount_point)
            .await
            .context("creating mount point")?;

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
            MountEntry { child, mount_point: mount_point.clone(), since: None },
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
        let _ = crate::rclone::fusermount_command(&entry.mount_point).status();

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

        // Check if the child process has already exited
        if let Some(entry) = self.mounts.get_mut(&remote.name) {
            match entry.child.try_wait() {
                Ok(Some(status)) => {
                    warn!(remote = %remote.name, exit_status = %status, "rclone mount exited unexpectedly");
                    self.mounts.remove(&remote.name);
                    return self.remount(remote).await;
                }
                Ok(None) => {} // still running
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
                        let since = *entry.since.get_or_insert_with(Utc::now);
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
                    // Mountpoint disappeared while the process is still running — stale mount
                    match entry.child.try_wait() {
                        Ok(Some(_)) | Err(_) => {
                            self.mounts.remove(&remote.name);
                            MountState::Unmounted
                        }
                        Ok(None) => {
                            error!(remote = %remote.name, "mount point inaccessible — remounting");
                            let _ = crate::rclone::fusermount_command(&mount_point).status();
                            self.mounts.remove(&remote.name);
                            self.remount(remote).await
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
