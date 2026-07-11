use chrono::Utc;
use onedrive_mount::{
    paths::sync_baseline_file,
    resolution::{Resolution, ResolutionAction},
    status::{
        ConflictEntry, DaemonStatus, MountState, RemoteStatus, SyncRuleStatus, SyncState,
    },
    sync_baseline::SyncBaseline,
};
use std::fs;
use tempfile::TempDir;
use tokio::sync::watch;

use crate::resolution_executor;

/// Build a minimal DaemonStatus with one remote+rule in BlockedOnConflicts state,
/// containing the given conflicts.
fn make_status(remote: &str, rule: &str, conflicts: Vec<ConflictEntry>) -> DaemonStatus {
    DaemonStatus {
        pid: std::process::id(),
        started_at: Some(Utc::now()),
        version: "test".to_string(),
        remotes: vec![RemoteStatus {
            name: remote.to_string(),
            mount: MountState::Unmounted,
            sync_rules: vec![SyncRuleStatus {
                name: rule.to_string(),
                last_sync: None,
                next_sync: None,
                state: SyncState::BlockedOnConflicts { since: Utc::now() },
                files_transferred: None,
                bytes_transferred: None,
                conflicts,
            }],
        }],
        config_error: None,
    }
}

/// Create a ConflictEntry for a file.
fn make_conflict(
    file: &str,
    local_conflict_path: &str,
    original_local_path: &str,
    remote_path: &str,
) -> ConflictEntry {
    ConflictEntry {
        file: file.to_string(),
        local_path: local_conflict_path.to_string(),
        original_local_path: original_local_path.to_string(),
        remote_path: remote_path.to_string(),
        local_size: 13,
        local_mtime: Utc::now(),
        remote_size: 14,
        remote_mtime: Utc::now(),
        detected_at: Utc::now(),
    }
}

/// RAII guard: removes the real on-disk baseline file for (remote, rule) on drop,
/// so that tests using the system data_dir don't interfere with each other.
struct BaselineGuard {
    path: std::path::PathBuf,
}

impl Drop for BaselineGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn baseline_guard(remote: &str, rule: &str) -> BaselineGuard {
    BaselineGuard {
        path: sync_baseline_file(remote, rule),
    }
}

// ─── KeepLocal test ───

#[tokio::test]
async fn keep_local_restores_local_and_uploads() {
    let local_dir = TempDir::new().unwrap();
    let remote_dir = TempDir::new().unwrap();
    let _baseline = baseline_guard("res-test-keep-local", "rule-keep-local");

    // Simulate post-conflict-detection state:
    // - Original local path has the REMOTE content (pulled by sync step after rename)
    // - Conflict file has the LOCAL content (renamed before pull)
    // - Remote file has the REMOTE content
    let original_path = local_dir.path().join("test.txt");
    let conflict_path = local_dir.path().join("test.conflict-20260101T000000.txt");
    let remote_path = remote_dir.path().join("test.txt");

    fs::write(&original_path, "remote content").unwrap();
    fs::write(&conflict_path, "local content").unwrap();
    fs::write(&remote_path, "remote content").unwrap();

    let conflict = make_conflict(
        "test.txt",
        conflict_path.to_str().unwrap(),
        original_path.to_str().unwrap(),
        remote_path.to_str().unwrap(),
    );

    let status = make_status("res-test-keep-local", "rule-keep-local", vec![conflict]);
    let (tx, _rx) = watch::channel(status);

    let resolutions = vec![Resolution {
        remote: "res-test-keep-local".to_string(),
        rule: "rule-keep-local".to_string(),
        file: "test.txt".to_string(),
        action: ResolutionAction::KeepLocal,
        resolved_at: Utc::now(),
    }];

    let result = resolution_executor::apply(&resolutions, &tx).await;

    assert!(result.failed.is_empty(), "resolution should not fail");

    // The original local path should have LOCAL content (restored from conflict file)
    assert_eq!(fs::read_to_string(&original_path).unwrap(), "local content");
    // The remote file should have LOCAL content (uploaded)
    assert_eq!(fs::read_to_string(&remote_path).unwrap(), "local content");
    // The conflict file should be deleted
    assert!(!conflict_path.exists(), "conflict file should be deleted");

    // The conflict should be removed from status
    let s = tx.borrow();
    assert!(s.remotes[0].sync_rules[0].conflicts.is_empty());
}

// ─── KeepRemote test ───

