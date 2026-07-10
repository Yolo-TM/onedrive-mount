// Integration tests for Config::validate()

use onedrive_mount::config::{Config, MountConfig, RemoteConfig, SyncRule};
use onedrive_mount::conflict::SyncStrategy;

fn valid_remote(name: &str) -> RemoteConfig {
    RemoteConfig {
        name: name.into(),
        r#type: "onedrive".into(),
        mount_point: "~/onedrive".into(),
        poll_interval: "30s".into(),
        enabled: true,
        mount: MountConfig::default(),
        sync_rules: vec![],
    }
}

fn valid_rule(name: &str) -> SyncRule {
    SyncRule {
        name: name.into(),
        remote_path: "Docs".into(),
        local_path: "~/docs".into(),
        patterns: vec!["*".into()],
        interval: "5m".into(),
        sync_strategy: SyncStrategy::Bidirectional,
        enabled: true,
    }
}

#[test]
fn valid_config_has_no_errors() {
    let config = Config {
        remotes: vec![valid_remote("onedrive")],
        log: Default::default(),
    };
    assert!(config.validate().is_empty());
}

#[test]
fn empty_remote_name_is_invalid() {
    let mut config = Config {
        remotes: vec![valid_remote("")],
        log: Default::default(),
    };
    config.remotes[0].name = String::new();
    let errors = config.validate();
    assert!(!errors.is_empty());
    assert!(errors.iter().any(|e| e.contains("empty name")));
}

#[test]
fn empty_mount_point_is_invalid() {
    let mut r = valid_remote("test");
    r.mount_point = String::new();
    let config = Config {
        remotes: vec![r],
        log: Default::default(),
    };
    let errors = config.validate();
    assert!(errors.iter().any(|e| e.contains("mount point")));
}

#[test]
fn invalid_poll_interval_is_caught() {
    let mut r = valid_remote("test");
    r.poll_interval = "notaninterval".into();
    let config = Config {
        remotes: vec![r],
        log: Default::default(),
    };
    let errors = config.validate();
    assert!(errors.iter().any(|e| e.contains("poll interval")));
}

#[test]
fn valid_intervals_are_accepted() {
    for interval in &["30s", "5m", "2h", "1s"] {
        let mut r = valid_remote("test");
        r.poll_interval = interval.to_string();
        let config = Config {
            remotes: vec![r],
            log: Default::default(),
        };
        assert!(
            config.validate().is_empty(),
            "interval '{}' should be valid",
            interval
        );
    }
}

#[test]
fn rule_with_empty_local_path_is_invalid() {
    let mut rule = valid_rule("docs");
    rule.local_path = String::new();
    let mut r = valid_remote("test");
    r.sync_rules = vec![rule];
    let config = Config {
        remotes: vec![r],
        log: Default::default(),
    };
    let errors = config.validate();
    assert!(errors.iter().any(|e| e.contains("local path")));
}

#[test]
fn rule_with_empty_remote_path_is_invalid() {
    let mut rule = valid_rule("docs");
    rule.remote_path = String::new();
    let mut r = valid_remote("test");
    r.sync_rules = vec![rule];
    let config = Config {
        remotes: vec![r],
        log: Default::default(),
    };
    let errors = config.validate();
    assert!(errors.iter().any(|e| e.contains("remote path")));
}

#[test]
fn rule_with_invalid_interval_is_caught() {
    let mut rule = valid_rule("docs");
    rule.interval = "bad".into();
    let mut r = valid_remote("test");
    r.sync_rules = vec![rule];
    let config = Config {
        remotes: vec![r],
        log: Default::default(),
    };
    let errors = config.validate();
    assert!(errors.iter().any(|e| e.contains("interval")));
}

#[test]
fn multiple_errors_all_reported() {
    let mut r = valid_remote("");
    r.mount_point = String::new();
    let config = Config {
        remotes: vec![r],
        log: Default::default(),
    };
    let errors = config.validate();
    assert!(
        errors.len() >= 2,
        "expected at least 2 errors, got: {:?}",
        errors
    );
}
