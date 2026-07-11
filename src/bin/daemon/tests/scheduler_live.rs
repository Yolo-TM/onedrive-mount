/// Integration tests for SyncScheduler that exercise real rclone local-to-local syncs.
///
/// These tests live inside the daemon binary's test module because `sync_scheduler` is
/// a private module only accessible from within the binary crate.
///
/// # rclone local backend
/// `sync_executor` builds the remote path as `format!("{}:{}", remote_name, rule.remote_path)`.
/// Using `remote_name = ":local"` and `rule.remote_path = "/absolute/path"` produces
/// `":local:/absolute/path"`, which rclone interprets as a plain local filesystem path.
/// The `RemoteConfig.name` field doubles as the rclone remote name, so every test here
/// sets it to `":local"` and ensures the `DaemonStatus` uses the same name.
use crate::sync_scheduler::SyncScheduler;
use onedrive_mount::{
    config::{MountConfig, RemoteConfig, SyncRule},
    conflict::SyncStrategy,
    status::{DaemonStatus, MountState, RemoteStatus, SyncRuleStatus, SyncState},
};
use std::fs;
use tempfile::TempDir;
use tokio::time::{Duration, timeout};

// ─── helpers ─────────────────────────────────────────────────────────────────

/// Build a `RemoteConfig` wired up for local-to-local rclone.
///
/// * `remote_name` — must be `":local"` so rclone receives `":local:/abs/path"`
/// * `local_dir`   — the "local" side (`SyncRule.local_path`)
/// * `remote_dir`  — the "remote" side (absolute path, passed after `":local:"`)
/// * `interval`    — sync interval string, e.g. `"2s"` or `"60s"`
fn make_remote_config(
    remote_name: &str,
    rule_name: &str,
    local_dir: &TempDir,
    remote_dir: &TempDir,
    interval: &str,
) -> RemoteConfig {
    let local_path = local_dir.path().to_string_lossy().into_owned();
    let remote_path = remote_dir.path().to_string_lossy().into_owned();

    RemoteConfig {
        name: remote_name.to_string(),
        r#type: "local".to_string(),
        mount_point: local_dir.path().to_string_lossy().into_owned(),
        poll_interval: "60s".to_string(),
        enabled: true,
        mount: MountConfig::default(),
        sync_rules: vec![SyncRule {
            name: rule_name.to_string(),
            remote_path,
            local_path,
            patterns: vec!["*".to_string()],
            interval: interval.to_string(),
            sync_strategy: SyncStrategy::MirrorUp,
            enabled: true,
        }],
    }
}

/// Build a `DaemonStatus` that has the remote/rule pre-populated so the
/// scheduler can find (and update) the rule's state.
fn make_status(remote_name: &str, rule_name: &str, rule_state: SyncState) -> DaemonStatus {
    DaemonStatus {
        pid: 0,
        started_at: None,
        version: String::new(),
        remotes: vec![RemoteStatus {
            name: remote_name.to_string(),
            mount: MountState::Unmounted,
            sync_rules: vec![SyncRuleStatus {
                name: rule_name.to_string(),
                last_sync: None,
                next_sync: None,
                state: rule_state,
                files_transferred: None,
                bytes_transferred: None,
                conflicts: vec![],
            }],
        }],
        config_error: None,
    }
}

/// Read the `last_sync` timestamp for a rule out of a `DaemonStatus`.
fn last_sync(
    status: &DaemonStatus,
    remote_name: &str,
    rule_name: &str,
) -> Option<chrono::DateTime<chrono::Utc>> {
    status
        .remotes
        .iter()
        .find(|r| r.name == remote_name)
        .and_then(|r| r.sync_rules.iter().find(|sr| sr.name == rule_name))
        .and_then(|sr| sr.last_sync)
}

// ─── tests ───────────────────────────────────────────────────────────────────

/// The scheduler fires repeatedly according to its interval and updates `last_sync`.
#[tokio::test]
async fn scheduler_fires_multiple_cycles() {
    let local = TempDir::new().unwrap();
    let remote = TempDir::new().unwrap();

    // ":local" as remote_name causes sync_executor to form ":local:/abs/path",
    // which rclone treats as a local filesystem path.
    let remote_name = ":local";
    let rule_name = "fires_rule";

    let config = make_remote_config(remote_name, rule_name, &local, &remote, "2s");
    let initial_status = make_status(remote_name, rule_name, SyncState::Idle);
    let (status_tx, status_rx) = tokio::sync::watch::channel(initial_status);

    let mut scheduler = SyncScheduler::new();
    scheduler.start(&[config], status_tx);

    // Wait long enough for at least two sync cycles (interval 2s, wait 5s).
    tokio::time::sleep(Duration::from_secs(5)).await;

    let status_snapshot = status_rx.borrow().clone();
    let ts = last_sync(&status_snapshot, remote_name, rule_name);
    assert!(ts.is_some(), "last_sync should be set after multiple cycles");

    let age = chrono::Utc::now() - ts.unwrap();
    assert!(
        age.num_seconds() < 6,
        "last_sync should be within the last 6 seconds, but age was {}s",
        age.num_seconds()
    );

    scheduler.stop().await;
}

