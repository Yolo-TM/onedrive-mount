//! The offline index: what `onedrive-tui ls`/`find` and odfs's offline
//! directory listings read instead of the network.
//!
//! Two ways entries get in:
//!
//! - **Full crawl** ([`full_crawl`]) — `rclone lsjson --hash -R` over the whole
//!   remote, in one call. Every entry it returns is proof the path still
//!   exists; anything for this remote the crawl did *not* return is proof it
//!   does not, which is how a remote-side deletion reaches the index (see
//!   `Store::prune_nodes_before`).
//! - **Opportunistic updates** ([`record_listing`]) — odfs's own passthrough
//!   `readdir` already pays for a directory listing on every online browse;
//!   folding that into the index is free and keeps the paths actually in use
//!   fresher than the crawl interval alone would. These carry no hash (a
//!   `stat()` cannot produce one), so they never overwrite a hash a full crawl
//!   already recorded — see the `COALESCE` in `Store::upsert_nodes`.

use std::process::Stdio;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use onedrive_core::config::RemoteConfig;
use onedrive_core::paths::Paths;
use onedrive_core::state::{NodeInput, Store};
use serde::Deserialize;
use tokio::process::Command;
use tracing::{debug, info};

/// What a crawl did, for the log line and for `onedrive-tui index refresh`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CrawlStats {
    pub files: u64,
    pub dirs: u64,
    /// Entries removed because the crawl no longer saw them — the count that
    /// matters most: it is the only sign a remote-side deletion happened.
    pub removed: u64,
}

/// One `rclone lsjson` entry we care about. Extra fields are ignored.
#[derive(Debug, Deserialize)]
struct Entry {
    #[serde(rename = "Path")]
    path: String,
    #[serde(rename = "Size")]
    size: i64,
    #[serde(rename = "ModTime")]
    mod_time: DateTime<Utc>,
    #[serde(rename = "IsDir")]
    is_dir: bool,
    #[serde(rename = "Hashes", default)]
    hashes: Option<Hashes>,
}

#[derive(Debug, Deserialize, Default)]
struct Hashes {
    quickxor: Option<String>,
}

impl From<Entry> for NodeInput {
    fn from(e: Entry) -> Self {
        NodeInput {
            path: e.path,
            is_dir: e.is_dir,
            // rclone reports -1 for a directory's own size; everything else
            // is a real byte count.
            size: if e.is_dir { 0 } else { e.size.max(0) as u64 },
            mtime: e.mod_time,
            quickxor: e.hashes.and_then(|h| h.quickxor),
        }
    }
}

/// Parse `rclone lsjson`'s output. Pure, so the shape can be tested against a
/// fixed string without spawning anything.
fn parse_lsjson(bytes: &[u8]) -> Result<Vec<NodeInput>> {
    let entries: Vec<Entry> =
        serde_json::from_slice(bytes).context("parsing rclone lsjson output")?;
    Ok(entries.into_iter().map(NodeInput::from).collect())
}

/// Build the `rclone lsjson` argv for a full crawl. Pure for the same reason
/// `rclone::mount_args` is: the exact command line should be assertable.
fn crawl_args(remote: &RemoteConfig, paths: &Paths) -> Vec<String> {
    vec![
        "lsjson".into(),
        format!("{}:", remote.name),
        "--hash".into(),
        "-R".into(),
        format!("--config={}", paths.rclone_config().display()),
        format!("--tpslimit={}", remote.index.tpslimit),
        "--timeout=30s".into(),
        "--contimeout=10s".into(),
        "--low-level-retries=5".into(),
    ]
}

/// Run `rclone lsjson` over the whole remote. The network half of a crawl,
/// with no store access — split out from [`full_crawl`] so a caller whose
/// `Store` lives behind a lock it cannot hold across an `.await` (the daemon's
/// `App` is exactly this) can run the network call first and apply the result
/// afterwards, synchronously, via [`apply_crawl`].
pub async fn crawl_remote(remote: &RemoteConfig, paths: &Paths) -> Result<Vec<NodeInput>> {
    let args = crawl_args(remote, paths);
    debug!(remote = %remote.name, args = ?args, "running full index crawl");

    let output = Command::new("rclone")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .context("spawning rclone lsjson (is it on PATH?)")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        bail!("crawl of {} failed: {stderr}", remote.name);
    }

    parse_lsjson(&output.stdout)
}

