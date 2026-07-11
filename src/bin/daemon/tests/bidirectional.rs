// Integration tests for the bidirectional sync flow through sync_executor::run().
//
// These tests use rclone's local-to-local mode.  sync_executor formats the
// rclone remote as "{remote_name}:{remote_path}", so passing ":local" as the
// remote_name and an absolute directory path as remote_path produces the string
// ":local:/abs/path" — rclone's anonymous :local: backend syntax.
//
// The baseline file is written to the real data_dir(), keyed by remote+rule
// name.  Each test uses a unique pair of names and registers a BaselineGuard
// that deletes the file when the test exits (success or failure).
//
// Requires `rclone` to be on PATH.

use chrono::Utc;
use onedrive_mount::{
    config::SyncRule,
    conflict::SyncStrategy,
    paths::sync_baseline_file,
    status::{DaemonStatus, MountState, RemoteStatus, SyncRuleStatus, SyncState},
    sync_baseline::SyncBaseline,
};
use std::ffi::CString;
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};
use tempfile::TempDir;
use tokio::sync::watch;

use crate::sync_executor;

// ─── helpers ───────────────────────────────────────────────────────────────────

/// The remote_name used for all tests.  Combined with an absolute path it
/// produces ":local:/abs/path" which is rclone's anonymous local backend.
const REMOTE_NAME: &str = ":local";

/// Build a minimal SyncRule for bidirectional sync.
/// `local_path` and `remote_path` are absolute filesystem paths.
fn make_rule(name: &str, local_path: &str, remote_path: &str) -> SyncRule {
    SyncRule {
        name: name.to_string(),
        remote_path: remote_path.to_string(),
        local_path: local_path.to_string(),
        patterns: vec!["*".to_string()],
        interval: "5m".to_string(),
        sync_strategy: SyncStrategy::Bidirectional,
        enabled: true,
    }
}

