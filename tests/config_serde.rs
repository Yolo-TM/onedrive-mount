// Integration tests: config roundtrip with real files

use onedrive_mount::config::{Config, LogConfig, MountConfig, RemoteConfig, SyncRule};
use onedrive_mount::conflict::ConflictStrategy;
use std::io::Write;
use tempfile::NamedTempFile;

fn make_full_config() -> Config {
    Config {
        remotes: vec![RemoteConfig {
            name: "onedrive".into(),
            r#type: "onedrive".into(),
            mount_point: "~/onedrive".into(),
            poll_interval: "30s".into(),
            enabled: true,
            mount: MountConfig {
                vfs_cache_mode: "full".into(),
                vfs_cache_max_age: "72h".into(),
                vfs_cache_max_size: "20G".into(),
                vfs_write_back: "5s".into(),
                transfers: 8,
                dir_cache_time: "15m".into(),
                extra_flags: vec!["--no-check-certificate".into()],
            },
            sync_rules: vec![SyncRule {
                name: "docs".into(),
                remote_path: "Files/docs".into(),
                local_path: "~/docs".into(),
                patterns: vec!["*.kdbx".into()],
                interval: "5m".into(),
                conflict_strategy: ConflictStrategy::RemoteWins,
                enabled: true,
            }],
        }],
        log: LogConfig {
            file: "~/.local/share/onedrive-mount/daemon.log".into(),
            level: "INFO".into(),
        },
    }
}

#[test]
fn roundtrip_full_config() {
    let config = make_full_config();
    let tmp = NamedTempFile::new().unwrap();

    config.save(tmp.path()).unwrap();
    let loaded = Config::load(tmp.path()).unwrap();

    assert_eq!(loaded.remotes.len(), 1);
    let r = &loaded.remotes[0];
    assert_eq!(r.name, "onedrive");
    assert_eq!(r.mount.transfers, 8);
    assert_eq!(r.sync_rules.len(), 1);
    assert_eq!(r.sync_rules[0].name, "docs");
    assert!(matches!(
        r.sync_rules[0].conflict_strategy,
        ConflictStrategy::RemoteWins
    ));
}

#[test]
fn defaults_applied_on_partial_toml() {
    let toml = r#"
[[remotes]]
name = "gdrive"
mount_point = "~/gdrive"
"#;
    let mut tmp = NamedTempFile::new().unwrap();
    tmp.write_all(toml.as_bytes()).unwrap();

    let config = Config::load(tmp.path()).unwrap();
    let r = &config.remotes[0];

    // Defaults should be filled in
    assert_eq!(r.r#type, "onedrive");
    assert!(r.enabled);
    assert!(!r.poll_interval.is_empty());
    assert!(r.sync_rules.is_empty());
}

#[test]
fn load_missing_file_returns_error() {
    let result = Config::load(std::path::Path::new("/nonexistent/path/config.toml"));
    assert!(result.is_err());
}

#[test]
fn multiple_remotes_roundtrip() {
    let mut config = make_full_config();
    config.remotes.push(RemoteConfig {
        name: "gdrive".into(),
        r#type: "drive".into(),
        mount_point: "~/gdrive".into(),
        poll_interval: "60s".into(),
        enabled: false,
        mount: Default::default(),
        sync_rules: vec![],
    });

    let tmp = NamedTempFile::new().unwrap();
    config.save(tmp.path()).unwrap();
    let loaded = Config::load(tmp.path()).unwrap();

    assert_eq!(loaded.remotes.len(), 2);
    assert_eq!(loaded.remotes[1].name, "gdrive");
    assert!(!loaded.remotes[1].enabled);
}
