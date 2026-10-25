//! The sync-safety matrix — the gate P2 has to pass.
//!
//! Every case asserts the same invariant: **no byte of either version is
//! unrecoverable**, and the canonical path the user opens still exists. These
//! run against the real `rclone bisync` with a local directory standing in for
//! OneDrive, so no credentials and no network are needed, but the code under
//! test is exactly the code that will touch the real database.
//!
//! Assertions are on *content*, not on file existence. The old engine's tests
//! passed while it was destroying data, partly because they checked that a file
//! was there rather than what was in it.
//!
//! The local stand-in differs from OneDrive in two ways worth naming: it has
//! nanosecond mtimes rather than 1-second, and its hash is not quickxor. Neither
//! affects what is being tested here — `--compare …,checksum` exercises the same
//! code path — and the mtime-tie case is forced explicitly rather than left to
//! chance.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{TimeZone, Utc};
use onedrive_rc_core::config::{Config, PinConfig};
use onedrive_rc_core::paths::Paths;
use onedrive_rcd::bisync::{self, Outcome, SENTINEL, VERSIONS_DIR};
use onedrive_rcd::{reconcile, snapshot};

/// Name of the stand-in remote defined in each fixture's own rclone config.
const REMOTE_NAME: &str = "stand-in";

/// A pin whose "remote" is a second local directory.
struct Fixture {
    _dir: tempfile::TempDir,
    paths: Paths,
    pin: PinConfig,
    local: PathBuf,
    remote: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        Self::with_pin_toml("")
    }

    fn with_pin_toml(extra: &str) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let local = root.join("pin");
        let remote = root.join("remote");
        std::fs::create_dir_all(&local).unwrap();
        std::fs::create_dir_all(&remote).unwrap();

        let config: Config = toml::from_str(&format!(
            r#"
            [[remote]]
            name = "{REMOTE_NAME}"
            mount_point = "{root}/OneDrive"
            [[remote.pin]]
            name = "Keepass"
            remote_path = "remote"
            local_path = "{local}"
            {extra}
            "#,
            root = root.display(),
            local = local.display(),
        ))
        .expect("fixture config should parse");

        let paths = Paths::rooted(root.join("home"));

        // Stand up a real rclone remote in the fixture's *own* config, so the
        // production code path is exercised unchanged — `remote_spec` builds
        // `stand-in:remote` exactly as it would for OneDrive. An `alias` remote
        // pointing at a directory is the local stand-in the plan calls for.
        //
        // This works because the daemon now passes `--config` explicitly rather
        // than letting rclone resolve it from the child's inherited HOME; without
        // that, these tests would silently use the developer's real rclone.conf.
        let rclone_conf = paths.rclone_config();
        std::fs::create_dir_all(rclone_conf.parent().unwrap()).unwrap();
        std::fs::write(
            &rclone_conf,
            format!(
                "[{REMOTE_NAME}]\ntype = alias\nremote = {}\n",
                root.display()
            ),
        )
        .unwrap();

        let pin = config.remotes[0].pins[0].clone();
        Self {
            _dir: dir,
            paths,
            pin,
            local,
            remote,
        }
    }

    fn remote_name(&self) -> &str {
        REMOTE_NAME
    }

    async fn sync(&self) -> Outcome {
        bisync::run(
            &self.pin,
            self.remote_name(),
            &self.paths,
            onedrive_rc_core::config::RcloneLogLevel::Notice,
        )
        .await
        .expect("bisync should not error out of band")
    }

    /// One full cycle, exactly as the daemon runs it: snapshot, sync, reconcile.
    async fn cycle(&self) -> (Outcome, Vec<reconcile::Conflict>) {
        let now = Utc::now();
        snapshot::take(&self.pin, &self.local, now)
            .await
            .expect("snapshot");
        let outcome = self.sync().await;
        let conflicts = reconcile::run(&self.local, now).await.expect("reconcile");
        (outcome, conflicts)
    }

    fn write_local(&self, name: &str, content: &str) {
        std::fs::write(self.local.join(name), content).unwrap();
    }

    fn write_remote(&self, name: &str, content: &str) {
        std::fs::write(self.remote.join(name), content).unwrap();
    }

    fn read_local(&self, name: &str) -> Option<String> {
        std::fs::read_to_string(self.local.join(name)).ok()
    }

    fn read_remote(&self, name: &str) -> Option<String> {
        std::fs::read_to_string(self.remote.join(name)).ok()
    }

    /// Every file content present anywhere we could recover from: both sides,
    /// plus the snapshots. Used to assert that nothing became unrecoverable.
    fn recoverable(&self) -> Vec<String> {
        let mut out = Vec::new();
        for root in [&self.local, &self.remote] {
            collect_contents(root, &mut out);
        }
        out.sort();
        out
    }

    fn files(&self, side: &Path) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        let Ok(entries) = std::fs::read_dir(side) else {
            return out;
        };
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == SENTINEL || name == VERSIONS_DIR {
                continue;
            }
            if let Ok(content) = std::fs::read_to_string(entry.path()) {
                out.insert(name, content);
            }
        }
        out
    }
}