#[tokio::test]
async fn keep_remote_deletes_conflict_file() {
    let local_dir = TempDir::new().unwrap();
    let remote_dir = TempDir::new().unwrap();
    let _baseline = baseline_guard("res-test-keep-remote", "rule-keep-remote");

    // After conflict detection + sync pull:
    // - Original local path has the REMOTE content (already pulled)
    // - Conflict file has the LOCAL content
    // - Remote has the REMOTE content
    let original_path = local_dir.path().join("test.txt");
    let conflict_path = local_dir.path().join("test.conflict-20260101T000000.txt");
    let remote_path = remote_dir.path().join("test.txt");

    fs::write(&original_path, "remote content").unwrap();
    fs::write(&conflict_path, "local content").unwrap();
    fs::write(&remote_path, "remote content").unwrap();

    let conflict = make_conflict(
        "test.txt",
        conflict_path.to_str().unwrap(),
        original_path.to_str().unwrap(),
        remote_path.to_str().unwrap(),
    );

    let status = make_status("res-test-keep-remote", "rule-keep-remote", vec![conflict]);
    let (tx, _rx) = watch::channel(status);

    let resolutions = vec![Resolution {
        remote: "res-test-keep-remote".to_string(),
        rule: "rule-keep-remote".to_string(),
        file: "test.txt".to_string(),
        action: ResolutionAction::KeepRemote,
        resolved_at: Utc::now(),
    }];

    let result = resolution_executor::apply(&resolutions, &tx).await;

    assert!(result.failed.is_empty(), "resolution should not fail");

    // The original local path should still have REMOTE content
    assert_eq!(
        fs::read_to_string(&original_path).unwrap(),
        "remote content"
    );
    // The conflict file should be deleted
    assert!(!conflict_path.exists(), "conflict file should be deleted");

    // The conflict should be removed from status
    let s = tx.borrow();
    assert!(s.remotes[0].sync_rules[0].conflicts.is_empty());
}

// ─── KeepBoth test ───

#[tokio::test]
async fn keep_both_renames_conflict_permanently() {
    let local_dir = TempDir::new().unwrap();
    let _baseline = baseline_guard("res-test-keep-both", "rule-keep-both");

    // After conflict detection + sync pull:
    // - Original local path has the REMOTE content
    // - Conflict file has the LOCAL content
    let original_path = local_dir.path().join("test.txt");
    let conflict_path = local_dir.path().join("test.conflict-20260101T000000.txt");

    fs::write(&original_path, "remote content").unwrap();
    fs::write(&conflict_path, "local content").unwrap();

    // remote_path is unused by KeepBoth (no upload/download) but must be a valid
    // entry in the ConflictEntry so the lookup succeeds.
    let remote_dir = TempDir::new().unwrap();
    let remote_path = remote_dir.path().join("test.txt");
    fs::write(&remote_path, "remote content").unwrap();

    let conflict = make_conflict(
        "test.txt",
        conflict_path.to_str().unwrap(),
        original_path.to_str().unwrap(),
        remote_path.to_str().unwrap(),
    );

    let status = make_status("res-test-keep-both", "rule-keep-both", vec![conflict]);
    let (tx, _rx) = watch::channel(status);

    let resolutions = vec![Resolution {
        remote: "res-test-keep-both".to_string(),
        rule: "rule-keep-both".to_string(),
        file: "test.txt".to_string(),
        action: ResolutionAction::KeepBoth,
        resolved_at: Utc::now(),
    }];

    let result = resolution_executor::apply(&resolutions, &tx).await;

    assert!(result.failed.is_empty(), "resolution should not fail");

    // The original local path should still have REMOTE content (untouched)
    assert_eq!(
        fs::read_to_string(&original_path).unwrap(),
        "remote content"
    );

    // The old (temporary) conflict file should no longer exist at its original name
    assert!(
        !conflict_path.exists(),
        "old conflict file should be renamed to permanent name"
    );

    // A new permanent conflict file should exist with LOCAL content
    let permanent_files: Vec<_> = fs::read_dir(local_dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.contains(".conflict-") && name != "test.conflict-20260101T000000.txt"
        })
        .collect();
    assert_eq!(
        permanent_files.len(),
        1,
        "should have exactly one permanent conflict file"
    );
    let permanent_content = fs::read_to_string(permanent_files[0].path()).unwrap();
    assert_eq!(permanent_content, "local content");

    // The conflict should be removed from status
    let s = tx.borrow();
    assert!(s.remotes[0].sync_rules[0].conflicts.is_empty());
}

// ─── State transition test ───