/// Bring the index up to date with what a crawl found. The synchronous half
/// of a crawl — see [`crawl_remote`].
///
/// A failed crawl changes nothing: the old index stays in place and is still
/// better than no offline listing at all, which is why `crawl_remote` returns
/// an error rather than this function clearing anything on the way in.
pub fn apply_crawl(store: &Store, remote: &str, nodes: &[NodeInput]) -> Result<CrawlStats> {
    let stats = CrawlStats {
        files: nodes.iter().filter(|n| !n.is_dir).count() as u64,
        dirs: nodes.iter().filter(|n| n.is_dir).count() as u64,
        removed: 0,
    };

    // The new generation is committed only after a crawl that fully
    // succeeded, so a crawl that failed partway through never reaches here
    // and so can never prune anything.
    let generation = store.index_generation()? + 1;
    store.upsert_nodes(remote, nodes, generation)?;
    let removed = store.prune_nodes_before(remote, generation)?;
    store.set_index_generation(generation)?;
    store.meta_set("last_full_crawl", &Utc::now().timestamp().to_string())?;

    let stats = CrawlStats { removed, ..stats };
    info!(
        remote = %remote,
        files = stats.files,
        dirs = stats.dirs,
        removed = stats.removed,
        "full crawl complete"
    );
    Ok(stats)
}

/// Crawl the whole remote and bring the index up to date with what it found,
/// in one call. For callers that hold their `Store` directly rather than
/// behind a lock split across an await — the CLI, and these tests.
pub async fn full_crawl(remote: &RemoteConfig, paths: &Paths, store: &Store) -> Result<CrawlStats> {
    let nodes = crawl_remote(remote, paths).await?;
    apply_crawl(store, &remote.name, &nodes)
}