/// Build a DaemonStatus that has a slot for the given remote+rule so that
/// sync_executor can find and update it during conflict detection.
fn make_status(remote_name: &str, rule_name: &str) -> DaemonStatus {
    DaemonStatus {
        pid: std::process::id(),
        started_at: Some(Utc::now()),
        version: "test".to_string(),
        remotes: vec![RemoteStatus {
            name: remote_name.to_string(),
            mount: MountState::Unmounted,
            sync_rules: vec![SyncRuleStatus {
                name: rule_name.to_string(),
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

/// Set the mtime of a file to `now + offset_secs`.  This ensures the file
/// reads as clearly changed (or clearly unchanged) relative to a baseline that
/// was recorded at "now", given the 2-second tolerance in is_unchanged().
fn bump_mtime(path: &std::path::Path, offset_secs: i64) {
    let base = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let ts = base + offset_secs;
    let times = [
        libc::timeval {
            tv_sec: ts,
            tv_usec: 0,
        },
        libc::timeval {
            tv_sec: ts,
            tv_usec: 0,
        },
    ];
    let cpath = CString::new(path.to_str().unwrap()).unwrap();
    let rc = unsafe { libc::utimes(cpath.as_ptr(), times.as_ptr()) };
    assert_eq!(rc, 0, "utimes failed for {}", path.display());
}

/// List all regular-file names (not subdirectories) directly inside `dir`.
fn list_files(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|ft| ft.is_file()).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    names.sort();
    names
}

/// RAII guard that removes the baseline file when dropped, regardless of
/// whether the test passed or failed.
struct BaselineGuard {
    path: std::path::PathBuf,
}

impl Drop for BaselineGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn baseline_guard(remote_name: &str, rule_name: &str) -> BaselineGuard {
    BaselineGuard {
        path: sync_baseline_file(remote_name, rule_name),
    }
}

// ─── tests ─────────────────────────────────────────────────────────────────────

/// After the very first sync the baseline file must exist and contain entries
/// for every file that was synced.
#[tokio::test]
async fn first_sync_creates_baseline() {
    let local = TempDir::new().unwrap();
    let remote = TempDir::new().unwrap();

    let rule_name = "bidir-first-sync";
    let _guard = baseline_guard(REMOTE_NAME, rule_name);

    fs::write(local.path().join("local.txt"), "from local").unwrap();
    fs::write(remote.path().join("remote.txt"), "from remote").unwrap();

    let rule = make_rule(
        rule_name,
        local.path().to_str().unwrap(),
        remote.path().to_str().unwrap(),
    );
    let (tx, _rx) = watch::channel(make_status(REMOTE_NAME, rule_name));

    sync_executor::run(REMOTE_NAME, &rule, &tx)
        .await
        .expect("first sync should succeed");

    let baseline_path = sync_baseline_file(REMOTE_NAME, rule_name);
    assert!(
        baseline_path.exists(),
        "baseline file should be created after first sync"
    );

    let baseline = SyncBaseline::load(&baseline_path);
    assert!(
        !baseline.files.is_empty(),
        "baseline should contain file entries"
    );
    assert!(
        baseline.files.contains_key("local.txt"),
        "baseline should track local.txt"
    );
    assert!(
        baseline.files.contains_key("remote.txt"),
        "baseline should track remote.txt"
    );
}

/// A file edited locally (with a newer mtime than the baseline) should be
/// pushed to the remote on the next sync.
#[tokio::test]
async fn local_edit_syncs_to_remote() {
    let local = TempDir::new().unwrap();
    let remote = TempDir::new().unwrap();

    let rule_name = "bidir-local-edit";
    let _guard = baseline_guard(REMOTE_NAME, rule_name);

    fs::write(local.path().join("doc.txt"), "original content").unwrap();
    fs::write(remote.path().join("doc.txt"), "original content").unwrap();

    let rule = make_rule(
        rule_name,
        local.path().to_str().unwrap(),
        remote.path().to_str().unwrap(),
    );
    let (tx, _rx) = watch::channel(make_status(REMOTE_NAME, rule_name));

    // First sync — establishes the baseline.
    sync_executor::run(REMOTE_NAME, &rule, &tx)
        .await
        .expect("first sync should succeed");

    // Edit the local file and advance its mtime beyond the 2-second tolerance
    // so the baseline considers it changed.
    fs::write(local.path().join("doc.txt"), "edited locally").unwrap();
    bump_mtime(&local.path().join("doc.txt"), 5);

    // Second sync — should push the local change to the remote.
    sync_executor::run(REMOTE_NAME, &rule, &tx)
        .await
        .expect("second sync should succeed");

    let content = fs::read_to_string(remote.path().join("doc.txt")).unwrap();
    assert_eq!(
        content, "edited locally",
        "remote should have the locally-edited content"
    );
}

/// A file that exists only on the remote side is pulled to local during the
/// first (baseline-establishing) sync, and on subsequent syncs a remote-only
/// new file (i.e. one not present in the baseline) is also pulled.
#[tokio::test]
async fn remote_edit_syncs_to_local() {
    let local = TempDir::new().unwrap();
    let remote = TempDir::new().unwrap();

    let rule_name = "bidir-remote-edit";
    let _guard = baseline_guard(REMOTE_NAME, rule_name);

    // Start with one shared file.
    fs::write(local.path().join("shared.txt"), "shared").unwrap();
    fs::write(remote.path().join("shared.txt"), "shared").unwrap();
    // Remote also has an extra file that local doesn't.
    fs::write(remote.path().join("remote_only.txt"), "from remote").unwrap();

    let rule = make_rule(
        rule_name,
        local.path().to_str().unwrap(),
        remote.path().to_str().unwrap(),
    );
    let (tx, _rx) = watch::channel(make_status(REMOTE_NAME, rule_name));

    // First sync — both sides get merged and baseline is recorded.
    sync_executor::run(REMOTE_NAME, &rule, &tx)
        .await
        .expect("first sync should succeed");

    // The remote-only file must have been pulled to local.
    assert!(
        local.path().join("remote_only.txt").exists(),
        "remote_only.txt should have been pulled to local during first sync"
    );
    assert_eq!(
        fs::read_to_string(local.path().join("remote_only.txt")).unwrap(),
        "from remote"
    );

    // Now add a second remote-only file after the baseline.
    fs::write(remote.path().join("remote_only2.txt"), "also from remote").unwrap();

    // Second sync — new remote-only file should be pulled even though baseline exists.
    sync_executor::run(REMOTE_NAME, &rule, &tx)
        .await
        .expect("second sync should succeed");

    assert!(
        local.path().join("remote_only2.txt").exists(),
        "remote_only2.txt should be pulled to local on second sync"
    );
    assert_eq!(
        fs::read_to_string(local.path().join("remote_only2.txt")).unwrap(),
        "also from remote"
    );

    // No conflicts should have been generated.
    let status = tx.borrow();
    let rule_status = &status.remotes[0].sync_rules[0];
    assert!(
        !rule_status.state.is_blocked(),
        "rule should not be blocked after pulling remote-only files"
    );
}

/// When both sides change the same file after the baseline is established, the
/// next sync must detect a conflict: the status becomes BlockedOnConflicts, a
/// ConflictEntry is recorded, and the local file is renamed to .conflict-*.
#[tokio::test]
async fn both_edited_detects_conflict() {
    let local = TempDir::new().unwrap();
    let remote = TempDir::new().unwrap();

    let rule_name = "bidir-conflict-detect";
    let _guard = baseline_guard(REMOTE_NAME, rule_name);

    fs::write(local.path().join("shared.txt"), "original content").unwrap();
    fs::write(remote.path().join("shared.txt"), "original content").unwrap();

    let rule = make_rule(
        rule_name,
        local.path().to_str().unwrap(),
        remote.path().to_str().unwrap(),
    );
    let (tx, _rx) = watch::channel(make_status(REMOTE_NAME, rule_name));

    // First sync — establishes the baseline.
    sync_executor::run(REMOTE_NAME, &rule, &tx)
        .await
        .expect("first sync should succeed");

    // Edit BOTH sides with different content.  Advance mtimes well past the
    // 2-second is_unchanged() tolerance.
    fs::write(local.path().join("shared.txt"), "local version").unwrap();
    bump_mtime(&local.path().join("shared.txt"), 10);

    fs::write(remote.path().join("shared.txt"), "remote version").unwrap();
    bump_mtime(&remote.path().join("shared.txt"), 10);

    // Second sync — conflict detection runs; sync should return Ok but blocked.
    sync_executor::run(REMOTE_NAME, &rule, &tx)
        .await
        .expect("sync run should return Ok even when blocked on conflicts");

    let status = tx.borrow();
    let rule_status = &status.remotes[0].sync_rules[0];

    assert!(
        rule_status.state.is_blocked(),
        "rule should be BlockedOnConflicts, got {:?}",
        rule_status.state
    );
    assert_eq!(
        rule_status.conflicts.len(),
        1,
        "should have exactly one conflict entry"
    );
    assert_eq!(rule_status.conflicts[0].file, "shared.txt");

    // The local file should have been renamed to a .conflict-* file.
    let files = list_files(local.path());
    assert!(
        files.iter().any(|f| f.contains(".conflict-")),
        "a .conflict-* file should exist in local dir, found: {files:?}"
    );
    assert!(
        !files.iter().any(|f| f == "shared.txt"),
        "original shared.txt should have been renamed away, found: {files:?}"
    );
}

/// After a conflict is detected the push step must NOT run — the remote file
/// should still contain its own edited content, not the (now-renamed) local one.
#[tokio::test]
async fn sync_aborts_after_conflict() {
    let local = TempDir::new().unwrap();
    let remote = TempDir::new().unwrap();

    let rule_name = "bidir-abort-after-conflict";
    let _guard = baseline_guard(REMOTE_NAME, rule_name);

    fs::write(local.path().join("data.txt"), "original content").unwrap();
    fs::write(remote.path().join("data.txt"), "original content").unwrap();

    let rule = make_rule(
        rule_name,
        local.path().to_str().unwrap(),
        remote.path().to_str().unwrap(),
    );
    let (tx, _rx) = watch::channel(make_status(REMOTE_NAME, rule_name));

    // First sync — establishes the baseline.
    sync_executor::run(REMOTE_NAME, &rule, &tx)
        .await
        .expect("first sync should succeed");

    // Edit both sides.
    fs::write(local.path().join("data.txt"), "local edit").unwrap();
    bump_mtime(&local.path().join("data.txt"), 10);

    fs::write(remote.path().join("data.txt"), "remote edit").unwrap();
    bump_mtime(&remote.path().join("data.txt"), 10);

    // Second sync — conflict blocks the push step.
    sync_executor::run(REMOTE_NAME, &rule, &tx)
        .await
        .expect("sync should return Ok");

    // Remote must still have its own edit (the push step was skipped).
    let remote_content = fs::read_to_string(remote.path().join("data.txt")).unwrap();
    assert_eq!(
        remote_content, "remote edit",
        "remote should still have its own edit because the push step was aborted"
    );
}

/// A brand-new file added on the local side after the baseline is established
/// should be pushed to the remote without triggering any conflict handling.
#[tokio::test]
async fn new_file_not_flagged_as_conflict() {
    let local = TempDir::new().unwrap();
    let remote = TempDir::new().unwrap();

    let rule_name = "bidir-new-file";
    let _guard = baseline_guard(REMOTE_NAME, rule_name);

    // Start with one shared file so the baseline is non-empty.
    fs::write(local.path().join("existing.txt"), "exists on both").unwrap();
    fs::write(remote.path().join("existing.txt"), "exists on both").unwrap();

    let rule = make_rule(
        rule_name,
        local.path().to_str().unwrap(),
        remote.path().to_str().unwrap(),
    );
    let (tx, _rx) = watch::channel(make_status(REMOTE_NAME, rule_name));

    // First sync — baseline now tracks existing.txt.
    sync_executor::run(REMOTE_NAME, &rule, &tx)
        .await
        .expect("first sync should succeed");

    // Add a brand-new file only on local.
    fs::write(local.path().join("brand_new.txt"), "only local").unwrap();

    // Second sync — should push the new file without conflict detection.
    sync_executor::run(REMOTE_NAME, &rule, &tx)
        .await
        .expect("second sync should succeed");

    assert!(
        remote.path().join("brand_new.txt").exists(),
        "brand_new.txt should be pushed to remote"
    );
    assert_eq!(
        fs::read_to_string(remote.path().join("brand_new.txt")).unwrap(),
        "only local"
    );

    // No conflict files should exist anywhere.
    let local_files = list_files(local.path());
    assert!(
        !local_files.iter().any(|f| f.contains(".conflict-")),
        "no conflict files should exist in local dir, found: {local_files:?}"
    );

    let status = tx.borrow();
    let rule_status = &status.remotes[0].sync_rules[0];
    assert!(
        !rule_status.state.is_blocked(),
        "rule should not be blocked after adding a new file"
    );
}

/// Three consecutive syncs with no file changes must transfer zero files (after
/// the initial sync) and must never produce conflicts or baseline drift.
#[tokio::test]
async fn multiple_cycles_no_drift() {
    let local = TempDir::new().unwrap();
    let remote = TempDir::new().unwrap();

    let rule_name = "bidir-no-drift";
    let _guard = baseline_guard(REMOTE_NAME, rule_name);

    fs::write(local.path().join("a.txt"), "content a").unwrap();
    fs::write(remote.path().join("b.txt"), "content b").unwrap();

    let rule = make_rule(
        rule_name,
        local.path().to_str().unwrap(),
        remote.path().to_str().unwrap(),
    );
    let (tx, _rx) = watch::channel(make_status(REMOTE_NAME, rule_name));

    for cycle in 1_u32..=3 {
        let outcome = sync_executor::run(REMOTE_NAME, &rule, &tx)
            .await
            .unwrap_or_else(|e| panic!("sync cycle {cycle} failed: {e}"));

        // Cycle 1 copies the files across; subsequent cycles should transfer nothing.
        if cycle > 1 {
            assert_eq!(
                outcome.files_transferred, 0,
                "cycle {cycle}: expected 0 files transferred, got {}",
                outcome.files_transferred
            );
        }

        let status = tx.borrow();
        let rule_status = &status.remotes[0].sync_rules[0];
        assert!(
            !rule_status.state.is_blocked(),
            "cycle {cycle}: rule should not be blocked"
        );
        assert!(
            rule_status.conflicts.is_empty(),
            "cycle {cycle}: no conflicts expected"
        );
    }

    // Baseline should be stable and track both files.
    let baseline_path = sync_baseline_file(REMOTE_NAME, rule_name);
    let baseline = SyncBaseline::load(&baseline_path);
    assert!(
        baseline.files.contains_key("a.txt"),
        "baseline should track a.txt"
    );
    assert!(
        baseline.files.contains_key("b.txt"),
        "baseline should track b.txt"
    );
}
