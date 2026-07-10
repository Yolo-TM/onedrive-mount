// Executes a single sync cycle for one rule, implementing each sync strategy

use anyhow::{Context, Result};
use chrono::Utc;
use onedrive_mount::{config::SyncRule, conflict::SyncStrategy, paths::expand_tilde};
use std::time::Duration;
use tracing::warn;

use crate::rclone::CopyMode;

/// How long a single rclone copy/check/sync invocation may run before we abort it.
const SYNC_TIMEOUT: Duration = Duration::from_secs(10 * 60); // 10 minutes

pub struct SyncOutcome {
    pub at: chrono::DateTime<Utc>,
    pub files_transferred: u32,
    pub bytes_transferred: u64,
}

pub async fn run(remote_name: &str, rule: &SyncRule) -> Result<SyncOutcome> {
    let local = expand_tilde(&rule.local_path);
    let remote = format!("{}:{}", remote_name, rule.remote_path);

    tokio::fs::create_dir_all(&local)
        .await
        .context("creating local sync directory")?;

    let stats = tokio::time::timeout(SYNC_TIMEOUT, run_inner(&local, &remote, rule))
        .await
        .unwrap_or_else(|_| Err(anyhow::anyhow!("sync timed out after 10 minutes")))?;

    tracing::debug!(
        rule = %rule.name,
        remote = %remote_name,
        files = stats.files,
        bytes = stats.bytes,
        "sync completed"
    );
    Ok(SyncOutcome {
        at: Utc::now(),
        files_transferred: stats.files,
        bytes_transferred: stats.bytes,
    })
}

/// Accumulated transfer stats across all rclone invocations in a single sync cycle.
#[derive(Default)]
struct TransferStats {
    files: u32,
    bytes: u64,
}

impl TransferStats {
    fn add(&mut self, other: &TransferStats) {
        self.files += other.files;
        self.bytes += other.bytes;
    }
}

async fn run_inner(local: &std::path::Path, remote: &str, rule: &SyncRule) -> Result<TransferStats> {
    let local_str = local.to_string_lossy();
    let mut stats = TransferStats::default();

    match rule.sync_strategy {
        SyncStrategy::Bidirectional => {
            // 1. Detect conflicts (files changed on both sides)
            rename_conflicts(local, remote, rule).await?;
            // 2. Push local → remote (exclude .conflict-* files, they stay local only)
            stats.add(&run_copy(&local_str, remote, &rule.patterns, CopyMode::Normal, true).await?);
            // 3. Pull remote → local (remote version overwrites the renamed-away local)
            stats.add(&run_copy(remote, &local_str, &rule.patterns, CopyMode::Normal, false).await?);
        }
        SyncStrategy::NewestWins => {
            // 1. Push local-only new files that don't exist on remote
            stats.add(&run_copy(&local_str, remote, &rule.patterns, CopyMode::IgnoreExisting, false).await?);
            // 2. Pull remote-only new files that don't exist locally
            stats.add(&run_copy(remote, &local_str, &rule.patterns, CopyMode::IgnoreExisting, false).await?);
            // 3. Push local files where local is newer
            stats.add(&run_copy(&local_str, remote, &rule.patterns, CopyMode::Update, false).await?);
            // 4. Pull remote files where remote is newer
            stats.add(&run_copy(remote, &local_str, &rule.patterns, CopyMode::Update, false).await?);
        }
        SyncStrategy::MirrorDown => {
            tracing::info!(rule = %rule.name, "mirror_down: discarding local changes, syncing remote to local");
            stats.add(&run_sync(remote, &local_str, &rule.patterns).await?);
        }
        SyncStrategy::MirrorUp => {
            tracing::info!(rule = %rule.name, "mirror_up: overwriting remote with local");
            stats.add(&run_sync(&local_str, remote, &rule.patterns).await?);
        }
    }
    Ok(stats)
}

async fn run_copy(
    src: &str,
    dst: &str,
    patterns: &[String],
    mode: CopyMode,
    exclude_conflicts: bool,
) -> Result<TransferStats> {
    let mut cmd = crate::rclone::copy_command(src, dst, patterns, mode, exclude_conflicts);
    cmd.args(STATS_FLAGS);

    tracing::debug!(%src, %dst, ?mode, exclude_conflicts, "running rclone copy");

    let output = tokio::process::Command::from(cmd)
        .output()
        .await
        .context("spawning rclone copy")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("rclone copy failed: {stderr}");
    }

    Ok(parse_stats(&output.stderr))
}

async fn run_sync(src: &str, dst: &str, patterns: &[String]) -> Result<TransferStats> {
    let mut cmd = crate::rclone::sync_command(src, dst, patterns);
    cmd.args(STATS_FLAGS);

    tracing::debug!(%src, %dst, "running rclone sync");

    let output = tokio::process::Command::from(cmd)
        .output()
        .await
        .context("spawning rclone sync")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("rclone sync failed: {stderr}");
    }

    Ok(parse_stats(&output.stderr))
}

/// Flags appended to every rclone invocation to get JSON stats on stderr.
const STATS_FLAGS: &[&str] = &["--use-json-log", "--stats-one-line", "-v"];

/// Parse the last JSON stats line from rclone's stderr.
/// Looks for a line containing `"stats":{...}` and extracts `transfers` and `bytes`.
fn parse_stats(stderr: &[u8]) -> TransferStats {
    let text = String::from_utf8_lossy(stderr);
    // Find the last line that contains a "stats" object
    for line in text.lines().rev() {
        if !line.contains("\"stats\"") {
            continue;
        }
        // Parse as generic JSON value
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            if let Some(stats) = v.get("stats") {
                let files = stats
                    .get("transfers")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32;
                let bytes = stats
                    .get("bytes")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                return TransferStats { files, bytes };
            }
        }
    }
    TransferStats::default()
}

async fn rename_conflicts(local: &std::path::Path, remote: &str, rule: &SyncRule) -> Result<()> {
    // rclone check --differ - writes conflicting relative file paths to stdout, one per line.
    // Exit code is non-zero when differences exist — that's expected, not an error.
    let cmd = crate::rclone::check_command(remote, &local.to_string_lossy(), &rule.patterns);

    let output = tokio::process::Command::from(cmd)
        .output()
        .await
        .context("spawning rclone check")?;

    let stdout = String::from_utf8_lossy(&output.stdout);

    for relative_path in stdout.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let local_path = local.join(relative_path);
        if !local_path.exists() {
            continue;
        }

        let ts = Utc::now().format("%Y%m%dT%H%M%S");
        let stem = local_path.file_stem().unwrap_or_default().to_string_lossy();
        let ext = local_path
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy()))
            .unwrap_or_default();
        let conflict_name = format!("{}.conflict-{}{}", stem, ts, ext);
        // Use parent() so subdirectory files stay in their original directory
        let conflict_path = local_path.parent().unwrap_or(local).join(conflict_name);

        warn!(
            rule = %rule.name,
            file = %relative_path,
            from = %local_path.display(),
            to = %conflict_path.display(),
            "conflict detected — renaming local copy (stays local only)"
        );

        tokio::fs::rename(&local_path, &conflict_path)
            .await
            .context("renaming conflict file")?;
    }

    Ok(())
}
