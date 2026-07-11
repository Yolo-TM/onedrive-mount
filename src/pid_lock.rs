use std::fs;
use std::path::{Path, PathBuf};

pub struct PidLock {
    path: PathBuf,
}

impl PidLock {
    pub fn acquire(path: &Path) -> Result<Self, u32> {
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }

        if let Ok(contents) = fs::read_to_string(path)
            && let Ok(pid) = contents.trim().parse::<u32>()
            && is_running(pid)
        {
            return Err(pid);
        }

        let our_pid = std::process::id();
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

fn is_running(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}
