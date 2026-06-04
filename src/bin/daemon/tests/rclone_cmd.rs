// Unit tests for rclone command builder

use onedrive_mount::config::{LogConfig, MountConfig, RemoteConfig};
use crate::rclone;

fn default_remote() -> RemoteConfig {
    RemoteConfig {
        name: "onedrive".into(),
        r#type: "onedrive".into(),
        mount_point: "~/onedrive".into(),
        poll_interval: "30s".into(),
        enabled: true,
        mount: MountConfig::default(),
        sync_rules: vec![],
    }
}

fn default_log() -> LogConfig {
    LogConfig {
        file: "~/.local/share/onedrive-mount/daemon.log".into(),
        level: "INFO".into(),
    }
}

#[test]
fn mount_command_starts_with_rclone_mount() {
    let cmd = rclone::mount_command(&default_remote(), &default_log());
    let prog = cmd.get_program().to_string_lossy().to_string();
    assert_eq!(prog, "rclone");
    let args: Vec<_> = cmd.get_args().map(|a| a.to_string_lossy().to_string()).collect();
    assert_eq!(args[0], "mount");
}

#[test]
fn mount_command_includes_remote_colon() {
    let cmd = rclone::mount_command(&default_remote(), &default_log());
    let args: Vec<_> = cmd.get_args().map(|a| a.to_string_lossy().to_string()).collect();
    assert!(args.contains(&"onedrive:".to_string()));
}

#[test]
fn copy_command_has_update_flag() {
    let cmd = rclone::copy_command("src:", "dst/", &[]);
    let args: Vec<_> = cmd.get_args().map(|a| a.to_string_lossy().to_string()).collect();
    assert!(args.contains(&"--update".to_string()));
}

#[test]
fn copy_command_includes_patterns() {
    let patterns = vec!["*.kdbx".to_string(), "*.pdf".to_string()];
    let cmd = rclone::copy_command("src:", "dst/", &patterns);
    let args: Vec<_> = cmd.get_args().map(|a| a.to_string_lossy().to_string()).collect();
    assert!(args.contains(&"--include".to_string()));
    assert!(args.contains(&"*.kdbx".to_string()));
    assert!(args.contains(&"*.pdf".to_string()));
}

#[test]
fn check_command_uses_differ_flag() {
    let cmd = rclone::check_command("remote:", "local/", &[]);
    let args: Vec<_> = cmd.get_args().map(|a| a.to_string_lossy().to_string()).collect();
    assert!(args.contains(&"--differ".to_string()));
}

#[test]
fn sanitize_rejects_metacharacters() {
    // Mount a remote whose name contains a shell metachar — should produce empty remote arg
    let mut remote = default_remote();
    remote.name = "evil;rm -rf /".into();
    let cmd = rclone::mount_command(&remote, &default_log());
    let args: Vec<_> = cmd.get_args().map(|a| a.to_string_lossy().to_string()).collect();
    // The remote arg should be ":" (empty name sanitized away, colon kept by format string)
    // or the entire arg starts with ":" meaning the name was blanked
    let remote_arg = &args[1]; // first arg after "mount"
    assert!(!remote_arg.contains(';'), "sanitized name must not contain ';'");
}
