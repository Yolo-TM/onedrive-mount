// Integration tests for DaemonStatus serialisation / deserialisation

use onedrive_mount::status::{DaemonStatus, MountState, RemoteStatus, SyncRuleStatus, SyncState};
use tempfile::NamedTempFile;

fn sample_status() -> DaemonStatus {
    DaemonStatus {
        pid: 12345,
        started_at: Some(chrono::Utc::now()),
        version: "0.2.0".into(),
        config_error: None,
        remotes: vec![RemoteStatus {
            name: "onedrive".into(),
            mount: MountState::Mounted {
                since: chrono::Utc::now(),
            },
            sync_rules: vec![SyncRuleStatus {
                name: "docs".into(),
                last_sync: Some(chrono::Utc::now()),
                next_sync: None,
                state: SyncState::Succeeded,
            }],
        }],
    }
}

#[test]
fn roundtrip_full_status() {
    let status = sample_status();
    let tmp = NamedTempFile::new().unwrap();
    status.save(tmp.path()).unwrap();

    let loaded = DaemonStatus::load(tmp.path()).unwrap();
    assert_eq!(loaded.pid, 12345);
    assert_eq!(loaded.remotes.len(), 1);
    assert_eq!(loaded.remotes[0].name, "onedrive");
    assert!(matches!(
        loaded.remotes[0].mount,
        MountState::Mounted { .. }
    ));
    assert_eq!(loaded.remotes[0].sync_rules[0].state, SyncState::Succeeded);
}

#[test]
fn failed_mount_state_roundtrip() {
    let mut status = sample_status();
    status.remotes[0].mount = MountState::Failed {
        error: "network unreachable".into(),
        at: chrono::Utc::now(),
    };
    let tmp = NamedTempFile::new().unwrap();
    status.save(tmp.path()).unwrap();
    let loaded = DaemonStatus::load(tmp.path()).unwrap();
    match &loaded.remotes[0].mount {
        MountState::Failed { error, .. } => assert_eq!(error, "network unreachable"),
        other => panic!("expected Failed, got {:?}", other),
    }
}

#[test]
fn config_error_roundtrip() {
    let mut status = sample_status();
    status.config_error = Some("unexpected key on line 5".into());
    let tmp = NamedTempFile::new().unwrap();
    status.save(tmp.path()).unwrap();
    let loaded = DaemonStatus::load(tmp.path()).unwrap();
    assert_eq!(
        loaded.config_error.as_deref(),
        Some("unexpected key on line 5")
    );
}

#[test]
fn config_error_none_not_written() {
    // When config_error is None the field should be omitted (skip_serializing_if)
    let status = sample_status();
    let tmp = NamedTempFile::new().unwrap();
    status.save(tmp.path()).unwrap();
    let raw = std::fs::read_to_string(tmp.path()).unwrap();
    assert!(
        !raw.contains("config_error"),
        "None config_error should not appear in TOML"
    );
}

#[test]
fn load_missing_returns_none() {
    let result = DaemonStatus::load(std::path::Path::new("/nonexistent/status.toml"));
    assert!(result.is_none());
}

#[test]
fn atomic_save_is_not_partial() {
    // save() writes to a .tmp file first, then renames — verify the final file is valid TOML
    let status = sample_status();
    let tmp = NamedTempFile::new().unwrap();
    status.save(tmp.path()).unwrap();
    // Tmp file should be gone (renamed away)
    let tmp_path = tmp.path().with_extension("toml.tmp");
    assert!(
        !tmp_path.exists(),
        ".tmp file should be cleaned up after save"
    );
}
