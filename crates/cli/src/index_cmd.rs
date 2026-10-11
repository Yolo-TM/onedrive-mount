//! `onedrive-tui ls` / `find` — offline, straight from the index.
//!
//! No daemon round-trip: these read the state database directly, the same way
//! `client::status_from_database` does when the daemon is silent. That is not
//! a fallback here, it is the design — the whole point of the offline index is
//! that listing it does not need anything running.

use std::fmt::Write as _;

use anyhow::{Context, Result};
use onedrive_core::config::Config;
use onedrive_core::paths::Paths;
use onedrive_core::state::{NodeRecord, Store};

/// Open the index read-only. A clear message if it does not exist yet, rather
/// than an empty listing that looks like the remote itself is empty.
fn open_index(paths: &Paths) -> Result<Store> {
    let db = paths.state_db();
    if !db.exists() {
        anyhow::bail!(
            "no offline index yet (no state at {})\n\
             run `onedrive-tui index refresh` once onedrived is running",
            db.display()
        );
    }
    Store::open_readonly(&db).with_context(|| format!("opening {}", db.display()))
}

/// Remote names to list against: every enabled remote, in config order.
fn enabled_remote_names(paths: &Paths) -> Vec<String> {
    Config::load(&paths.config_file())
        .unwrap_or_default()
        .enabled_remotes()
        .map(|r| r.name.clone())
        .collect()
}

/// `a/b/` → `a/b`; a leading `/` is also stripped, since the index's paths
/// never carry either.
fn normalize(path: &str) -> String {
    path.trim_matches('/').to_string()
}

fn format_entry(node: &NodeRecord) -> String {
    if node.is_dir {
        format!("{}/", node.path)
    } else {
        format!("{}\t{}", node.path, node.size)
    }
}

/// List the children of `path` (the root if `None`) across every enabled
/// remote.
pub fn ls(paths: &Paths, path: Option<&str>) -> Result<String> {
    let store = open_index(paths)?;
    let remotes = enabled_remote_names(paths);
    if remotes.is_empty() {
        return Ok("no enabled remotes configured\n".to_string());
    }
    let target = path.map(normalize).unwrap_or_default();

    let mut out = String::new();
    for remote in &remotes {
        if remotes.len() > 1 {
            let _ = writeln!(out, "== {remote} ==");
        }

        if !target.is_empty() {
            match store.node(remote, &target)? {
                Some(node) if !node.is_dir => {
                    let _ = writeln!(out, "{}: not a directory", node.path);
                    continue;
                }
                None => {
                    let _ = writeln!(
                        out,
                        "{target}: not in the index (try `onedrive-tui index refresh`)"
                    );
                    continue;
                }
                Some(_) => {}
            }
        }

        let children = store.children(remote, &target)?;
        if children.is_empty() {
            let _ = writeln!(out, "(empty)");
        }
        for child in children {
            let _ = writeln!(out, "{}", format_entry(&child));
        }
    }
    Ok(out)
}

/// Every indexed path whose name contains `pattern`, case-insensitively,
/// across every enabled remote.
pub fn find(paths: &Paths, pattern: &str) -> Result<String> {
    let store = open_index(paths)?;
    let remotes = enabled_remote_names(paths);
    if remotes.is_empty() {
        return Ok("no enabled remotes configured\n".to_string());
    }

    let mut out = String::new();
    let mut total = 0;
    for remote in &remotes {
        for node in store.search(remote, pattern)? {
            total += 1;
            if remotes.len() > 1 {
                let _ = writeln!(out, "{remote}: {}", format_entry(&node));
            } else {
                let _ = writeln!(out, "{}", format_entry(&node));
            }
        }
    }
    if total == 0 {
        let _ = writeln!(out, "no matches for {pattern:?}");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use onedrive_core::state::NodeInput;

    fn paths_with_index(home: &std::path::Path) -> Paths {
        let paths = Paths::rooted(home);
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        std::fs::write(
            paths.config_file(),
            "[[remote]]\nname = \"od\"\nmount_point = \"~/OneDrive\"\n",
        )
        .unwrap();

        let store = Store::open(&paths.state_db()).unwrap();
        store
            .upsert_nodes(
                "od",
                &[
                    NodeInput {
                        path: "Keepass".into(),
                        is_dir: true,
                        size: 0,
                        mtime: Utc::now(),
                        quickxor: None,
                    },
                    NodeInput {
                        path: "Keepass/Timm.kdbx".into(),
                        is_dir: false,
                        size: 76_046,
                        mtime: Utc::now(),
                        quickxor: None,
                    },
                    NodeInput {
                        path: "notes.txt".into(),
                        is_dir: false,
                        size: 5,
                        mtime: Utc::now(),
                        quickxor: None,
                    },
                ],
                1,
            )
            .unwrap();
        paths
    }

    #[test]
    fn ls_with_no_path_lists_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_with_index(dir.path());

        let out = ls(&paths, None).unwrap();
        assert!(out.contains("Keepass/"), "{out}");
        assert!(out.contains("notes.txt\t5"), "{out}");
        assert!(!out.contains("Timm.kdbx"), "only direct children, {out}");
    }

    #[test]
    fn ls_descends_into_a_given_path() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_with_index(dir.path());

        let out = ls(&paths, Some("Keepass")).unwrap();
        assert!(out.contains("Keepass/Timm.kdbx"), "{out}");

        // Leading/trailing slashes are forgiving.
        assert_eq!(out, ls(&paths, Some("/Keepass/")).unwrap());
    }

    #[test]
    fn ls_on_a_file_says_so_rather_than_listing_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_with_index(dir.path());

        let out = ls(&paths, Some("notes.txt")).unwrap();
        assert!(out.contains("not a directory"), "{out}");
    }

    #[test]
    fn ls_on_an_unindexed_path_explains_rather_than_claiming_its_empty() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_with_index(dir.path());

        let out = ls(&paths, Some("Nope")).unwrap();
        assert!(out.contains("not in the index"), "{out}");
        assert!(out.contains("index refresh"), "{out}");
    }

    #[test]
    fn find_matches_case_insensitively_and_reports_size() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_with_index(dir.path());

        let out = find(&paths, "TIMM").unwrap();
        assert!(out.contains("Keepass/Timm.kdbx\t76046"), "{out}");
    }

    #[test]
    fn find_with_no_matches_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_with_index(dir.path());

        let out = find(&paths, "zzz").unwrap();
        assert!(out.contains("no matches"), "{out}");
    }

    #[test]
    fn without_an_index_at_all_the_error_names_the_fix() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path());
        let err = ls(&paths, None).unwrap_err();
        assert!(format!("{err:#}").contains("index refresh"), "{err:#}");
    }
}
