use std::process::Command;

pub fn sync_now(daemon_pid: u32) -> Result<(), String> {
    #[cfg(unix)]
    {
        let ret = unsafe { libc::kill(daemon_pid as libc::pid_t, libc::SIGUSR1) };
        if ret == 0 {
            Ok(())
        } else {
            Err(format!(
                "kill({daemon_pid}, SIGUSR1) failed: {}",
                std::io::Error::last_os_error()
            ))
        }
    }
    #[cfg(not(unix))]
    {
        let _ = daemon_pid;
        Err("sync_now not supported on this platform".into())
    }
}

pub fn delete_remote(name: &str) -> Result<(), String> {
    let status = Command::new("rclone")
        .args(["config", "delete", name])
        .status()
        .map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("rclone config delete {name} failed"))
    }
}

pub fn list_remotes() -> Vec<String> {
    let Ok(output) = Command::new("rclone").arg("listremotes").output() else {
        return vec![];
    };

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| line.trim_end_matches(':').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}
