// Integration tests for sync strategies using rclone local-to-local mode.
// These tests create real temp directories and run actual rclone commands
// to verify that each strategy moves files in the correct direction.
//
// Requires `rclone` to be on PATH.

use onedrive_mount::conflict::SyncStrategy;
use std::fs;
use std::process::Command;
use tempfile::TempDir;

/// Run an rclone command and assert it succeeds.
fn rclone_ok(cmd: &mut Command) {
    let output = cmd.output().expect("failed to spawn rclone");
    assert!(
        output.status.success(),
        "rclone failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Build an `rclone copy` command with filter args.
#[allow(clippy::suspicious_command_arg_space)]
fn rclone_copy(src: &str, dst: &str, patterns: &[&str]) -> Command {
    let mut cmd = Command::new("rclone");
    cmd.arg("copy").arg(src).arg(dst);
    add_filters(&mut cmd, patterns);
    cmd
}

/// Build an `rclone sync` command with filter args.
#[allow(clippy::suspicious_command_arg_space)]
fn rclone_sync(src: &str, dst: &str, patterns: &[&str]) -> Command {
    let mut cmd = Command::new("rclone");
    cmd.arg("sync").arg(src).arg(dst);
    add_filters(&mut cmd, patterns);
    cmd
}

/// Build an `rclone check` command.
#[allow(clippy::suspicious_command_arg_space)]
fn rclone_check(remote: &str, local: &str, patterns: &[&str]) -> Command {
    let mut cmd = Command::new("rclone");
    cmd.arg("check")
        .arg(remote)
        .arg(local)
        .arg("--differ")
        .arg("-");
    add_filters(&mut cmd, patterns);
    cmd
}

#[allow(clippy::suspicious_command_arg_space)]
fn add_filters(cmd: &mut Command, patterns: &[&str]) {
    for p in patterns {
        cmd.arg("--filter").arg(format!("+ {p}"));
    }
    cmd.arg("--filter").arg("- *");
}

fn write_file(dir: &std::path::Path, name: &str, contents: &str) {
    fs::write(dir.join(name), contents).unwrap();
}

fn read_file(dir: &std::path::Path, name: &str) -> String {
    fs::read_to_string(dir.join(name)).unwrap()
}

fn file_exists(dir: &std::path::Path, name: &str) -> bool {
    dir.join(name).exists()
}

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

/// Execute the bidirectional strategy using rclone commands.
#[allow(clippy::suspicious_command_arg_space)]
fn exec_bidirectional(local: &std::path::Path, remote: &std::path::Path) {
    let local_s = local.to_str().unwrap();
    let remote_s = remote.to_str().unwrap();
    let patterns = &["*"];

    // 1. Check for conflicts
    let output = rclone_check(remote_s, local_s, patterns).output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);

    // 2. Rename conflicting local files
    for relative_path in stdout.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let local_path = local.join(relative_path);
        if !local_path.exists() {
            continue;
        }
        let stem = local_path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let ext = local_path
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy()))
            .unwrap_or_default();
        let conflict_name = format!("{}.conflict-test{}", stem, ext);
        fs::rename(
            &local_path,
            local_path.parent().unwrap().join(conflict_name),
        )
        .unwrap();
    }

    // 3. Push local → remote (exclude conflict files)
    let mut copy_up = Command::new("rclone");
    copy_up.arg("copy").arg(local_s).arg(remote_s);
    copy_up.arg("--filter").arg("- *.conflict-*");
    add_filters(&mut copy_up, patterns);
    rclone_ok(&mut copy_up);

    // 4. Pull remote → local
    rclone_ok(&mut rclone_copy(remote_s, local_s, patterns));
}

/// Execute the newest_wins strategy using rclone commands.
fn exec_newest_wins(local: &std::path::Path, remote: &std::path::Path) {
    let local_s = local.to_str().unwrap();
    let remote_s = remote.to_str().unwrap();
    let patterns = &["*"];

    // 1. Push local-only new files
    rclone_ok(rclone_copy(local_s, remote_s, patterns).arg("--ignore-existing"));
    // 2. Pull remote-only new files
    rclone_ok(rclone_copy(remote_s, local_s, patterns).arg("--ignore-existing"));
    // 3. Push newer local files
    rclone_ok(rclone_copy(local_s, remote_s, patterns).arg("--update"));
    // 4. Pull newer remote files
    rclone_ok(rclone_copy(remote_s, local_s, patterns).arg("--update"));
}

