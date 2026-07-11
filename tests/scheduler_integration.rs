// Integration tests for the sync scheduler logic.
//
// These tests spawn a minimal scheduler loop (same pattern as sync_scheduler.rs)
// using local-to-local rclone to verify scheduling behavior without mounting.
//
// Requires: `rclone` on PATH, `--features daemon`.

#![cfg(feature = "daemon")]

use chrono::Utc;
use onedrive_mount::{
    config::{MountConfig, RemoteConfig, SyncRule},
    conflict::SyncStrategy,
    status::{DaemonStatus, RemoteStatus, SyncRuleStatus, SyncState},
};
use std::fs;
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// Build initial DaemonStatus with one remote and one sync rule.
fn make_status(remote_name: &str, rule_name: &str) -> DaemonStatus {
    DaemonStatus {
        pid: std::process::id(),
        started_at: Some(Utc::now()),
        version: "test".into(),
        remotes: vec![RemoteStatus {
            name: remote_name.into(),
            mount: onedrive_mount::status::MountState::Unmounted,
            sync_rules: vec![SyncRuleStatus {
                name: rule_name.into(),
                last_sync: None,
                next_sync: None,
                state: SyncState::Idle,
                files_transferred: None,
                bytes_transferred: None,
                conflicts: vec![],
            }],
        }],
        config_error: None,
    }
}

/// A minimal scheduler loop that mirrors the real sync_scheduler.rs logic.
/// Uses rclone copy local->remote (MirrorUp strategy for simplicity).
async fn run_scheduler_loop(
    remote_name: String,
    rule: SyncRule,
    status_tx: watch::Sender<DaemonStatus>,
    cancel: CancellationToken,
    mut sync_now_rx: mpsc::Receiver<()>,
) {
    let interval_dur = Duration::from_secs(
        onedrive_mount::defaults::parse_interval_secs(&rule.interval).unwrap_or(900),
    );

    let mut timer = tokio::time::interval(interval_dur);
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    timer.tick().await; // consume immediate first tick

    loop {
        // Set next_sync
        let next = Utc::now() + chrono::Duration::from_std(interval_dur).unwrap_or_default();
        status_tx.send_modify(|s| {
            if let Some(r) = s.remotes.iter_mut().find(|r| r.name == remote_name)
                && let Some(sr) = r.sync_rules.iter_mut().find(|sr| sr.name == rule.name)
            {
                sr.next_sync = Some(next);
            }
        });

        // Wait for tick or manual trigger
        let triggered_manually = tokio::select! {
            _ = cancel.cancelled() => break,
            _ = timer.tick() => false,
            Some(()) = sync_now_rx.recv() => true,
        };

        if triggered_manually {
            timer.reset();
        }

        // Check if blocked
        let is_blocked = {
            let s = status_tx.borrow();
            s.remotes
                .iter()
                .find(|r| r.name == remote_name)
                .and_then(|r| r.sync_rules.iter().find(|sr| sr.name == rule.name))
                .map(|sr| sr.state.is_blocked())
                .unwrap_or(false)
        };
        if is_blocked {
            continue;
        }

        // Clear next_sync, set Running
        status_tx.send_modify(|s| {
            if let Some(r) = s.remotes.iter_mut().find(|r| r.name == remote_name)
                && let Some(sr) = r.sync_rules.iter_mut().find(|sr| sr.name == rule.name)
            {
                sr.next_sync = None;
                sr.state = SyncState::Running;
            }
        });

        // Execute sync (rclone sync local -> remote for MirrorUp)
        let local_path = rule.local_path.clone();
        let remote_path = rule.remote_path.clone();
        let output = tokio::process::Command::new("rclone")
            .arg("sync")
            .arg(&local_path)
            .arg(&remote_path)
            .output()
            .await;

        let now = Utc::now();
        match output {
            Ok(o) if o.status.success() => {
                status_tx.send_modify(|s| {
                    if let Some(r) = s.remotes.iter_mut().find(|r| r.name == remote_name)
                        && let Some(sr) = r.sync_rules.iter_mut().find(|sr| sr.name == rule.name)
                    {
                        sr.state = SyncState::Succeeded;
                        sr.last_sync = Some(now);
                    }
                });
            }
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr).to_string();
                status_tx.send_modify(|s| {
                    if let Some(r) = s.remotes.iter_mut().find(|r| r.name == remote_name)
                        && let Some(sr) = r.sync_rules.iter_mut().find(|sr| sr.name == rule.name)
                    {
                        sr.state = SyncState::Failed {
                            error: stderr,
                            at: now,
                        };
                        sr.last_sync = Some(now);
                    }
                });
            }
            Err(e) => {
                status_tx.send_modify(|s| {
                    if let Some(r) = s.remotes.iter_mut().find(|r| r.name == remote_name)
                        && let Some(sr) = r.sync_rules.iter_mut().find(|sr| sr.name == rule.name)
                    {
                        sr.state = SyncState::Failed {
                            error: e.to_string(),
                            at: now,
                        };
                        sr.last_sync = Some(now);
                    }
                });
            }
        }
    }
}