/// A file written to the local dir appears in the remote dir after one sync cycle.
#[tokio::test]
async fn local_file_syncs_to_remote() {
    let local = TempDir::new().unwrap();
    let remote = TempDir::new().unwrap();

    let remote_name = ":local";
    let rule_name = "file_sync_rule";

    let config = make_remote_config(remote_name, rule_name, &local, &remote, "2s");
    let initial_status = make_status(remote_name, rule_name, SyncState::Idle);
    let (status_tx, _status_rx) = tokio::sync::watch::channel(initial_status);

    let mut scheduler = SyncScheduler::new();
    scheduler.start(&[config], status_tx);

    // Write a file before the first sync fires.
    fs::write(local.path().join("hello.txt"), "world").unwrap();

    // Wait for the scheduler to run at least one sync (interval 2s, wait 3s).
    tokio::time::sleep(Duration::from_secs(3)).await;

    assert!(
        remote.path().join("hello.txt").exists(),
        "hello.txt should have been synced to the remote dir"
    );
    assert_eq!(
        fs::read_to_string(remote.path().join("hello.txt")).unwrap(),
        "world"
    );

    scheduler.stop().await;
}

/// `trigger_sync_now` causes a sync well before the next scheduled interval.
#[tokio::test]
async fn trigger_sync_now_fires_immediately() {
    let local = TempDir::new().unwrap();
    let remote = TempDir::new().unwrap();

    let remote_name = ":local";
    let rule_name = "trigger_rule";

    // Use a very long interval so the timer would never fire during the test.
    let config = make_remote_config(remote_name, rule_name, &local, &remote, "60s");
    let initial_status = make_status(remote_name, rule_name, SyncState::Idle);
    let (status_tx, status_rx) = tokio::sync::watch::channel(initial_status);

    let mut scheduler = SyncScheduler::new();
    scheduler.start(&[config], status_tx);

    // Trigger an immediate sync.
    let triggered = scheduler.trigger_sync_now(remote_name, rule_name);
    assert!(triggered, "trigger_sync_now should return true for a known rule");

    // Wait up to 5s for the sync to complete.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let ts = last_sync(&status_rx.borrow(), remote_name, rule_name);
        if ts.is_some() {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("last_sync was never set after trigger_sync_now within 5 seconds");
        }
    }

    scheduler.stop().await;
}

/// When a rule is `BlockedOnConflicts`, the scheduler skips sync and leaves
/// `last_sync` as `None`.
#[tokio::test]
async fn blocked_rule_skipped() {
    let local = TempDir::new().unwrap();
    let remote = TempDir::new().unwrap();

    let remote_name = ":local";
    let rule_name = "blocked_rule";

    let config = make_remote_config(remote_name, rule_name, &local, &remote, "2s");
    // Pre-set the rule state to BlockedOnConflicts so the scheduler never runs.
    let initial_status = make_status(
        remote_name,
        rule_name,
        SyncState::BlockedOnConflicts {
            since: chrono::Utc::now(),
        },
    );
    let (status_tx, status_rx) = tokio::sync::watch::channel(initial_status);

    let mut scheduler = SyncScheduler::new();
    scheduler.start(&[config], status_tx);

    // Wait longer than two intervals; sync should be skipped every time.
    tokio::time::sleep(Duration::from_secs(4)).await;

    let ts = last_sync(&status_rx.borrow(), remote_name, rule_name);
    assert!(
        ts.is_none(),
        "last_sync should remain None when rule is BlockedOnConflicts, got {:?}",
        ts
    );

    scheduler.stop().await;
}

/// `stop()` completes without hanging even when a sync is in progress.
#[tokio::test]
async fn scheduler_stops_cleanly() {
    let local = TempDir::new().unwrap();
    let remote = TempDir::new().unwrap();

    let remote_name = ":local";
    let rule_name = "stop_rule";

    let config = make_remote_config(remote_name, rule_name, &local, &remote, "2s");
    let initial_status = make_status(remote_name, rule_name, SyncState::Idle);
    let (status_tx, _status_rx) = tokio::sync::watch::channel(initial_status);

    let mut scheduler = SyncScheduler::new();
    scheduler.start(&[config], status_tx);

    // Allow at least one sync to run.
    tokio::time::sleep(Duration::from_secs(2)).await;

    // stop() must complete within 5 seconds; it should never hang.
    timeout(Duration::from_secs(5), scheduler.stop())
        .await
        .expect("scheduler.stop() should complete within 5 seconds");
}
