use anyhow::{Context, Result};
use chrono::Utc;
use onedrive_mount::{
    paths::sync_baseline_file,
    resolution::{Resolution, ResolutionAction},
    status::{ConflictEntry, DaemonStatus, SyncState},
    sync_baseline::SyncBaseline,
};
use tokio::sync::watch;
use tracing::{error, info, warn};

pub struct ApplyResult {
    pub unblocked: Vec<(String, String)>,
    pub failed: Vec<Resolution>,
}

pub async fn apply(
    resolutions: &[Resolution],
    status_tx: &watch::Sender<DaemonStatus>,
) -> ApplyResult {
    let mut unblocked = Vec::new();
    let mut failed = Vec::new();

    for res in resolutions {
        info!(
            remote = %res.remote,
            rule = %res.rule,
            file = %res.file,
            action = ?res.action,
            "applying conflict resolution"
        );

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

                if !matches!(res.action, ResolutionAction::KeepBoth) {
                    let conflict_path = std::path::Path::new(&conflict.local_path);
                    if conflict_path.exists() {
                        if let Err(e) = tokio::fs::remove_file(conflict_path).await {
                            warn!(
                                file = %conflict.local_path,
                                error = %e,
                                "failed to delete resolved conflict file"
                            );
                        } else {
                            info!(file = %conflict.local_path, "deleted resolved conflict file");
                        }
                    }
                }

                let baseline_path = sync_baseline_file(&res.remote, &res.rule);
                let mut baseline = SyncBaseline::load(&baseline_path);
                baseline.set(&res.file, Utc::now());
                if let Err(e) = baseline.save(&baseline_path) {
                    warn!(error = %e, "failed to update baseline after resolution");
                }

                status_tx.send_modify(|s| {
                    if let Some(remote) = s.remotes.iter_mut().find(|r| r.name == res.remote)
                        && let Some(rule) =
                            remote.sync_rules.iter_mut().find(|sr| sr.name == res.rule)
                    {
                        rule.conflicts.retain(|c| c.file != res.file);
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
                failed.push(res.clone());
            }
        }
    }

    let s = status_tx.borrow();
    for res in resolutions {
        if let Some(remote) = s.remotes.iter().find(|r| r.name == res.remote)
            && let Some(rule) = remote.sync_rules.iter().find(|sr| sr.name == res.rule)
            && !rule.state.is_blocked()
            && !unblocked
                .iter()
                .any(|(r, n)| r == &res.remote && n == &res.rule)
        {
            unblocked.push((res.remote.clone(), res.rule.clone()));
        }
    }

    ApplyResult { unblocked, failed }
}

async fn apply_one(res: &Resolution, conflict: &ConflictEntry) -> Result<()> {
    match res.action {
        ResolutionAction::KeepLocal => {
            info!(
                file = %res.file,
                conflict_file = %conflict.local_path,
                remote = %conflict.remote_path,
                "keep_local: uploading local version to remote"
            );
            run_copy(&conflict.local_path, &conflict.remote_path).await?;

            info!(
                file = %res.file,
                from = %conflict.local_path,
                to = %conflict.original_local_path,
                "keep_local: restoring local version as canonical local file"
            );
            tokio::fs::copy(&conflict.local_path, &conflict.original_local_path)
                .await
                .context("restoring local file for keep_local")?;
        }
        ResolutionAction::KeepRemote => {
            let original = std::path::Path::new(&conflict.original_local_path);
            if !original.exists() {
                info!(
                    file = %res.file,
                    local = %conflict.original_local_path,
                    remote = %conflict.remote_path,
                    "keep_remote: local file missing — downloading remote version"
                );
                run_copy(&conflict.remote_path, &conflict.original_local_path).await?;
            } else {
                info!(
                    file = %res.file,
                    local = %conflict.original_local_path,
                    "keep_remote: remote version already in place locally, discarding conflict file"
                );
            }
        }
        ResolutionAction::KeepBoth => {
            let conflict_file = std::path::Path::new(&conflict.local_path);
            let ts = Utc::now().format("%Y%m%dT%H%M%S");
            let raw_stem = conflict_file
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy();
            let base_stem = raw_stem
                .find(".conflict-")
                .map(|i| &raw_stem[..i])
                .unwrap_or(&raw_stem);
            let ext = conflict_file
                .extension()
                .map(|e| format!(".{}", e.to_string_lossy()))
                .unwrap_or_default();
            let permanent_name = format!("{}.conflict-{}{}", base_stem, ts, ext);
            let permanent_path = conflict_file
                .parent()
                .unwrap_or(conflict_file)
                .join(&permanent_name);

            info!(
                file = %res.file,
                from = %conflict_file.display(),
                to = %permanent_path.display(),
                "keep_both: renaming conflict file to permanent name"
            );
            tokio::fs::rename(conflict_file, &permanent_path)
                .await
                .context("renaming conflict file for keep_both")?;
        }
    }
    Ok(())
}

async fn run_copy(src: &str, dst: &str) -> Result<()> {
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
