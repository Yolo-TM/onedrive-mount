// Single-instance enforcement via a PID file.
//
// On acquire: write our PID. If a file already exists and the PID in it belongs
// to a running process, return Err so the caller can exit early.
// On drop: remove the file so the next launch doesn't see a stale lock.

use std::fs;
use std::path::{Path, PathBuf};

pub struct PidLock {
    path: PathBuf,
}

impl PidLock {
    /// Try to acquire the lock at `path`.
    /// Returns `Err(existing_pid)` if another instance is already running.
    pub fn acquire(path: &Path) -> Result<Self, u32> {
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }

        // Check if a previous PID file exists and that process is still alive
        if let Ok(contents) = fs::read_to_string(path)
            && let Ok(pid) = contents.trim().parse::<u32>()
            && is_running(pid)
        {
            return Err(pid);
        }

        let our_pid = std::process::id();
        // Ignore write errors — better to run without a lock than to refuse to start
        let _ = fs::write(path, our_pid.to_string());

        Ok(Self {
            path: path.to_owned(),
        })
    }
}

impl Drop for PidLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Returns true if a process with this PID exists and is running.
fn is_running(pid: u32) -> bool {
    // Sending signal 0 checks existence without actually signalling the process.
    // On Linux this works for processes owned by the same user.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}
