// Executes a single sync cycle for one rule, implementing each conflict strategy

use anyhow::{Context, Result};
use chrono::Utc;
use onedrive_mount::{
    config::SyncRule,
    conflict::ConflictStrategy,
    paths::expand_tilde,
};
use std::{path::PathBuf, time::Duration};
use tracing::{info, warn};

/// How long a single rclone copy/check invocation may run before we abort it.
const SYNC_TIMEOUT: Duration = Duration::from_secs(10 * 60); // 10 minutes

pub struct SyncOutcome {
    pub at: chrono::DateTime<Utc>,
}

pub async fn run(remote_name: &str, rule: &SyncRule) -> Result<SyncOutcome> {
    let local = expand_tilde(&rule.local_path);
    let remote = format!("{}:{}", remote_name, rule.remote_path);

    tokio::fs::create_dir_all(&local)
        .await
        .context("creating local sync directory")?;

    tokio::time::timeout(SYNC_TIMEOUT, run_inner(&local, &remote, rule))
        .await
        .unwrap_or_else(|_| Err(anyhow::anyhow!("sync timed out after 10 minutes")))?;

    info!(rule = %rule.name, remote = %remote_name, "sync completed");
    Ok(SyncOutcome { at: Utc::now() })
}

async fn run_inner(local: &PathBuf, remote: &str, rule: &SyncRule) -> Result<()> {
    match rule.conflict_strategy {
        ConflictStrategy::RemoteWins => {
            // Remote is SSOT; pull remote to local, overwriting stale local files
            run_copy(remote, &local.to_string_lossy(), &rule.patterns).await?;
        }
        ConflictStrategy::NewestWins => {
            // Push local changes first (only newer), then pull remote (only newer)
            run_copy(&local.to_string_lossy(), remote, &rule.patterns).await?;
            run_copy(remote, &local.to_string_lossy(), &rule.patterns).await?;
        }
        ConflictStrategy::KeepBoth => {
            rename_conflicts(local, remote, rule).await?;
            run_copy(&local.to_string_lossy(), remote, &rule.patterns).await?;
            run_copy(remote, &local.to_string_lossy(), &rule.patterns).await?;
        }
    }
    Ok(())
}

async fn run_copy(src: &str, dst: &str, patterns: &[String]) -> Result<()> {
    let cmd = crate::rclone::copy_command(src, dst, patterns);

    let output = tokio::process::Command::from(cmd)
        .output()
        .await
        .context("spawning rclone copy")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("rclone copy failed: {stderr}");
    }
    Ok(())
}

async fn rename_conflicts(local: &PathBuf, remote: &str, rule: &SyncRule) -> Result<()> {
    // rclone check --differ lists files that exist on both sides but have different content.
    // We rename the local copy so the subsequent bidirectional copy keeps both versions.
    let cmd = crate::rclone::check_command(remote, &local.to_string_lossy(), &rule.patterns);

    let output = tokio::process::Command::from(cmd)
        .output()
        .await
        .context("spawning rclone check")?;

    // rclone check exits non-zero when differences exist — that's expected
    let stderr = String::from_utf8_lossy(&output.stderr);

    // rclone check writes differing files to stderr with a prefix like "ERROR : filename: sizes differ"
    for line in stderr.lines() {
        let filename = if let Some(rest) = line.strip_prefix("ERROR : ") {
            // "filename: sizes differ" or "filename: md5 hash differ"
            rest.split(':').next().map(str::trim)
        } else {
            None
        };

        let Some(filename) = filename else { continue };
        let local_path = local.join(filename);
        if local_path.exists() {
            let ts = Utc::now().format("%Y%m%dT%H%M%S");
            let stem = local_path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy();
            let ext = local_path
                .extension()
                .map(|e| format!(".{}", e.to_string_lossy()))
                .unwrap_or_default();
            let conflict_name = format!("{}.conflict-{}{}", stem, ts, ext);
            let conflict_path = local_path.with_file_name(conflict_name);

            warn!(
                from = %local_path.display(),
                to = %conflict_path.display(),
                "renaming conflicting local file"
            );

            tokio::fs::rename(&local_path, &conflict_path)
                .await
                .context("renaming conflict file")?;
        }
    }

    Ok(())
}
