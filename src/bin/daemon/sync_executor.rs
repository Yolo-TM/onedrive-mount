use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use onedrive_mount::{
    config::SyncRule,
    conflict::SyncStrategy,
    paths::{expand_tilde, sync_baseline_file},
    status::{ConflictEntry, DaemonStatus, SyncState},
    sync_baseline::SyncBaseline,
};
use std::time::Duration;
use tokio::sync::watch;
use tracing::warn;

use crate::rclone::CopyMode;

const SYNC_TIMEOUT: Duration = Duration::from_secs(10 * 60);

pub struct SyncOutcome {
    pub at: chrono::DateTime<Utc>,
    pub files_transferred: u32,
    pub bytes_transferred: u64,
}

pub async fn run(
    remote_name: &str,
    rule: &SyncRule,
    status_tx: &watch::Sender<DaemonStatus>,
) -> Result<SyncOutcome> {
    let local = expand_tilde(&rule.local_path);
    let remote = format!("{}:{}", remote_name, rule.remote_path);

    tokio::fs::create_dir_all(&local)
        .await
        .context("creating local sync directory")?;

    let stats = tokio::time::timeout(
        SYNC_TIMEOUT,
        run_inner(&local, &remote, remote_name, rule, status_tx),
    )
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

async fn run_inner(
    local: &std::path::Path,
    remote: &str,
    remote_name: &str,
    rule: &SyncRule,
    status_tx: &watch::Sender<DaemonStatus>,
) -> Result<TransferStats> {
    let local_str = local.to_string_lossy();
    let mut stats = TransferStats::default();

    match rule.sync_strategy {
        SyncStrategy::Bidirectional => {
            rename_conflicts(local, remote, remote_name, rule, status_tx).await?;
            stats.add(&run_copy(&local_str, remote, &rule.patterns, CopyMode::Normal, true).await?);
            stats
                .add(&run_copy(remote, &local_str, &rule.patterns, CopyMode::Normal, false).await?);
            update_baseline(local, remote, remote_name, rule).await;
        }
        SyncStrategy::NewestWins => {
            stats.add(
                &run_copy(
                    &local_str,
                    remote,
                    &rule.patterns,
                    CopyMode::IgnoreExisting,
                    false,
                )
                .await?,
            );
            stats.add(
                &run_copy(
                    remote,
                    &local_str,
                    &rule.patterns,
                    CopyMode::IgnoreExisting,
                    false,
                )
                .await?,
            );
            stats
                .add(&run_copy(&local_str, remote, &rule.patterns, CopyMode::Update, false).await?);
            stats
                .add(&run_copy(remote, &local_str, &rule.patterns, CopyMode::Update, false).await?);
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

const STATS_FLAGS: &[&str] = &["--use-json-log", "--stats-one-line", "-v"];

fn parse_stats(stderr: &[u8]) -> TransferStats {
    let text = String::from_utf8_lossy(stderr);
    for line in text.lines().rev() {
        if !line.contains("\"stats\"") {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line)
            && let Some(stats) = v.get("stats")
        {
            let files = stats.get("transfers").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let bytes = stats.get("bytes").and_then(|v| v.as_u64()).unwrap_or(0);
            return TransferStats { files, bytes };
        }
    }
    TransferStats::default()
}

async fn rename_conflicts(
    local: &std::path::Path,
    remote: &str,
    remote_name: &str,
    rule: &SyncRule,
    status_tx: &watch::Sender<DaemonStatus>,
) -> Result<()> {
    let cmd = crate::rclone::check_command(remote, &local.to_string_lossy(), &rule.patterns);
    let output = tokio::process::Command::from(cmd)
        .output()
        .await
        .context("spawning rclone check")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let differing: Vec<String> = stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();

    if differing.is_empty() {
        return Ok(());
    }

    let baseline_path = sync_baseline_file(remote_name, &rule.name);
    let baseline = SyncBaseline::load(&baseline_path);
    let has_baseline = !baseline.files.is_empty();

    let mut new_conflicts: Vec<ConflictEntry> = Vec::new();

    for relative_path in &differing {
        let local_path = local.join(relative_path);
        let local_meta = match tokio::fs::metadata(&local_path).await {
            Ok(m) => m,
            Err(_) => continue,
        };

        let local_mtime = local_meta
            .modified()
            .ok()
            .and_then(|t| {
                DateTime::from_timestamp(
                    t.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as i64,
                    0,
                )
            })
            .unwrap_or_else(Utc::now);

        let remote_file_path = format!("{}/{}", remote, relative_path);
        let (remote_size, remote_mtime) = fetch_remote_meta(&remote_file_path).await;

        if has_baseline {
            let local_changed = !baseline.is_unchanged(relative_path, local_mtime);
            let remote_changed = !baseline.is_unchanged(relative_path, remote_mtime);

            if !local_changed || !remote_changed {
                tracing::debug!(
                    rule = %rule.name,
                    file = %relative_path,
                    local_changed,
                    remote_changed,
                    "file differs but only one side changed since last sync — not a conflict"
                );
                continue;
            }
        } else {
            tracing::debug!(
                rule = %rule.name,
                file = %relative_path,
                "no baseline yet — skipping conflict detection for first sync"
            );
            continue;
        }

        let local_size = local_meta.len();
        let ts = Utc::now().format("%Y%m%dT%H%M%S");
        let stem = local_path.file_stem().unwrap_or_default().to_string_lossy();
        let ext = local_path
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy()))
            .unwrap_or_default();
        let conflict_name = format!("{}.conflict-{}{}", stem, ts, ext);
        let conflict_path = local_path.parent().unwrap_or(local).join(&conflict_name);

        warn!(
            rule = %rule.name,
            file = %relative_path,
            from = %local_path.display(),
            to = %conflict_path.display(),
            "true conflict — both sides changed since last sync, renaming local copy"
        );

        tokio::fs::rename(&local_path, &conflict_path)
            .await
            .context("renaming conflict file")?;

        new_conflicts.push(ConflictEntry {
            file: relative_path.to_string(),
            local_path: conflict_path.to_string_lossy().to_string(),
            original_local_path: local_path.to_string_lossy().to_string(),
            remote_path: remote_file_path,
            local_size,
            local_mtime,
            remote_size,
            remote_mtime,
            detected_at: Utc::now(),
        });
    }

    if !new_conflicts.is_empty() {
        let rule_name = rule.name.clone();
        status_tx.send_modify(|s| {
            if let Some(remote_status) = s.remotes.iter_mut().find(|r| r.name == remote_name)
                && let Some(rule_status) = remote_status
                    .sync_rules
                    .iter_mut()
                    .find(|r| r.name == rule_name)
            {
                for entry in new_conflicts {
                    if !rule_status.conflicts.iter().any(|c| c.file == entry.file) {
                        rule_status.conflicts.push(entry);
                    }
                }
                rule_status.state = SyncState::BlockedOnConflicts { since: Utc::now() };
            }
        });
    }

    Ok(())
}

async fn update_baseline(
    local: &std::path::Path,
    remote: &str,
    remote_name: &str,
    rule: &SyncRule,
) {
    let baseline_path = sync_baseline_file(remote_name, &rule.name);
    let mut baseline = SyncBaseline::load(&baseline_path);

    if let Ok(mut read_dir) = tokio::fs::read_dir(local).await {
        while let Ok(Some(entry)) = read_dir.next_entry().await {
            let path = entry.path();
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if name.contains(".conflict-") {
                continue;
            }
            let Ok(meta) = tokio::fs::metadata(&path).await else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            let Some(relative) = path
                .strip_prefix(local)
                .ok()
                .map(|p| p.to_string_lossy().to_string())
            else {
                continue;
            };
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| {
                    DateTime::from_timestamp(
                        t.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as i64,
                        0,
                    )
                })
                .unwrap_or_else(Utc::now);
            baseline.set(&relative, mtime);
        }
    }

    let output = tokio::process::Command::new("rclone")
        .arg("lsjson")
        .arg(remote)
        .output()
        .await;

    if let Ok(output) = output
        && output.status.success()
        && let Ok(arr) = serde_json::from_slice::<serde_json::Value>(&output.stdout)
        && let Some(files) = arr.as_array()
    {
        for f in files {
            let Some(name) = f.get("Name").and_then(|v| v.as_str()) else {
                continue;
            };
            if baseline.files.contains_key(name) {
                continue;
            }
            let mtime = f
                .get("ModTime")
                .and_then(|v| v.as_str())
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(Utc::now);
            baseline.set(name, mtime);
        }
    }

    if let Err(e) = baseline.save(&baseline_path) {
        warn!(remote = %remote_name, rule = %rule.name, error = %e, "failed to save sync baseline");
    } else {
        tracing::debug!(
            remote = %remote_name,
            rule = %rule.name,
            files = baseline.files.len(),
            "sync baseline updated"
        );
    }
}

async fn fetch_remote_meta(remote_path: &str) -> (u64, DateTime<Utc>) {
    let output = tokio::process::Command::new("rclone")
        .arg("lsjson")
        .arg("--no-traverse")
        .arg(remote_path)
        .output()
        .await;

    let Ok(output) = output else {
        return (0, Utc::now());
    };
    if !output.status.success() {
        return (0, Utc::now());
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let Ok(arr) = serde_json::from_str::<serde_json::Value>(&text) else {
        return (0, Utc::now());
    };
    let Some(obj) = arr.as_array().and_then(|a| a.first()) else {
        return (0, Utc::now());
    };

    let size = obj.get("Size").and_then(|v| v.as_u64()).unwrap_or(0);
    let mtime = obj
        .get("ModTime")
        .and_then(|v| v.as_str())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(Utc::now);

    (size, mtime)
}