/// Execute the mirror_down strategy.
fn exec_mirror_down(local: &std::path::Path, remote: &std::path::Path) {
    rclone_ok(&mut rclone_sync(
        remote.to_str().unwrap(),
        local.to_str().unwrap(),
        &["*"],
    ));
}

/// Execute the mirror_up strategy.
fn exec_mirror_up(local: &std::path::Path, remote: &std::path::Path) {
    rclone_ok(&mut rclone_sync(
        local.to_str().unwrap(),
        remote.to_str().unwrap(),
        &["*"],
    ));
}

fn exec_strategy(strategy: SyncStrategy, local: &std::path::Path, remote: &std::path::Path) {
    match strategy {
        SyncStrategy::Bidirectional => exec_bidirectional(local, remote),
        SyncStrategy::NewestWins => exec_newest_wins(local, remote),
        SyncStrategy::MirrorDown => exec_mirror_down(local, remote),
        SyncStrategy::MirrorUp => exec_mirror_up(local, remote),
    }
}

// ─── Bidirectional tests ───

#[test]
fn bidirectional_pushes_local_only_file_to_remote() {
    let local = TempDir::new().unwrap();
    let remote = TempDir::new().unwrap();

    write_file(local.path(), "new_local.txt", "local content");
    exec_strategy(SyncStrategy::Bidirectional, local.path(), remote.path());

    assert!(file_exists(remote.path(), "new_local.txt"));
    assert_eq!(read_file(remote.path(), "new_local.txt"), "local content");
}

#[test]
fn bidirectional_pulls_remote_only_file_to_local() {
    let local = TempDir::new().unwrap();
    let remote = TempDir::new().unwrap();

    write_file(remote.path(), "new_remote.txt", "remote content");
    exec_strategy(SyncStrategy::Bidirectional, local.path(), remote.path());

    assert!(file_exists(local.path(), "new_remote.txt"));
    assert_eq!(read_file(local.path(), "new_remote.txt"), "remote content");
}

#[test]
fn bidirectional_conflict_keeps_both_locally() {
    let local = TempDir::new().unwrap();
    let remote = TempDir::new().unwrap();

    write_file(local.path(), "shared.txt", "local version");
    write_file(remote.path(), "shared.txt", "remote version");

    exec_strategy(SyncStrategy::Bidirectional, local.path(), remote.path());

    // Remote version should be at the original filename locally
    assert_eq!(read_file(local.path(), "shared.txt"), "remote version");

    // Local version should be renamed to conflict file
    let files = list_files(local.path());
    let conflict_file = files
        .iter()
        .find(|f| f.contains(".conflict-"))
        .expect("conflict file should exist");
    let conflict_content = read_file(local.path(), conflict_file);
    assert_eq!(conflict_content, "local version");
}

#[test]
fn bidirectional_conflict_files_not_pushed_to_remote() {
    let local = TempDir::new().unwrap();
    let remote = TempDir::new().unwrap();

    write_file(local.path(), "shared.txt", "local version");
    write_file(remote.path(), "shared.txt", "remote version");

    exec_strategy(SyncStrategy::Bidirectional, local.path(), remote.path());

    // Remote should only have shared.txt, NOT the conflict file
    let remote_files = list_files(remote.path());
    assert_eq!(remote_files, vec!["shared.txt"]);
    assert_eq!(read_file(remote.path(), "shared.txt"), "remote version");
}

// ─── NewestWins tests ───

#[test]
fn newest_wins_pushes_local_only_file() {
    let local = TempDir::new().unwrap();
    let remote = TempDir::new().unwrap();

    write_file(local.path(), "only_local.txt", "local data");
    exec_strategy(SyncStrategy::NewestWins, local.path(), remote.path());

    assert!(file_exists(remote.path(), "only_local.txt"));
    assert_eq!(read_file(remote.path(), "only_local.txt"), "local data");
}

