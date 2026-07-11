use crate::rclone::{self, CopyMode};
use onedrive_mount::config::{LogConfig, MountConfig, RemoteConfig};

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

fn args_of(cmd: &std::process::Command) -> Vec<String> {
    cmd.get_args()
        .map(|a| a.to_string_lossy().to_string())
        .collect()
}

#[test]
fn mount_command_starts_with_rclone_mount() {
    let cmd = rclone::mount_command(&default_remote(), &default_log());
    let prog = cmd.get_program().to_string_lossy().to_string();
    assert_eq!(prog, "rclone");
    let args = args_of(&cmd);
    assert_eq!(args[0], "mount");
}

#[test]
fn mount_command_includes_remote_colon() {
    let cmd = rclone::mount_command(&default_remote(), &default_log());
    let args = args_of(&cmd);
    assert!(args.contains(&"onedrive:".to_string()));
}

#[test]
fn copy_command_normal_has_no_update_flag() {
    let cmd = rclone::copy_command("src:", "dst/", &[], CopyMode::Normal, false);
    let args = args_of(&cmd);
    assert!(!args.contains(&"--update".to_string()));
    assert!(!args.contains(&"--ignore-existing".to_string()));
}

#[test]
fn copy_command_update_has_update_flag() {
    let cmd = rclone::copy_command("src:", "dst/", &[], CopyMode::Update, false);
    let args = args_of(&cmd);
    assert!(args.contains(&"--update".to_string()));
}

#[test]
fn copy_command_ignore_existing_has_flag() {
    let cmd = rclone::copy_command("src:", "dst/", &[], CopyMode::IgnoreExisting, false);
    let args = args_of(&cmd);
    assert!(args.contains(&"--ignore-existing".to_string()));
}

#[test]
fn copy_command_uses_filter_not_include() {
    let patterns = vec!["*.kdbx".to_string(), "*.pdf".to_string()];
    let cmd = rclone::copy_command("src:", "dst/", &patterns, CopyMode::Normal, false);
    let args = args_of(&cmd);
    assert!(!args.contains(&"--include".to_string()));
    assert!(args.contains(&"--filter".to_string()));
    assert!(args.contains(&"+ *.kdbx".to_string()));
    assert!(args.contains(&"+ *.pdf".to_string()));
    assert!(args.contains(&"- *".to_string()));
}

#[test]
fn copy_command_exclude_conflicts() {
    let patterns = vec!["*.kdbx".to_string()];
    let cmd = rclone::copy_command("src:", "dst/", &patterns, CopyMode::Normal, true);
    let args = args_of(&cmd);
    assert!(args.contains(&"- *.conflict-*".to_string()));
}

#[test]
fn sync_command_has_sync_verb() {
    let cmd = rclone::sync_command("src:", "dst/", &["*.txt".to_string()]);
    let args = args_of(&cmd);
    assert_eq!(args[0], "sync");
    assert!(args.contains(&"--filter".to_string()));
}

#[test]
fn check_command_uses_differ_flag() {
    let cmd = rclone::check_command("remote:", "local/", &[]);
    let args = args_of(&cmd);
    assert!(args.contains(&"--differ".to_string()));
}

#[test]
fn check_command_uses_filter_not_include() {
    let patterns = vec!["*.kdbx".to_string()];
    let cmd = rclone::check_command("remote:", "local/", &patterns);
    let args = args_of(&cmd);
    assert!(!args.contains(&"--include".to_string()));
    assert!(args.contains(&"--filter".to_string()));
    assert!(args.contains(&"+ *.kdbx".to_string()));
}

#[test]
fn sanitize_rejects_metacharacters() {
    let mut remote = default_remote();
    remote.name = "evil;rm -rf /".into();
    let cmd = rclone::mount_command(&remote, &default_log());
    let args = args_of(&cmd);
    let remote_arg = &args[1];
    assert!(
        !remote_arg.contains(';'),
        "sanitized name must not contain ';'"
    );
}