#[tokio::test]
async fn resolution_unblocks_rule() {
    let local_dir = TempDir::new().unwrap();
    let remote_dir = TempDir::new().unwrap();
    let _baseline = baseline_guard("res-test-unblock", "rule-unblock");

    let original_path = local_dir.path().join("test.txt");
    let conflict_path = local_dir.path().join("test.conflict-20260101T000000.txt");
    let remote_path = remote_dir.path().join("test.txt");

    fs::write(&original_path, "remote content").unwrap();
    fs::write(&conflict_path, "local content").unwrap();
    fs::write(&remote_path, "remote content").unwrap();

    let conflict = make_conflict(
        "test.txt",
        conflict_path.to_str().unwrap(),
        original_path.to_str().unwrap(),
        remote_path.to_str().unwrap(),
    );

    let status = make_status("res-test-unblock", "rule-unblock", vec![conflict]);
    let (tx, _rx) = watch::channel(status);

    // Verify initial state is blocked
    assert!(tx.borrow().remotes[0].sync_rules[0].state.is_blocked());

    let resolutions = vec![Resolution {
        remote: "res-test-unblock".to_string(),
        rule: "rule-unblock".to_string(),
        file: "test.txt".to_string(),
        action: ResolutionAction::KeepRemote,
        resolved_at: Utc::now(),
    }];

    let result = resolution_executor::apply(&resolutions, &tx).await;
    assert!(result.failed.is_empty());

    // After all conflicts resolved, state should be Idle
    let s = tx.borrow();
    let rule_status = &s.remotes[0].sync_rules[0];
    assert!(rule_status.conflicts.is_empty());
    assert_eq!(rule_status.state, SyncState::Idle);
    assert!(!rule_status.state.is_blocked());
}

// ─── Failed resolution preserved ───

#[tokio::test]
async fn failed_resolution_preserved() {
    let local_dir = TempDir::new().unwrap();

    // Set up a conflict where the remote path points to a nonexistent directory,
    // so rclone copyto will fail.
    let original_path = local_dir.path().join("test.txt");
    let conflict_path = local_dir.path().join("test.conflict-20260101T000000.txt");
    let invalid_remote_path = "/nonexistent-remote/bad/path/test.txt";

    fs::write(&original_path, "remote content").unwrap();
    fs::write(&conflict_path, "local content").unwrap();

    let conflict = make_conflict(
        "test.txt",
        conflict_path.to_str().unwrap(),
        original_path.to_str().unwrap(),
        invalid_remote_path,
    );

    let status = make_status("res-test-fail", "rule-fail", vec![conflict]);
    let (tx, _rx) = watch::channel(status);

    let resolutions = vec![Resolution {
        remote: "res-test-fail".to_string(),
        rule: "rule-fail".to_string(),
        file: "test.txt".to_string(),
        action: ResolutionAction::KeepLocal,
        resolved_at: Utc::now(),
    }];

    let result = resolution_executor::apply(&resolutions, &tx).await;

    // The resolution should fail because rclone copyto to an invalid path fails
    assert_eq!(
        result.failed.len(),
        1,
        "failed list should contain the resolution"
    );
    assert_eq!(result.failed[0].file, "test.txt");

    // The conflict should still be present in status (not removed on failure)
    let s = tx.borrow();
    let rule_status = &s.remotes[0].sync_rules[0];
    assert_eq!(rule_status.conflicts.len(), 1);
    assert_eq!(rule_status.conflicts[0].file, "test.txt");
}

// ─── Baseline update test ───

#[tokio::test]
async fn baseline_updated_after_resolution() {
    let local_dir = TempDir::new().unwrap();
    let remote_dir = TempDir::new().unwrap();
    // Each test uses its own unique remote+rule pair to avoid races with other tests
    // that share the same on-disk baseline directory.
    let _baseline = baseline_guard("res-test-baseline", "rule-baseline");

    let original_path = local_dir.path().join("test.txt");
    let conflict_path = local_dir.path().join("test.conflict-20260101T000000.txt");
    let remote_path = remote_dir.path().join("test.txt");

    fs::write(&original_path, "remote content").unwrap();
    fs::write(&conflict_path, "local content").unwrap();
    fs::write(&remote_path, "remote content").unwrap();

    let conflict = make_conflict(
        "test.txt",
        conflict_path.to_str().unwrap(),
        original_path.to_str().unwrap(),
        remote_path.to_str().unwrap(),
    );

    let status = make_status("res-test-baseline", "rule-baseline", vec![conflict]);
    let (tx, _rx) = watch::channel(status);

    let resolutions = vec![Resolution {
        remote: "res-test-baseline".to_string(),
        rule: "rule-baseline".to_string(),
        file: "test.txt".to_string(),
        action: ResolutionAction::KeepRemote,
        resolved_at: Utc::now(),
    }];

    let result = resolution_executor::apply(&resolutions, &tx).await;
    assert!(result.failed.is_empty(), "resolution should not fail");

    // resolution_executor writes the baseline via sync_baseline_file(), which uses
    // the real XDG data_dir. The _baseline guard cleans it up on drop.
    let baseline_path = sync_baseline_file("res-test-baseline", "rule-baseline");
    let baseline = SyncBaseline::load(&baseline_path);

    assert!(
        baseline.files.contains_key("test.txt"),
        "baseline should contain an entry for the resolved file"
    );
    let entry_time = baseline.files["test.txt"];
    let diff = (Utc::now() - entry_time).num_seconds().abs();
    assert!(
        diff < 10,
        "baseline timestamp should be recent (within 10s), got diff={diff}s"
    );
}
