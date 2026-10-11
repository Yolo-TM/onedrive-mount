//! The shipped example config must always parse and validate.
//!
//! Easy to forget otherwise: `deny_unknown_fields` means a renamed setting turns
//! the documentation into a file that is actively rejected, and the example is
//! what `onedrive-tui setup` and the README both point people at.

use std::path::{Path, PathBuf};

use onedrive_core::config::Config;
use onedrive_core::paths::Paths;

fn example_path() -> PathBuf {
    // crates/core -> repo root
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("config.example.toml")
}

#[test]
fn example_config_parses_and_validates() {
    let path = example_path();
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));

    let config: Config =
        toml::from_str(&text).unwrap_or_else(|e| panic!("config.example.toml does not parse: {e}"));

    config
        .validate(&Paths::rooted("/home/example"))
        .unwrap_or_else(|e| panic!("config.example.toml does not validate:\n{}", e.msg));
}

/// Every value in the example is commented as being the default, so loading it
/// must produce exactly what omitting everything would.
#[test]
fn example_config_values_match_the_defaults() {
    let text = std::fs::read_to_string(example_path()).unwrap();
    let example: Config = toml::from_str(&text).unwrap();

    let minimal: Config = toml::from_str(
        r#"
        [[remote]]
        name = "onedrive-timm"
        mount_point = "~/OneDrive"

        [[remote.pin]]
        name = "Keepass"
        remote_path = "Dokumente/Keepass"
        local_path = "~/onedrive-keepass"
        "#,
    )
    .unwrap();

    assert_eq!(
        example, minimal,
        "config.example.toml claims to show defaults but differs from them"
    );
}

/// The example is also the documentation, so keep it honest about the settings
/// that exist to prevent the specific failures that motivated this rewrite.
#[test]
fn example_config_documents_the_load_bearing_settings() {
    let text = std::fs::read_to_string(example_path()).unwrap();

    for key in [
        "rclone_log_level",   // decoupled from our own level
        "log_retention_size", // retention actually enforced
        "vfs_cache_max_age",  // what keeps recent files offline
        "writes",             // offline write policy
        "conflict",           // both-sides-changed policy
        "versions",           // the propagated-delete recovery path
        "max_delete",
    ] {
        assert!(
            text.contains(key),
            "config.example.toml no longer documents {key:?}"
        );
    }
}