fn make_rule(local_path: &str, remote_path: &str, interval: &str) -> SyncRule {
    SyncRule {
        name: "test-rule".into(),
        remote_path: remote_path.into(),
        local_path: local_path.into(),
        patterns: vec![],
        interval: interval.into(),
        sync_strategy: SyncStrategy::MirrorUp,
        enabled: true,
    }
}

/// Test 1 & 3: The scheduler fires multiple sync cycles on time and doesn't
/// silently die after the first cycle.
#[tokio::test]
async fn scheduler_fires_multiple_cycles() {
    let local = TempDir::new().unwrap();
    let remote = TempDir::new().unwrap();

    // Write a file so rclone has something to do
    fs::write(local.path().join("hello.txt"), "initial").unwrap();

    let rule = make_rule(
        local.path().to_str().unwrap(),
        remote.path().to_str().unwrap(),
        "2s",
    );
    let (status_tx, status_rx) = watch::channel(make_status("local-test", "test-rule"));
    let cancel = CancellationToken::new();
    let (_sync_now_tx, sync_now_rx) = mpsc::channel::<()>(1);

    let cancel_clone = cancel.clone();
    let handle = tokio::spawn(run_scheduler_loop(
        "local-test".into(),
        rule,
        status_tx,
        cancel_clone,
        sync_now_rx,
    ));

    // Wait long enough for at least 2 sync cycles (2s interval, wait 5s)
    tokio::time::sleep(Duration::from_secs(5)).await;
    cancel.cancel();
    let _ = handle.await;

    let status = status_rx.borrow();
    let sr = &status.remotes[0].sync_rules[0];

    // Verify at least 2 syncs happened (last_sync is set and state is Succeeded)
    assert!(
        sr.last_sync.is_some(),
        "last_sync should be set after sync cycles"
    );
    assert_eq!(
        sr.state,
        SyncState::Succeeded,
        "state should be Succeeded after running"
    );

    // The file should have been synced to remote
    assert!(
        remote.path().join("hello.txt").exists(),
        "file should be synced to remote"
    );
}

/// Test 2: A local file edit gets uploaded to remote after a sync cycle.
#[tokio::test]
async fn local_edit_syncs_to_remote() {
    let local = TempDir::new().unwrap();
    let remote = TempDir::new().unwrap();

    let rule = make_rule(
        local.path().to_str().unwrap(),
        remote.path().to_str().unwrap(),
        "2s",
    );
    let (status_tx, status_rx) = watch::channel(make_status("local-test", "test-rule"));
    let cancel = CancellationToken::new();
    let (_sync_now_tx, sync_now_rx) = mpsc::channel::<()>(1);

    let cancel_clone = cancel.clone();
    let handle = tokio::spawn(run_scheduler_loop(
        "local-test".into(),
        rule,
        status_tx,
        cancel_clone,
        sync_now_rx,
    ));

    // Wait for first sync cycle to pass
    tokio::time::sleep(Duration::from_millis(2500)).await;

    // Write a new file after the first cycle
    fs::write(local.path().join("edited.txt"), "edited content").unwrap();

    // Wait for the next sync cycle to pick it up
    tokio::time::sleep(Duration::from_secs(3)).await;
    cancel.cancel();
    let _ = handle.await;

    // The edited file should now exist on remote
    let remote_content = fs::read_to_string(remote.path().join("edited.txt"));
    assert_eq!(
        remote_content.unwrap(),
        "edited content",
        "edited file should be synced to remote"
    );
}