#[test]
fn newest_wins_pulls_remote_only_file() {
    let local = TempDir::new().unwrap();
    let remote = TempDir::new().unwrap();

    write_file(remote.path(), "only_remote.txt", "remote data");
    exec_strategy(SyncStrategy::NewestWins, local.path(), remote.path());

    assert!(file_exists(local.path(), "only_remote.txt"));
    assert_eq!(read_file(local.path(), "only_remote.txt"), "remote data");
}

#[test]
fn newest_wins_newer_local_overwrites_remote() {
    let local = TempDir::new().unwrap();
    let remote = TempDir::new().unwrap();

    // Create remote file first (older)
    write_file(remote.path(), "doc.txt", "old remote");
    // Sleep to ensure different mtime
    std::thread::sleep(std::time::Duration::from_secs(2));
    // Create local file second (newer)
    write_file(local.path(), "doc.txt", "new local");

    exec_strategy(SyncStrategy::NewestWins, local.path(), remote.path());

    assert_eq!(read_file(remote.path(), "doc.txt"), "new local");
    assert_eq!(read_file(local.path(), "doc.txt"), "new local");
}

// ─── MirrorDown tests ───

#[test]
fn mirror_down_pulls_remote_to_local() {
    let local = TempDir::new().unwrap();
    let remote = TempDir::new().unwrap();

    write_file(remote.path(), "from_remote.txt", "remote data");
    exec_strategy(SyncStrategy::MirrorDown, local.path(), remote.path());

    assert!(file_exists(local.path(), "from_remote.txt"));
    assert_eq!(read_file(local.path(), "from_remote.txt"), "remote data");
}

#[test]
fn mirror_down_deletes_local_only_files() {
    let local = TempDir::new().unwrap();
    let remote = TempDir::new().unwrap();

    write_file(local.path(), "local_only.txt", "should be deleted");
    write_file(remote.path(), "remote.txt", "keep this");

    exec_strategy(SyncStrategy::MirrorDown, local.path(), remote.path());

    assert!(
        !file_exists(local.path(), "local_only.txt"),
        "local-only file should be deleted"
    );
    assert!(file_exists(local.path(), "remote.txt"));
}

#[test]
fn mirror_down_does_not_push_to_remote() {
    let local = TempDir::new().unwrap();
    let remote = TempDir::new().unwrap();

    write_file(local.path(), "local_only.txt", "should not be pushed");
    exec_strategy(SyncStrategy::MirrorDown, local.path(), remote.path());

    assert!(!file_exists(remote.path(), "local_only.txt"));
}

// ─── MirrorUp tests ───

#[test]
fn mirror_up_pushes_local_to_remote() {
    let local = TempDir::new().unwrap();
    let remote = TempDir::new().unwrap();

    write_file(local.path(), "from_local.txt", "local data");
    exec_strategy(SyncStrategy::MirrorUp, local.path(), remote.path());

    assert!(file_exists(remote.path(), "from_local.txt"));
    assert_eq!(read_file(remote.path(), "from_local.txt"), "local data");
}

#[test]
fn mirror_up_deletes_remote_only_files() {
    let local = TempDir::new().unwrap();
    let remote = TempDir::new().unwrap();

    write_file(remote.path(), "remote_only.txt", "should be deleted");
    write_file(local.path(), "local.txt", "keep this");

    exec_strategy(SyncStrategy::MirrorUp, local.path(), remote.path());

    assert!(
        !file_exists(remote.path(), "remote_only.txt"),
        "remote-only file should be deleted"
    );
    assert!(file_exists(remote.path(), "local.txt"));
}

#[test]
fn mirror_up_does_not_pull_to_local() {
    let local = TempDir::new().unwrap();
    let remote = TempDir::new().unwrap();

    write_file(remote.path(), "remote_only.txt", "should not be pulled");
    exec_strategy(SyncStrategy::MirrorUp, local.path(), remote.path());

    assert!(!file_exists(local.path(), "remote_only.txt"));
}
