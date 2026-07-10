// Applies conflict resolution decisions from the GUI.
// Each resolution either copies a file in one direction or renames the local copy.

use anyhow::{Context, Result};
use chrono::Utc;
use onedrive_mount::{
    resolution::{Resolution, ResolutionAction},
    status::{ConflictEntry, DaemonStatus, SyncState},
};
use tokio::sync::watch;
use tracing::{error, info, warn};



/// Apply a batch of resolutions. Returns the set of (remote, rule) pairs that were unblocked.
pub async fn apply(
    resolutions: &[Resolution],
    status_tx: &watch::Sender<DaemonStatus>,
) -> Vec<(String, String)> {
    let mut unblocked = Vec::new();

    for res in resolutions {
        info!(
            remote = %res.remote,
            rule = %res.rule,
            file = %res.file,
            action = ?res.action,
            "applying conflict resolution"
        );

        // Find the conflict entry from status to get paths
        let conflict = {
            let s = status_tx.borrow();
            s.remotes
                .iter()
                .find(|r| r.name == res.remote)
                .and_then(|r| r.sync_rules.iter().find(|sr| sr.name == res.rule))
                .and_then(|sr| sr.conflicts.iter().find(|c| c.file == res.file))
                .cloned()
        };

        let Some(conflict) = conflict else {
            warn!(
                remote = %res.remote,
                rule = %res.rule,
                file = %res.file,
                "conflict not found in status — may have been resolved already"
            );
            continue;
        };

        match apply_one(res, &conflict).await {
            Ok(()) => {
                info!(
                    remote = %res.remote,
                    rule = %res.rule,
                    file = %res.file,
                    "resolution applied successfully"
                );
                // Remove conflict from status
                status_tx.send_modify(|s| {
                    if let Some(remote) = s.remotes.iter_mut().find(|r| r.name == res.remote)
                        && let Some(rule) = remote.sync_rules.iter_mut().find(|sr| sr.name == res.rule)
                    {
                        rule.conflicts.retain(|c| c.file != res.file);
                        // If all conflicts resolved, unblock the rule
                        if rule.conflicts.is_empty() && rule.state.is_blocked() {
                            rule.state = SyncState::Idle;
                            info!(
                                remote = %res.remote,
                                rule = %res.rule,
                                "all conflicts resolved — rule unblocked"
                            );
                        }
                    }
                });
            }
            Err(e) => {
                error!(
                    remote = %res.remote,
                    rule = %res.rule,
                    file = %res.file,
                    error = %e,
                    "failed to apply resolution"
                );
            }
        }
    }

    // Collect unblocked rules for re-triggering
    let s = status_tx.borrow();
    for res in resolutions {
        if let Some(remote) = s.remotes.iter().find(|r| r.name == res.remote)
            && let Some(rule) = remote.sync_rules.iter().find(|sr| sr.name == res.rule)
            && !rule.state.is_blocked()
            && !unblocked.iter().any(|(r, n)| r == &res.remote && n == &res.rule)
        {
            unblocked.push((res.remote.clone(), res.rule.clone()));
        }
    }

    unblocked
}

async fn apply_one(res: &Resolution, conflict: &ConflictEntry) -> Result<()> {
    match res.action {
        ResolutionAction::KeepLocal => {
            // Copy local → remote (overwrite remote with local version)
            info!(
                file = %res.file,
                local = %conflict.local_path,
                remote = %conflict.remote_path,
                "keep_local: copying local to remote"
            );
            run_copy(&conflict.local_path, &conflict.remote_path).await?;
        }
        ResolutionAction::KeepRemote => {
            // Copy remote → local (overwrite local with remote version)
            info!(
                file = %res.file,
                local = %conflict.local_path,
                remote = %conflict.remote_path,
                "keep_remote: copying remote to local"
            );
            run_copy(&conflict.remote_path, &conflict.local_path).await?;
        }
        ResolutionAction::KeepBoth => {
            // Rename local to .conflict-<timestamp>, then copy remote → local
            let local = std::path::Path::new(&conflict.local_path);
            let ts = Utc::now().format("%Y%m%dT%H%M%S");
            let stem = local
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy();
            let ext = local
                .extension()
                .map(|e| format!(".{}", e.to_string_lossy()))
                .unwrap_or_default();
            let conflict_name = format!("{}.conflict-{}{}", stem, ts, ext);
            let conflict_path = local.parent().unwrap_or(local).join(&conflict_name);

            info!(
                file = %res.file,
                from = %local.display(),
                to = %conflict_path.display(),
                "keep_both: renaming local copy"
            );
            tokio::fs::rename(local, &conflict_path)
                .await
                .context("renaming local file for keep_both")?;

            info!(
                file = %res.file,
                remote = %conflict.remote_path,
                local = %conflict.local_path,
                "keep_both: copying remote to local"
            );
            run_copy(&conflict.remote_path, &conflict.local_path).await?;
        }
    }
    Ok(())
}

/// Run a single-file rclone copy between two paths.
async fn run_copy(src: &str, dst: &str) -> Result<()> {
    // For single-file resolution, use rclone copyto (copies a single file to a destination path)
    let mut cmd = std::process::Command::new("rclone");
    cmd.arg("copyto").arg(src).arg(dst);

    let output = tokio::process::Command::from(cmd)
        .output()
        .await
        .context("spawning rclone copyto")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("rclone copyto failed: {stderr}");
    }
    Ok(())
}