/// Fold a live directory listing into the index without a full crawl.
///
/// Called with whatever odfs already obtained for an online `readdir` or a
/// single `lookup`/`getattr` — no new network call of our own. Stamped with
/// the *current* generation rather than a new one: this is corroborating a
/// partial listing, not establishing a complete one, so it must never be used
/// to decide what to prune (see `Store::prune_nodes_before`).
pub fn record_listing(store: &Store, remote: &str, nodes: &[NodeInput]) -> Result<()> {
    if nodes.is_empty() {
        return Ok(());
    }
    let generation = store.index_generation()?;
    store.upsert_nodes(remote, nodes, generation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use onedrive_core::config::Config;

    fn remote() -> RemoteConfig {
        let config: Config = toml::from_str(
            r#"
            [[remote]]
            name = "onedrive-timm"
            mount_point = "~/OneDrive"
            "#,
        )
        .unwrap();
        config.remotes.into_iter().next().unwrap()
    }

    #[test]
    fn crawl_args_are_recursive_and_hashed() {
        let paths = Paths::rooted("/home/t");
        let joined = crawl_args(&remote(), &paths).join(" ");
        assert!(joined.starts_with("lsjson onedrive-timm: "), "{joined}");
        assert!(joined.contains("--hash"), "{joined}");
        assert!(joined.contains("-R"), "{joined}");
        assert!(joined.contains("--tpslimit=10"), "{joined}");
    }

    #[test]
    fn parses_a_real_lsjson_response() {
        let json = br#"[
            {"Path":"Keepass","Name":"Keepass","Size":-1,"MimeType":"inode/directory","ModTime":"2021-03-24T13:13:10Z","IsDir":true,"ID":"x"},
            {"Path":"Keepass/Timm.kdbx","Name":"Timm.kdbx","Size":76046,"MimeType":"application/octet-stream","ModTime":"2026-09-28T09:13:24Z","IsDir":false,"Hashes":{"quickxor":"9f0211bcc55a827e58328ceb95cd2075ab55b8c7"},"ID":"y"}
        ]"#;
        let nodes = parse_lsjson(json).unwrap();
        assert_eq!(nodes.len(), 2);

        let dir = &nodes[0];
        assert_eq!(dir.path, "Keepass");
        assert!(dir.is_dir);
        assert_eq!(
            dir.size, 0,
            "a directory's -1 size becomes 0, not -1 as u64"
        );
        assert!(dir.quickxor.is_none());

        let file = &nodes[1];
        assert_eq!(file.path, "Keepass/Timm.kdbx");
        assert!(!file.is_dir);
        assert_eq!(file.size, 76046);
        assert_eq!(
            file.quickxor.as_deref(),
            Some("9f0211bcc55a827e58328ceb95cd2075ab55b8c7")
        );
    }

    /// A backend with no hash support (every non-OneDrive remote, and the
    /// `alias` stand-in the integration tests use) must parse fine — the hash
    /// is informative, not load-bearing for the index itself.
    #[test]
    fn entries_with_no_hashes_at_all_parse_fine() {
        let json = br#"[{"Path":"a.txt","Name":"a.txt","Size":3,"ModTime":"2026-01-01T00:00:00Z","IsDir":false}]"#;
        let nodes = parse_lsjson(json).unwrap();
        assert_eq!(nodes.len(), 1);
        assert!(nodes[0].quickxor.is_none());
    }

    #[test]
    fn garbage_output_is_an_error_not_a_panic() {
        assert!(parse_lsjson(b"not json").is_err());
        assert!(parse_lsjson(b"").is_err());
    }

    #[tokio::test]
    async fn a_full_crawl_against_a_real_local_remote_populates_and_prunes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let remote_dir = root.join("remote");
        std::fs::create_dir_all(remote_dir.join("Keepass")).unwrap();
        std::fs::write(remote_dir.join("Keepass/Timm.kdbx"), "entries").unwrap();
        std::fs::write(remote_dir.join("notes.txt"), "hi").unwrap();

        let paths = Paths::rooted(root.join("home"));
        std::fs::create_dir_all(paths.rclone_config().parent().unwrap()).unwrap();
        std::fs::write(
            paths.rclone_config(),
            format!(
                "[stand-in]\ntype = alias\nremote = {}\n",
                remote_dir.display()
            ),
        )
        .unwrap();

        let config: Config = toml::from_str(&format!(
            r#"
            [[remote]]
            name = "stand-in"
            mount_point = "{root}/OneDrive"
            "#,
            root = root.display(),
        ))
        .unwrap();
        let remote = &config.remotes[0];

        let store = Store::open_memory().unwrap();
        let stats = full_crawl(remote, &paths, &store).await.unwrap();

        assert_eq!(stats.files, 2, "{stats:?}");
        assert_eq!(stats.dirs, 1, "{stats:?}");
        assert_eq!(stats.removed, 0);
        assert!(store.node("stand-in", "notes.txt").unwrap().is_some());
        assert!(
            store
                .node("stand-in", "Keepass/Timm.kdbx")
                .unwrap()
                .is_some()
        );
        assert_eq!(store.index_generation().unwrap(), 1);

        // Delete something upstream, then crawl again: the index must notice.
        std::fs::remove_file(remote_dir.join("notes.txt")).unwrap();
        let stats = full_crawl(remote, &paths, &store).await.unwrap();
        assert_eq!(stats.removed, 1, "{stats:?}");
        assert!(store.node("stand-in", "notes.txt").unwrap().is_none());
        assert!(
            store
                .node("stand-in", "Keepass/Timm.kdbx")
                .unwrap()
                .is_some(),
            "untouched entries must survive a later crawl"
        );
    }

    #[tokio::test]
    async fn a_crawl_of_a_remote_that_cannot_be_reached_leaves_the_index_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path().join("home"));
        std::fs::create_dir_all(paths.rclone_config().parent().unwrap()).unwrap();
        // No stand-in defined at all: rclone will fail to find the remote.
        std::fs::write(paths.rclone_config(), "").unwrap();

        let config: Config = toml::from_str(&format!(
            r#"
            [[remote]]
            name = "missing"
            mount_point = "{}/OneDrive"
            "#,
            dir.path().display(),
        ))
        .unwrap();

        let store = Store::open_memory().unwrap();
        store
            .upsert_nodes(
                "missing",
                &[NodeInput {
                    path: "already-here.txt".into(),
                    is_dir: false,
                    size: 1,
                    mtime: Utc::now(),
                    quickxor: None,
                }],
                1,
            )
            .unwrap();
        store.set_index_generation(1).unwrap();

        let err = full_crawl(&config.remotes[0], &paths, &store)
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("crawl of missing failed"),
            "{err:#}"
        );

        // Nothing was wiped, and the generation never advanced.
        assert!(store.node("missing", "already-here.txt").unwrap().is_some());
        assert_eq!(store.index_generation().unwrap(), 1);
    }

    #[test]
    fn an_opportunistic_listing_uses_the_current_generation_not_a_new_one() {
        let store = Store::open_memory().unwrap();
        store.set_index_generation(3).unwrap();

        record_listing(
            &store,
            "od",
            &[NodeInput {
                path: "Keepass/Timm.kdbx".into(),
                is_dir: false,
                size: 76_046,
                mtime: Utc::now(),
                quickxor: None,
            }],
        )
        .unwrap();

        // The generation counter itself is untouched: an opportunistic update
        // is not a crawl and must never let a later crawl's pruning treat it
        // as if a complete listing had just finished.
        assert_eq!(store.index_generation().unwrap(), 3);
        assert!(store.node("od", "Keepass/Timm.kdbx").unwrap().is_some());
    }

    /// The reason `record_listing` must never carry a new generation: if it
    /// bumped the counter, pruning after it would sweep away every path that
    /// merely was not part of that one small listing.
    #[tokio::test]
    async fn opportunistic_updates_never_cause_pruning() {
        let store = Store::open_memory().unwrap();
        store
            .upsert_nodes(
                "od",
                &[NodeInput {
                    path: "untouched.txt".into(),
                    is_dir: false,
                    size: 1,
                    mtime: Utc::now(),
                    quickxor: None,
                }],
                1,
            )
            .unwrap();
        store.set_index_generation(1).unwrap();

        record_listing(
            &store,
            "od",
            &[NodeInput {
                path: "Keepass/Timm.kdbx".into(),
                is_dir: false,
                size: 1,
                mtime: Utc::now(),
                quickxor: None,
            }],
        )
        .unwrap();

        // Pruning "before the current generation" must remove nothing: an
        // opportunistic update does not advance the generation.
        assert_eq!(store.prune_nodes_before("od", 1).unwrap(), 0);
        assert!(store.node("od", "untouched.txt").unwrap().is_some());
    }
}
