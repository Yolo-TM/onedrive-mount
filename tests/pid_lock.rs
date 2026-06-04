// Unit tests for the PID lock mechanism

use onedrive_mount::pid_lock::PidLock;
use tempfile::TempDir;

fn tmp_pid_path(dir: &TempDir) -> std::path::PathBuf {
    dir.path().join("test.pid")
}

#[test]
fn acquire_creates_pid_file() {
    let dir = TempDir::new().unwrap();
    let path = tmp_pid_path(&dir);

    let _lock = PidLock::acquire(&path).expect("should succeed on first acquire");
    assert!(path.exists(), "PID file should be created");

    let contents = std::fs::read_to_string(&path).unwrap();
    let pid: u32 = contents.trim().parse().unwrap();
    assert_eq!(pid, std::process::id());
}

#[test]
fn drop_removes_pid_file() {
    let dir = TempDir::new().unwrap();
    let path = tmp_pid_path(&dir);

    {
        let _lock = PidLock::acquire(&path).unwrap();
        assert!(path.exists());
    }
    assert!(!path.exists(), "PID file should be removed after drop");
}

#[test]
fn second_acquire_fails_while_first_held() {
    let dir = TempDir::new().unwrap();
    let path = tmp_pid_path(&dir);

    let _lock1 = PidLock::acquire(&path).unwrap();
    let result = PidLock::acquire(&path);
    assert!(result.is_err(), "second acquire should fail while first is held");
}

#[test]
fn second_acquire_succeeds_after_first_dropped() {
    let dir = TempDir::new().unwrap();
    let path = tmp_pid_path(&dir);

    {
        let _lock1 = PidLock::acquire(&path).unwrap();
    }
    // After drop the file is gone, so a new acquire should succeed
    let lock2 = PidLock::acquire(&path);
    assert!(lock2.is_ok(), "acquire should succeed after previous lock dropped");
}

#[test]
fn stale_pid_file_with_dead_pid_is_overwritten() {
    let dir = TempDir::new().unwrap();
    let path = tmp_pid_path(&dir);

    // Write a PID that is very unlikely to exist
    std::fs::write(&path, "999999999").unwrap();

    let lock = PidLock::acquire(&path);
    assert!(lock.is_ok(), "stale PID file with dead process should be overwritten");
}
