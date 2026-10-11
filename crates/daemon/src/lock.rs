//! Single-instance enforcement.
//!
//! This replaces the old `PidLock`, which was a read-then-write with no locking
//! and no identity check. Two failure modes followed from that: two daemons
//! could both pass the check and start, and — much worse — a stale PID file
//! after a SIGKILL would refuse startup *forever* if the kernel recycled that
//! PID to any other process owned by the user, with systemd retrying every 10s.
//!
//! `flock` fixes both by construction. The kernel owns the lock, so it is
//! released when our file descriptor goes away for any reason at all, including
//! SIGKILL and panics. There is no such thing as a stale flock.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub enum LockError {
    /// Another live process holds the lock.
    Held {
        pid: Option<u32>,
        comm: Option<String>,
    },
    Io(std::io::Error),
}

impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Held {
                pid: Some(pid),
                comm: Some(comm),
            } => {
                write!(f, "onedrived is already running (pid {pid}, {comm})")
            }
            Self::Held {
                pid: Some(pid),
                comm: None,
            } => {
                write!(f, "onedrived is already running (pid {pid})")
            }
            Self::Held { .. } => f.write_str("onedrived is already running"),
            Self::Io(e) => write!(f, "cannot take the daemon lock: {e}"),
        }
    }
}

impl std::error::Error for LockError {}

/// Held for the lifetime of the daemon. Dropping it releases the lock.
#[derive(Debug)]
pub struct Lock {
    file: File,
    path: PathBuf,
}

impl Lock {
    /// Take the exclusive lock, or report who holds it.
    pub fn acquire(path: &Path) -> Result<Self, LockError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(LockError::Io)?;
        }

        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(LockError::Io)?;

        // SAFETY: `file` owns a valid fd for the duration of this call.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            return match err.raw_os_error() {
                Some(libc::EWOULDBLOCK) => {
                    // The holder wrote its pid into the file; read it so the
                    // message can name the process rather than just saying no.
                    let pid = read_pid(&mut file);
                    let comm = pid.and_then(process_name);
                    Err(LockError::Held { pid, comm })
                }
                _ => Err(LockError::Io(err)),
            };
        }

        // We own it. Record our pid for the benefit of whoever is refused next.
        file.set_len(0).map_err(LockError::Io)?;
        file.seek(SeekFrom::Start(0)).map_err(LockError::Io)?;
        write!(file, "{}", std::process::id()).map_err(LockError::Io)?;
        file.flush().map_err(LockError::Io)?;

        Ok(Self {
            file,
            path: path.to_owned(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        // Releasing is implicit in closing the fd; the file is left in place on
        // purpose so its contents stay readable for diagnostics. Unlinking it
        // would race with another daemon that already opened it.
        let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

fn read_pid(file: &mut File) -> Option<u32> {
    let mut text = String::new();
    file.seek(SeekFrom::Start(0)).ok()?;
    file.read_to_string(&mut text).ok()?;
    text.trim().parse().ok()
}

/// Read `/proc/<pid>/comm`, so a refusal can say *what* is holding the lock.
///
/// The old code trusted a bare PID with no identity check at all, which is what
/// made PID reuse able to wedge startup permanently.
fn process_name(pid: u32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|s| s.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquires_and_records_our_pid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.lock");

        let lock = Lock::acquire(&path).unwrap();
        assert_eq!(lock.path(), path);
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.trim().parse::<u32>().unwrap(), std::process::id());
    }

    #[test]
    fn creates_the_parent_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b/daemon.lock");
        let _lock = Lock::acquire(&path).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn releasing_allows_a_later_acquire() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.lock");

        drop(Lock::acquire(&path).unwrap());
        let _second = Lock::acquire(&path).expect("lock should be free again");
    }

    /// The headline property: a lock left behind by a dead process is not stale,
    /// because the kernel released it when the fd closed. The old PID-file
    /// design would refuse startup here if the PID had been recycled.
    #[test]
    fn a_lock_file_from_a_dead_process_does_not_block_startup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.lock");

        // A pid that is almost certainly not running, written as if by a crash.
        std::fs::write(&path, "4194303").unwrap();

        let lock = Lock::acquire(&path).expect("a stale lock file must not block startup");
        assert_eq!(
            std::fs::read_to_string(&path)
                .unwrap()
                .trim()
                .parse::<u32>()
                .unwrap(),
            std::process::id(),
        );
        drop(lock);
    }

    /// A second holder is refused, and the refusal names who has it.
    ///
    /// `flock` locks belong to the *open file description*, not the process, so
    /// two separate `acquire` calls conflict even inside one process — which is
    /// what makes this testable without spawning anything. An earlier version
    /// shelled out to `flock(1)` and so passed vacuously wherever util-linux was
    /// absent, such as the nix build sandbox.
    #[test]
    fn a_second_holder_is_refused_and_named() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.lock");

        let first = Lock::acquire(&path).expect("the first holder gets the lock");

        let err = Lock::acquire(&path).unwrap_err();
        match &err {
            LockError::Held { pid, comm } => {
                assert_eq!(
                    *pid,
                    Some(std::process::id()),
                    "the refusal should name the holder's pid"
                );
                assert!(
                    comm.is_some(),
                    "the refusal should name the holding process"
                );
            }
            other => panic!("expected Held, got {other:?}"),
        }
        assert!(
            err.to_string().starts_with("onedrived is already running"),
            "{err}"
        );

        // Once the first holder lets go, the lock is available again.
        drop(first);
        Lock::acquire(&path).expect("the lock should be free after the holder drops it");
    }

    #[test]
    fn reports_io_errors_rather_than_pretending_to_hold_the_lock() {
        // A directory where a file is expected.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("as-a-dir");
        std::fs::create_dir(&path).unwrap();

        let err = Lock::acquire(&path).unwrap_err();
        assert!(matches!(err, LockError::Io(_)), "{err:?}");
    }

    #[test]
    fn our_own_process_name_is_readable() {
        let name = process_name(std::process::id());
        assert!(name.is_some(), "/proc/self/comm should be readable");
        assert_eq!(process_name(4_194_303), None, "a dead pid has no comm");
    }
}