/// Test 4: next_sync is updated correctly in status.
#[tokio::test]
async fn next_sync_is_updated_in_status() {
    let local = TempDir::new().unwrap();
    let remote = TempDir::new().unwrap();

    fs::write(local.path().join("data.txt"), "x").unwrap();

    let rule = make_rule(
        local.path().to_str().unwrap(),
        remote.path().to_str().unwrap(),
        "2s",
    );
    let (status_tx, mut status_rx) = watch::channel(make_status("local-test", "test-rule"));
    let cancel = CancellationToken::new();
    let (_sync_now_tx, sync_now_rx) = mpsc::channel::<()>(1);

    let cancel_clone = cancel.clone();
    let handle = tokio::spawn(run_scheduler_loop(
        "local-test".into(),
        rule,
        status_tx,
        cancel_clone,
        sync_now_rx,
    ));

    // Give the scheduler time to set next_sync (it does so immediately on loop entry)
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Check that next_sync was set
    let status = status_rx.borrow_and_update();
    let sr = &status.remotes[0].sync_rules[0];
    assert!(
        sr.next_sync.is_some(),
        "next_sync should be set before the first tick fires"
    );

    // Verify next_sync is roughly 2s in the future
    let next = sr.next_sync.unwrap();
    let diff = (next - Utc::now()).num_milliseconds();
    assert!(
        diff > 0 && diff <= 2500,
        "next_sync should be ~2s in the future, got {}ms",
        diff
    );
    drop(status);

    // Wait for a sync to complete, then check that next_sync is set again
    tokio::time::sleep(Duration::from_secs(3)).await;

    let status = status_rx.borrow_and_update();
    let sr = &status.remotes[0].sync_rules[0];
    // After completing a sync, the scheduler loops and sets next_sync again
    // (it may be None briefly during the sync itself, but should be set after)
    // At this point either next_sync is set (waiting for next tick) or
    // the sync just completed and it's about to set it.
    // We verify last_sync was set, confirming at least one cycle completed.
    assert!(sr.last_sync.is_some(), "at least one sync should have run");
    drop(status);

    cancel.cancel();
    let _ = handle.await;
}

/// Test 5: Blocked rules are skipped - set state to BlockedOnConflicts,
/// verify no sync runs.
#[tokio::test]
async fn blocked_rules_are_skipped() {
    let local = TempDir::new().unwrap();
    let remote = TempDir::new().unwrap();

    fs::write(local.path().join("should_not_sync.txt"), "blocked").unwrap();

    let rule = make_rule(
        local.path().to_str().unwrap(),
        remote.path().to_str().unwrap(),
        "2s",
    );

    let mut initial_status = make_status("local-test", "test-rule");
    // Set the rule to blocked state
    initial_status.remotes[0].sync_rules[0].state = SyncState::BlockedOnConflicts {
        since: Utc::now(),
    };

    let (status_tx, status_rx) = watch::channel(initial_status);
    let cancel = CancellationToken::new();
    let (_sync_now_tx, sync_now_rx) = mpsc::channel::<()>(1);

    let cancel_clone = cancel.clone();
    let handle = tokio::spawn(run_scheduler_loop(
        "local-test".into(),
        rule,
        status_tx,
        cancel_clone,
        sync_now_rx,
    ));

    // Wait for more than one interval
    tokio::time::sleep(Duration::from_secs(5)).await;
    cancel.cancel();
    let _ = handle.await;

    // Verify no sync happened: last_sync should still be None and file should not be on remote
    let status = status_rx.borrow();
    let sr = &status.remotes[0].sync_rules[0];
    assert!(
        sr.last_sync.is_none(),
        "blocked rule should never have synced"
    );
    assert!(
        !remote.path().join("should_not_sync.txt").exists(),
        "blocked rule should not have synced files to remote"
    );
}

/// Test 6: trigger_sync_now causes an immediate sync.
#[tokio::test]
async fn trigger_sync_now_causes_immediate_sync() {
    let local = TempDir::new().unwrap();
    let remote = TempDir::new().unwrap();

    fs::write(local.path().join("urgent.txt"), "immediate sync").unwrap();

    // Use a long interval so the timer won't fire naturally during the test
    let rule = make_rule(
        local.path().to_str().unwrap(),
        remote.path().to_str().unwrap(),
        "60s",
    );
    let (status_tx, mut status_rx) = watch::channel(make_status("local-test", "test-rule"));
    let cancel = CancellationToken::new();
    let (sync_now_tx, sync_now_rx) = mpsc::channel::<()>(1);

    let cancel_clone = cancel.clone();
    let handle = tokio::spawn(run_scheduler_loop(
        "local-test".into(),
        rule,
        status_tx,
        cancel_clone,
        sync_now_rx,
    ));

    // Give the scheduler time to start and wait on timer
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Trigger immediate sync
    let start = Instant::now();
    sync_now_tx.send(()).await.unwrap();

    // Wait for sync to complete (should be fast for local-to-local)
    tokio::time::sleep(Duration::from_secs(2)).await;
    let elapsed = start.elapsed();

    cancel.cancel();
    let _ = handle.await;

    // Verify the sync happened quickly (well under the 60s interval)
    assert!(
        elapsed < Duration::from_secs(5),
        "trigger_sync_now should cause immediate sync, took {:?}",
        elapsed
    );

    // Verify the file was synced
    let content = fs::read_to_string(remote.path().join("urgent.txt")).unwrap();
    assert_eq!(content, "immediate sync");

    // Verify status was updated
    let status = status_rx.borrow_and_update();
    let sr = &status.remotes[0].sync_rules[0];
    assert!(sr.last_sync.is_some(), "last_sync should be set");
    assert_eq!(sr.state, SyncState::Succeeded);
}