fn collect_contents(root: &Path, out: &mut Vec<String>) {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if entry.file_name().to_string_lossy() != SENTINEL
                && let Ok(content) = std::fs::read_to_string(&path)
            {
                out.push(content);
            }
        }
    }
}

/// Assert a cycle succeeded, showing *why* if it did not. A bare `matches!`
/// assertion hides the refusal or error message, which is the only useful part.
#[track_caller]
fn expect_ok(outcome: Outcome) {
    match outcome {
        Outcome::Ok { .. } => {}
        Outcome::Refused { code, msg } => panic!("refused [{code}]: {msg}"),
        Outcome::Failed { code, msg } => panic!("failed [{code}]: {msg}"),
    }
}

fn rclone_available() -> bool {
    std::process::Command::new("rclone")
        .arg("version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Skip rather than fail where rclone is genuinely absent, but make it loud.
macro_rules! require_rclone {
    () => {
        if !rclone_available() {
            eprintln!("SKIPPED: rclone is not on PATH, so the safety matrix cannot run");
            return;
        }
    };
}

/// Force two files to share an mtime to the second, which is what makes
/// `--conflict-resolve newer` unable to pick a winner.
fn equalise_mtimes(a: &Path, b: &Path) {
    let when = filetime::FileTime::from_unix_time(1_800_000_000, 0);
    filetime::set_file_mtime(a, when).unwrap();
    filetime::set_file_mtime(b, when).unwrap();
}

/// Give two files whole-second mtimes `apart` seconds apart, `b` being newer.
///
/// Necessary because the local stand-in has nanosecond mtimes while OneDrive's
/// precision is exactly 1 second. With `--modify-window 1s`, sub-second
/// timestamps make `--conflict-resolve newer` unable to rank *any* pair — even
/// seconds apart — so a test that merely slept between writes would exercise the
/// tie path on every conflict and never the ordinary one. Setting whole seconds
/// explicitly models what OneDrive actually reports.
fn separate_mtimes(older: &Path, newer: &Path, apart: i64) {
    let base = 1_800_000_000;
    filetime::set_file_mtime(older, filetime::FileTime::from_unix_time(base, 0)).unwrap();
    filetime::set_file_mtime(newer, filetime::FileTime::from_unix_time(base + apart, 0)).unwrap();
}

// ===========================================================================
// The matrix
// ===========================================================================

#[tokio::test]
async fn local_only_change_propagates_up() {
    require_rclone!();
    let f = Fixture::new();
    f.write_local("Timm.kdbx", "v1");
    expect_ok(f.cycle().await.0);

    f.write_local("Timm.kdbx", "v2-local");
    let (outcome, conflicts) = f.cycle().await;

    assert!(matches!(outcome, Outcome::Ok { .. }), "{outcome:?}");
    assert!(conflicts.is_empty(), "a one-sided change is not a conflict");
    assert_eq!(f.read_remote("Timm.kdbx").as_deref(), Some("v2-local"));
    assert_eq!(f.read_local("Timm.kdbx").as_deref(), Some("v2-local"));
}

/// **The case the old engine destroyed on every cycle.** `sync_executor.rs:269`
/// treated "only the remote changed" as "not a conflict" and then pushed the
/// stale local copy over it.
#[tokio::test]
async fn remote_only_change_propagates_down() {
    require_rclone!();
    let f = Fixture::new();
    f.write_local("Timm.kdbx", "v1");
    expect_ok(f.cycle().await.0);

    f.write_remote("Timm.kdbx", "edited-on-another-machine");
    let (outcome, conflicts) = f.cycle().await;

    assert!(matches!(outcome, Outcome::Ok { .. }), "{outcome:?}");
    assert!(conflicts.is_empty());
    assert_eq!(
        f.read_local("Timm.kdbx").as_deref(),
        Some("edited-on-another-machine"),
        "a remote-only edit must reach the local side, not be overwritten by it"
    );
}

#[tokio::test]
async fn both_sides_changed_keeps_both_versions() {
    require_rclone!();
    let f = Fixture::new();
    f.write_local("Timm.kdbx", "v1");
    expect_ok(f.cycle().await.0);

    // Distinct whole-second mtimes, so rclone can pick a winner: remote is newer.
    f.write_local("Timm.kdbx", "local-entries");
    f.write_remote("Timm.kdbx", "remote-entries");
    separate_mtimes(&f.local.join("Timm.kdbx"), &f.remote.join("Timm.kdbx"), 60);

    let (outcome, conflicts) = f.cycle().await;
    assert!(matches!(outcome, Outcome::Ok { .. }), "{outcome:?}");

    // The canonical name exists and holds the newer content.
    assert_eq!(f.read_local("Timm.kdbx").as_deref(), Some("remote-entries"));

    // The loser is kept, timestamped, with its extension intact.
    assert_eq!(conflicts.len(), 1, "{conflicts:?}");
    let copy = &conflicts[0].copy;
    assert!(
        copy.ends_with(".kdbx"),
        "a conflict copy must stay openable: {copy}"
    );
    assert!(copy.contains("conflict-local"), "{copy}");
    assert_eq!(f.read_local(copy).as_deref(), Some("local-entries"));

    // Nothing is unrecoverable.
    let all = f.recoverable();
    assert!(all.contains(&"local-entries".to_string()), "{all:?}");
    assert!(all.contains(&"remote-entries".to_string()), "{all:?}");
}

/// **The case that would have reproduced the original bug.** With identical
/// mtimes `--conflict-resolve newer` suffixes both sides and leaves the
/// canonical path absent — KeePassXC would report its database missing.
#[tokio::test]
async fn equal_mtimes_still_leave_a_usable_database() {
    require_rclone!();
    let f = Fixture::new();
    f.write_local("Timm.kdbx", "v1");
    expect_ok(f.cycle().await.0);

    f.write_local("Timm.kdbx", "local-entries-and-more");
    f.write_remote("Timm.kdbx", "remote-entries");
    equalise_mtimes(&f.local.join("Timm.kdbx"), &f.remote.join("Timm.kdbx"));

    let (outcome, conflicts) = f.cycle().await;
    assert!(matches!(outcome, Outcome::Ok { .. }), "{outcome:?}");

    // The invariant that matters: the file the user opens exists.
    let canonical = f.read_local("Timm.kdbx");
    assert!(
        canonical.is_some(),
        "the canonical database must exist even when rclone picked no winner; \
         conflicts were {conflicts:?}"
    );

    // And both versions survived somewhere.
    let all = f.recoverable();
    assert!(
        all.contains(&"local-entries-and-more".to_string()),
        "{all:?}"
    );
    assert!(all.contains(&"remote-entries".to_string()), "{all:?}");

    assert!(
        conflicts.iter().any(|c| c.restored_canonical),
        "the restore should be reported so the user knows the winner was our choice"
    );
}

/// A delete propagates — which is correct — and produces no conflict copy, so
/// the snapshot is the only recovery path. This is why `.versions/` exists.
#[tokio::test]
async fn a_propagated_delete_is_recoverable_from_a_snapshot() {
    require_rclone!();
    let f = Fixture::new();
    f.write_local("Timm.kdbx", "the only copy");
    f.write_local("other.txt", "padding");
    f.write_local("more.txt", "padding");
    f.write_local("evenmore.txt", "padding");
    expect_ok(f.cycle().await.0);

    // One of four files, so well under max_delete_percent.
    std::fs::remove_file(f.local.join("Timm.kdbx")).unwrap();
    let (outcome, _) = f.cycle().await;
    assert!(matches!(outcome, Outcome::Ok { .. }), "{outcome:?}");

    assert_eq!(
        f.read_remote("Timm.kdbx"),
        None,
        "the delete should propagate"
    );
    assert_eq!(f.read_local("Timm.kdbx"), None);

    // But the content is still recoverable from the snapshot.
    let mut snapshots = Vec::new();
    collect_contents(&f.local.join(VERSIONS_DIR), &mut snapshots);
    assert!(
        snapshots.contains(&"the only copy".to_string()),
        "a propagated delete must leave a snapshot behind: {snapshots:?}"
    );
}

/// The catastrophic case: an expired token or a wrong remote path makes one side
/// look empty. bisync must refuse rather than mirror the emptiness.
#[tokio::test]
async fn an_empty_looking_remote_is_refused_with_no_deletions() {
    require_rclone!();
    let f = Fixture::new();
    f.write_local("Timm.kdbx", "precious");
    f.write_local("notes.txt", "also precious");
    expect_ok(f.cycle().await.0);

    let before = f.files(&f.local);
    assert_eq!(before.len(), 2);

    // Simulate the remote going away: everything gone, sentinel included.
    for entry in std::fs::read_dir(&f.remote).unwrap().filter_map(Result::ok) {
        let _ = std::fs::remove_file(entry.path());
    }

    let (outcome, _) = f.cycle().await;
    match outcome {
        Outcome::Refused { code, msg } => {
            assert_eq!(code, onedrive_rc_core::Code::CheckAccess);
            assert!(msg.contains("Nothing was changed"), "{msg}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }

    assert_eq!(
        f.files(&f.local),
        before,
        "a refusal must not touch the local side at all"
    );
}

/// Deleting most of a pin is the shape of an accident, so it is refused.
#[tokio::test]
async fn a_mass_deletion_is_refused() {
    require_rclone!();
    let f = Fixture::with_pin_toml("max_delete_percent = 10");
    for i in 0..20 {
        f.write_local(&format!("f{i}.txt"), &format!("content {i}"));
    }
    expect_ok(f.cycle().await.0);
    let before = f.files(&f.remote);

    for i in 0..20 {
        std::fs::remove_file(f.local.join(format!("f{i}.txt"))).unwrap();
    }

    let (outcome, _) = f.cycle().await;
    match outcome {
        Outcome::Refused { code, msg } => {
            assert_eq!(code, onedrive_rc_core::Code::CheckAccess);
            assert!(msg.contains("max_delete_percent"), "{msg}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert_eq!(f.files(&f.remote), before, "the remote must be untouched");
}

/// `.versions/` holds recovery copies and must never be uploaded.
#[tokio::test]
async fn snapshots_never_leave_the_machine() {
    require_rclone!();
    let f = Fixture::new();
    f.write_local("Timm.kdbx", "v1");
    f.cycle().await;
    f.write_local("Timm.kdbx", "v2");
    f.cycle().await;

    // There is something to leak.
    assert!(f.local.join(VERSIONS_DIR).is_dir());
    assert!(snapshot::size(&f.local).await > 0);

    assert!(
        !f.remote.join(VERSIONS_DIR).exists(),
        "remote contains: {:?}",
        std::fs::read_dir(&f.remote)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name())
            .collect::<Vec<_>>()
    );
}

/// An interrupted run must not demand manual intervention, and must not lose
/// anything. `--resilient --recover` is what makes the next run succeed.
#[tokio::test]
async fn an_interrupted_run_recovers_by_itself() {
    require_rclone!();
    let f = Fixture::new();
    f.write_local("Timm.kdbx", "v1");
    expect_ok(f.cycle().await.0);

    // Corrupt the workdir the way a kill -9 mid-write would.
    let workdir = f.paths.bisync_workdir(f.remote_name(), &f.pin.name);
    for entry in std::fs::read_dir(&workdir).unwrap().filter_map(Result::ok) {
        if entry.file_name().to_string_lossy().ends_with(".lst") {
            std::fs::write(entry.path(), b"truncated garbage").unwrap();
        }
    }

    f.write_local("Timm.kdbx", "v2-after-interruption");
    let (outcome, _) = f.cycle().await;

    assert!(
        matches!(outcome, Outcome::Ok { .. }),
        "the next run should recover without manual intervention: {outcome:?}"
    );
    let all = f.recoverable();
    assert!(
        all.contains(&"v2-after-interruption".to_string()),
        "{all:?}"
    );
}

/// A workdir wiped entirely is the first-run path, which must establish a
/// baseline rather than fail — and must not resolve it by discarding a side.
#[tokio::test]
async fn a_missing_baseline_resyncs_without_losing_the_newer_side() {
    require_rclone!();
    let f = Fixture::new();
    f.write_local("Timm.kdbx", "v1");
    expect_ok(f.cycle().await.0);

    let workdir = f.paths.bisync_workdir(f.remote_name(), &f.pin.name);
    std::fs::remove_dir_all(&workdir).unwrap();

    // The remote is now the newer side; `--resync-mode newer` must prefer it
    // rather than rclone's default of path1.
    f.write_remote("Timm.kdbx", "newer-remote-after-baseline-loss");
    separate_mtimes(&f.local.join("Timm.kdbx"), &f.remote.join("Timm.kdbx"), 60);

    let (outcome, _) = f.cycle().await;
    assert!(
        matches!(outcome, Outcome::Ok { resynced: true, .. }),
        "{outcome:?}"
    );
    assert_eq!(
        f.read_local("Timm.kdbx").as_deref(),
        Some("newer-remote-after-baseline-loss"),
        "a resync must not discard the newer side"
    );
}

/// Clock skew used to be able to invent changes and pick wrong winners. With
/// `--compare …,checksum` identical content is identical whatever the mtime.
#[tokio::test]
async fn clock_skew_does_not_invent_changes() {
    require_rclone!();
    let f = Fixture::new();
    f.write_local("Timm.kdbx", "identical");
    expect_ok(f.cycle().await.0);

    // Same content, mtime five minutes in the future.
    let skewed = filetime::FileTime::from_unix_time(Utc::now().timestamp() + 300, 0);
    filetime::set_file_mtime(f.local.join("Timm.kdbx"), skewed).unwrap();

    let (outcome, conflicts) = f.cycle().await;
    assert!(matches!(outcome, Outcome::Ok { .. }), "{outcome:?}");
    assert!(
        conflicts.is_empty(),
        "a pure mtime change must not be treated as a conflict: {conflicts:?}"
    );
    assert_eq!(f.read_local("Timm.kdbx").as_deref(), Some("identical"));
    assert_eq!(f.read_remote("Timm.kdbx").as_deref(), Some("identical"));
}

/// KeePassXC saves by writing a temp file and renaming over the database, and
/// leaves a lock file while open. Neither should ever be uploaded, and rapid
/// saves must not produce a torn copy.
#[tokio::test]
async fn the_keepass_save_pattern_is_safe() {
    require_rclone!();
    let f = Fixture::new();
    f.write_local("Timm.kdbx", "initial");
    expect_ok(f.cycle().await.0);

    for i in 0..20 {
        // Exactly what a password manager does: temp file, then rename.
        let tmp = f.local.join("Timm.kdbx.tmp");
        std::fs::write(&tmp, format!("entries-after-save-{i}")).unwrap();
        std::fs::rename(&tmp, f.local.join("Timm.kdbx")).unwrap();
        // And a lock file while the database is open.
        std::fs::write(f.local.join("Timm.kdbx.lock"), b"locked").unwrap();
    }

    let (outcome, _) = f.cycle().await;
    assert!(matches!(outcome, Outcome::Ok { .. }), "{outcome:?}");

    assert_eq!(
        f.read_remote("Timm.kdbx").as_deref(),
        Some("entries-after-save-19"),
        "the remote should hold the last complete save"
    );
    assert!(
        f.read_remote("Timm.kdbx.lock").is_none(),
        "a lock file must never be uploaded"
    );
    assert!(
        f.read_remote("Timm.kdbx.tmp").is_none(),
        "a temp file must never be uploaded"
    );
}

/// Several cycles with no changes must leave both sides byte-identical and not
/// accumulate anything. The old engine drifted here.
#[tokio::test]
async fn repeated_cycles_do_not_drift() {
    require_rclone!();
    let f = Fixture::new();
    f.write_local("Timm.kdbx", "stable");
    f.write_local("notes.txt", "also stable");
    expect_ok(f.cycle().await.0);

    let expected = f.files(&f.local);
    for i in 0..5 {
        let (outcome, conflicts) = f.cycle().await;
        assert!(
            matches!(outcome, Outcome::Ok { .. }),
            "cycle {i}: {outcome:?}"
        );
        assert!(conflicts.is_empty(), "cycle {i}: {conflicts:?}");
        assert_eq!(f.files(&f.local), expected, "local drifted on cycle {i}");
        assert_eq!(f.files(&f.remote), expected, "remote drifted on cycle {i}");
    }
}

/// Offline edits accumulate locally; when the sync finally runs, the last one
/// wins and the intermediates are still in the snapshots.
#[tokio::test]
async fn offline_edits_then_a_sync_keeps_the_latest_and_the_history() {
    require_rclone!();
    let f = Fixture::new();
    f.write_local("Timm.kdbx", "v0");
    expect_ok(f.cycle().await.0);

    // Three edits, each with its own snapshot, as the scheduler would produce.
    for i in 1..=3 {
        f.write_local("Timm.kdbx", &format!("v{i}"));
        snapshot::take(
            &f.pin,
            &f.local,
            Utc.timestamp_opt(1_800_000_000 + i * 60, 0).unwrap(),
        )
        .await
        .unwrap();
    }

    let (outcome, _) = f.cycle().await;
    assert!(matches!(outcome, Outcome::Ok { .. }), "{outcome:?}");
    assert_eq!(f.read_remote("Timm.kdbx").as_deref(), Some("v3"));

    let mut snapshots = Vec::new();
    collect_contents(&f.local.join(VERSIONS_DIR), &mut snapshots);
    for v in ["v0", "v1", "v2"] {
        assert!(
            snapshots.contains(&v.to_string()),
            "{v} should still be recoverable: {snapshots:?}"
        );
    }
}

/// A second conflict on the same file must not overwrite the first copy.
#[tokio::test]
async fn successive_conflicts_accumulate_rather_than_overwrite() {
    require_rclone!();
    let f = Fixture::new();
    f.write_local("Timm.kdbx", "v1");
    expect_ok(f.cycle().await.0);

    let mut kept = Vec::new();
    for round in 0..2 {
        f.write_local("Timm.kdbx", &format!("local-{round}"));
        f.write_remote("Timm.kdbx", &format!("remote-{round}"));
        separate_mtimes(
            &f.local.join("Timm.kdbx"),
            &f.remote.join("Timm.kdbx"),
            60 + round as i64,
        );

        let (outcome, conflicts) = f.cycle().await;
        assert!(
            matches!(outcome, Outcome::Ok { .. }),
            "round {round}: {outcome:?}"
        );
        assert_eq!(conflicts.len(), 1, "round {round}: {conflicts:?}");
        kept.push(conflicts[0].copy.clone());

        // A second conflict in the same wall-clock second would collide, so
        // step past it.
        std::thread::sleep(std::time::Duration::from_millis(1100));
    }

    assert_ne!(
        kept[0], kept[1],
        "each conflict needs its own name: {kept:?}"
    );
    for (round, name) in kept.iter().enumerate() {
        assert_eq!(
            f.read_local(name).as_deref(),
            Some(format!("local-{round}").as_str()),
            "the copy from round {round} must survive the next conflict"
        );
    }
}
