//! The `rclone bisync` driver.
//!
//! Every flag here was checked against rclone 1.75.1 before being relied on;
//! §16 of `docs/OVERHAUL.md` records what the plan got wrong. Four things are
//! worth knowing when reading this file:
//!
//! 1. **The access sentinel must not be filtered.** Excluding `.onedrive-check`
//!    makes `--check-access` unable to find its own sentinel, and *every* run
//!    aborts with `Access test failed: Path1 count 0, Path2 count 0`.
//! 2. **`--max-delete` is a percentage**, not a file count, despite the name.
//! 3. **The conflict suffix is appended after the whole filename**, so a
//!    conflict on `Timm.kdbx` yields `Timm.kdbx.conflict-local` — extension
//!    lost, and a fixed name that the next conflict would overwrite.
//! 4. **`--conflict-resolve newer` does not guarantee a winner.** On an exact
//!    mtime tie it suffixes both sides and leaves the canonical path *absent*.
//!
//! (3) and (4) are why [`crate::reconcile`] runs after every sync. Relying on
//! the flag would have reproduced the original vanishing-database bug through a
//! different mechanism.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result};
use onedrive_core::Code;
use onedrive_core::config::{PinConfig, RcloneLogLevel};
use onedrive_core::paths::Paths;
use serde::Deserialize;
use tokio::process::Command;
use tracing::{debug, info, warn};

/// Upper bound on one bisync run. Generous because a first resync of a large
/// folder is genuinely slow, but bounded so a wedged run cannot hold the pin
/// lock forever.
const RUN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3600);

/// Name of the `--check-access` sentinel. Present on both sides, synced like any
/// other file, and deliberately *not* in the filter list.
pub const SENTINEL: &str = ".onedrive-check";

/// Directory holding pre-run snapshots, inside the pin and excluded from sync.
pub const VERSIONS_DIR: &str = ".versions";

/// rclone's fixed conflict suffixes. The reconciliation pass rewrites these into
/// timestamped names that keep the original extension.
pub const SUFFIX_LOCAL: &str = "conflict-local";
pub const SUFFIX_REMOTE: &str = "conflict-remote";

// ---------------------------------------------------------------------------
// Outcome
// ---------------------------------------------------------------------------

/// What one bisync run did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stats {
    pub transfers: u64,
    pub deletes: u64,
    pub renames: u64,
    pub errors: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The run completed. `resynced` records whether a baseline had to be
    /// established first.
    Ok { stats: Stats, resynced: bool },
    /// Refused for a reason that is *protective*, not a malfunction: the access
    /// sentinel was missing, or too many files would have been deleted. Nothing
    /// was changed, and the user has to look.
    Refused { code: Code, msg: String },
    /// Failed for an ordinary reason — offline, throttled, auth.
    Failed { code: Code, msg: String },
}

// ---------------------------------------------------------------------------
// Filters
// ---------------------------------------------------------------------------

/// Patterns excluded from every pin, before the user's own.
///
/// These are the files an editor or password manager creates *while* saving. The
/// old design uploaded them, which is how a half-written database could reach
/// the remote.
const ALWAYS_EXCLUDE: &[&str] = &[
    "- .versions/**",
    "- *.lock",
    "- *.kdbx.lock",
    "- *.tmp",
    "- *.partial",
    "- *~",
    "- .~lock.*",
    "- .goutputstream*",
    "- ~$*",
    "- .#*",
];

/// Build the filter file contents.
///
/// Note what is *not* here: the access sentinel. See the module docs.
pub fn filters(pin: &PinConfig) -> String {
    let mut out = String::new();
    for rule in ALWAYS_EXCLUDE {
        out.push_str(rule);
        out.push('\n');
    }

    // An empty pattern list means "everything", so only emit an include/exclude
    // pair when the user actually narrowed it.
    if !pin.patterns.is_empty() {
        for pattern in &pin.patterns {
            out.push_str("+ ");
            out.push_str(pattern.trim());
            out.push('\n');
        }
        // The sentinel has to survive a narrowing pattern list too, or
        // --check-access breaks.
        out.push_str("+ ");
        out.push_str(SENTINEL);
        out.push('\n');
        out.push_str("- *\n");
    }
    out
}

// ---------------------------------------------------------------------------
// Command construction
// ---------------------------------------------------------------------------

/// Build the argv for one bisync run.
///
/// Kept pure so the exact command can be asserted in tests — and so the safety
/// flags cannot be quietly dropped by a refactor.
pub fn args(
    pin: &PinConfig,
    remote: &str,
    paths: &Paths,
    filters_file: &Path,
    log_level: RcloneLogLevel,
    resync: bool,
) -> Vec<String> {
    let local = pin.local_path(paths);
    let mut args: Vec<String> = vec![
        "bisync".into(),
        local.to_string_lossy().into_owned(),
        pin.remote_spec(remote),
    ];

    // R2: content identity is a hash. This is what stops clock skew and
    // OneDrive's 1-second mtime precision from inventing changes.
    args.push("--compare".into());
    args.push("size,modtime,checksum".into());
    // OneDrive's ModTime precision is exactly 1s (verified via `rclone backend
    // features`), so anything finer would report phantom differences.
    args.push("--modify-window=1s".into());
    // OneDrive is case-insensitive.
    args.push("--ignore-case-sync".into());

    // R1: both versions are kept. The loser is renamed, never deleted.
    args.push("--conflict-resolve=newer".into());
    args.push("--conflict-loser=pathname".into());
    args.push(format!("--conflict-suffix={SUFFIX_LOCAL},{SUFFIX_REMOTE}"));

    // R3: refuse rather than guess. If one side looks empty — an expired token,
    // a wrong path — the sentinel is missing and bisync will not run.
    args.push("--check-access".into());
    args.push(format!("--check-filename={SENTINEL}"));
    args.push(format!("--max-delete={}", pin.max_delete_percent));

    // Survive an interruption without demanding a manual --resync.
    args.push("--resilient".into());
    args.push("--recover".into());
    args.push("--max-lock=2m".into());

    // Explicit rather than relying on the child's inherited HOME. See
    // `Paths::rclone_config`.
    args.push("--config".into());
    args.push(paths.rclone_config().to_string_lossy().into_owned());

    args.push("--workdir".into());
    args.push(
        paths
            .bisync_workdir(remote, &pin.name)
            .to_string_lossy()
            .into_owned(),
    );
    args.push("--filters-file".into());
    args.push(filters_file.to_string_lossy().into_owned());

    args.push("--timeout=30s".into());
    args.push("--contimeout=10s".into());
    args.push("--low-level-retries=5".into());

    if resync {
        args.push("--resync".into());
        // Prefer the newer side when establishing a baseline. `path1` (rclone's
        // default) would mean the first run could overwrite newer remote
        // content with whatever happens to be local.
        args.push("--resync-mode=newer".into());
    }

    args.push("--use-json-log".into());
    args.push(format!("--log-level={}", log_level.as_flag()));
    args
}

// ---------------------------------------------------------------------------
// Running
// ---------------------------------------------------------------------------

/// Prepare the workdir, the filter file and the access sentinels.
pub async fn prepare(pin: &PinConfig, remote: &str, paths: &Paths) -> Result<PathBuf> {
    let workdir = paths.bisync_workdir(remote, &pin.name);
    tokio::fs::create_dir_all(&workdir)
        .await
        .with_context(|| format!("creating {}", workdir.display()))?;

    let local = pin.local_path(paths);
    tokio::fs::create_dir_all(&local)
        .await
        .with_context(|| format!("creating {}", local.display()))?;

    let filters_file = paths.bisync_filters(remote, &pin.name);
    let wanted = filters(pin);
    // Only rewrite when the contents change: bisync hashes the filter file and
    // demands a --resync when it differs, so needless churn would force
    // needless resyncs.
    let current = tokio::fs::read_to_string(&filters_file)
        .await
        .unwrap_or_default();
    if current != wanted {
        tokio::fs::write(&filters_file, &wanted)
            .await
            .with_context(|| format!("writing {}", filters_file.display()))?;
        debug!(pin = %pin.name, "filter file updated");
    }

    let sentinel = local.join(SENTINEL);
    if !sentinel.exists() {
        tokio::fs::write(&sentinel, b"onedrive --check-access sentinel\n")
            .await
            .with_context(|| format!("writing {}", sentinel.display()))?;
    }

    Ok(filters_file)
}

/// Make sure the sentinel exists on the remote side as well.
///
/// `--check-access` requires it on *both* paths, and it cannot bootstrap itself:
/// on a brand-new pin the remote folder has no sentinel, so the very first run —
/// the resync, which is the most consequential one — would abort with
/// `Access test failed: Path1 count 1, Path2 count 0`. Copying it up first means
/// the access check protects even that run, instead of having to be disabled for
/// it.
async fn ensure_remote_sentinel(pin: &PinConfig, remote: &str, paths: &Paths) -> Result<()> {
    let local_sentinel = pin.local_path(paths).join(SENTINEL);
    let target = format!("{}/{SENTINEL}", pin.remote_spec(remote));

    let output = Command::new("rclone")
        .arg("copyto")
        .arg("--config")
        .arg(paths.rclone_config())
        .arg(&local_sentinel)
        .arg(&target)
        .arg("--use-json-log")
        .arg("--log-level=NOTICE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .context("running rclone copyto for the access sentinel")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "could not create the access sentinel on the remote: {}",
            last_error_line(&stderr).unwrap_or_else(|| stderr.trim().to_string())
        );
    }
    debug!(pin = %pin.name, "remote access sentinel in place");
    Ok(())
}

/// Whether a workdir has prior listings, i.e. whether a baseline exists.
///
/// Without one, bisync exits 7 with "cannot find prior Path1 or Path2
/// listings"; we check up front so the resync is entered deliberately rather
/// than as a reaction to a failure.
pub fn needs_resync(paths: &Paths, remote: &str, pin: &str) -> bool {
    let workdir = paths.bisync_workdir(remote, pin);
    let Ok(entries) = std::fs::read_dir(&workdir) else {
        return true;
    };
    !entries
        .filter_map(Result::ok)
        .any(|e| e.file_name().to_string_lossy().ends_with(".lst"))
}

/// Run bisync once, resyncing first if there is no baseline.
pub async fn run(
    pin: &PinConfig,
    remote: &str,
    paths: &Paths,
    log_level: RcloneLogLevel,
) -> Result<Outcome> {
    let filters_file = prepare(pin, remote, paths).await?;
    let resync = needs_resync(paths, remote, &pin.name);

    if resync {
        info!(
            pin = %pin.name,
            "no prior listings; establishing a baseline with --resync (newer side wins)"
        );
        // Must happen before the run: --check-access cannot bootstrap its own
        // sentinel, so without this the first resync aborts.
        if let Err(e) = ensure_remote_sentinel(pin, remote, paths).await {
            return Ok(Outcome::Failed {
                code: crate::rclone::classify(&e.to_string()).unwrap_or(Code::Internal),
                msg: format!("{e:#}"),
            });
        }
    }

    let outcome = run_once(pin, remote, paths, &filters_file, log_level, resync).await?;

    // A workdir can also be left unusable by an interruption that `--recover`
    // cannot repair. One retry with a resync, and only if we had not already
    // resynced, so this cannot loop.
    if !resync
        && let Outcome::Failed {
            code: Code::Internal,
            ref msg,
        } = outcome
        && wants_resync(msg)
    {
        warn!(
            pin = %pin.name,
            "bisync needs a new baseline; resyncing once (newer side wins)"
        );
        return run_once(pin, remote, paths, &filters_file, log_level, true).await;
    }

    Ok(outcome)
}

async fn run_once(
    pin: &PinConfig,
    remote: &str,
    paths: &Paths,
    filters_file: &Path,
    log_level: RcloneLogLevel,
    resync: bool,
) -> Result<Outcome> {
    let argv = args(pin, remote, paths, filters_file, log_level, resync);
    debug!(pin = %pin.name, ?argv, "running rclone bisync");

    let mut cmd = Command::new("rclone");
    cmd.args(&argv)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // A timed-out bisync must not keep running while anything else touches
        // the same tree. This is the defect that let two rclone processes copy
        // in opposite directions at once.
        .kill_on_drop(true);

    let output = match tokio::time::timeout(RUN_TIMEOUT, cmd.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => {
            return Ok(Outcome::Failed {
                code: Code::Dependency,
                msg: format!("could not run rclone: {e}"),
            });
        }
        Err(_) => {
            return Ok(Outcome::Failed {
                code: Code::Internal,
                msg: format!("bisync did not finish within {RUN_TIMEOUT:?}"),
            });
        }
    };

    // bisync writes its JSON log to stderr, same as `rclone mount`.
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    for line in stderr.lines() {
        emit(line, remote, &pin.name);
    }

    if output.status.success() {
        return Ok(Outcome::Ok {
            stats: parse_stats(&stderr),
            resynced: resync,
        });
    }

    Ok(classify_failure(&stderr, output.status))
}

/// Decide what a non-zero exit means.
///
/// The distinction that matters is `Refused` versus `Failed`: a refusal means
/// the safety machinery worked and a human should look, while a failure is
/// ordinary and will be retried.
fn classify_failure(stderr: &str, status: std::process::ExitStatus) -> Outcome {
    let lower = stderr.to_ascii_lowercase();
    let detail = last_error_line(stderr)
        .or_else(|| first_nonempty_line(stderr))
        .unwrap_or_else(|| format!("rclone bisync exited with {status}"));

    if lower.contains("access test failed") || lower.contains("check file check failed") {
        return Outcome::Refused {
            code: Code::CheckAccess,
            msg: format!(
                "refused to run: the {SENTINEL} sentinel is missing on one side, \
                 which usually means the remote path is wrong or the token expired. \
                 Nothing was changed. ({detail})"
            ),
        };
    }

    // A second, independent safety net: bisync refuses to sync *to* a side whose
    // listing has become entirely empty, before it even reaches the access
    // check. This is what actually fires when a remote vanishes mid-life.
    //
    // "current" and "prior" are very different things here and must not be
    // conflated: `current` is the live side, so an empty one means the remote is
    // gone and refusing is right. `prior` is *our own stored baseline*, so an
    // empty one means a damaged workdir — treating that as a vanished remote
    // would block syncing permanently behind a misleading message. The
    // `prior` case is handled by `wants_resync` below.
    if lower.contains("empty current path") {
        return Outcome::Refused {
            code: Code::CheckAccess,
            msg: format!(
                "refused to run: one side's listing came back empty, which usually means \
                 the remote path is wrong or the token expired. Nothing was changed. ({detail})"
            ),
        };
    }

    if lower.contains("too many deletes") {
        return Outcome::Refused {
            code: Code::CheckAccess,
            msg: format!(
                "refused to run: more than max_delete_percent of the files would have \
                 been deleted. Nothing was changed. ({detail})"
            ),
        };
    }

    // Anything rclone itself reported is classified the same way as for mounts.
    if let Some(code) = crate::rclone::classify(stderr) {
        return Outcome::Failed { code, msg: detail };
    }

    Outcome::Failed {
        code: Code::Internal,
        msg: detail,
    }
}

/// Whether a failure is the kind that a fresh baseline fixes.
fn wants_resync(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    lower.contains("must run --resync")
        || lower.contains("cannot find prior")
        || lower.contains("prior path1 or path2 listings")
        // A damaged baseline, as an interrupted run can leave behind. Our own
        // state, so rebuilding it is safe — unlike an empty *current* listing,
        // which means the live side is gone.
        || lower.contains("empty prior path")
}

/// Pull the most informative error line out of rclone's log.
///
/// The *first* one, deliberately: bisync follows a real error with generic
/// trailing lines ("Bisync aborted. Error is retryable...") that say nothing
/// about the cause, so taking the last would consistently report the least
/// useful message.
fn last_error_line(stderr: &str) -> Option<String> {
    stderr
        .lines()
        .filter_map(|line| serde_json::from_str::<LogLine>(line).ok())
        .find(|entry| matches!(entry.level.as_str(), "error" | "critical" | "emergency"))
        .map(|entry| strip_ansi(&entry.msg))
}

/// Falls back to this when stderr has content but none of it parses as a
/// JSON log line — e.g. a flag-parse error, which rclone prints as plain
/// text before its JSON logger is even set up (exactly what an
/// unsupported/too-old rclone produces: "Error: unknown flag:
/// --conflict-resolve", followed by a multi-line usage dump). Without this, a
/// real diagnostic message was silently replaced by a bare exit status, which
/// is what made a stale rclone take real effort to diagnose the one time it
/// actually happened. The *first* non-empty line, not the last: Cobra prints
/// the error before the usage text, the same ordering that makes
/// `last_error_line` prefer the first match over the last for JSON output.
fn first_nonempty_line(stderr: &str) -> Option<String> {
    stderr
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(strip_ansi)
}

/// rclone colourises some messages even with `--use-json-log`.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // Skip to the end of the escape sequence.
            for e in chars.by_ref() {
                if e.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out.trim().to_string()
}

// ---------------------------------------------------------------------------
// Log parsing
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct LogLine {
    #[serde(default)]
    level: String,
    #[serde(default)]
    msg: String,
    #[serde(default)]
    stats: Option<RcloneStats>,
}

#[derive(Debug, Deserialize)]
struct RcloneStats {
    #[serde(default)]
    transfers: u64,
    #[serde(default)]
    deletes: u64,
    #[serde(default)]
    renames: u64,
    #[serde(default)]
    errors: u64,
    #[serde(default)]
    bytes: u64,
}

/// Read the final stats object out of the log.
///
/// rclone emits one per run alongside its human summary, so this needs no prose
/// scraping — and the last one is authoritative.
pub fn parse_stats(stderr: &str) -> Stats {
    let mut stats = Stats::default();
    for line in stderr.lines() {
        if let Ok(entry) = serde_json::from_str::<LogLine>(line)
            && let Some(s) = entry.stats
        {
            stats = Stats {
                transfers: s.transfers,
                deletes: s.deletes,
                renames: s.renames,
                errors: s.errors,
                bytes: s.bytes,
            };
        }
    }
    stats
}

/// Re-emit one bisync log line into our own stream.
fn emit(line: &str, remote: &str, pin: &str) {
    let Ok(entry) = serde_json::from_str::<LogLine>(line) else {
        return;
    };
    // The stats line is reported through `Stats`; logging its embedded newlines
    // would wreck the column layout.
    if entry.stats.is_some() {
        return;
    }
    let msg = strip_ansi(&entry.msg);
    if msg.is_empty() {
        return;
    }
    let subject = format!("pin/{pin}");

    match entry.level.as_str() {
        "emergency" | "critical" | "error" => tracing::error!(
            source = "rclone", remote = %remote, pin = %pin, subject = %subject, "{msg}"
        ),
        "warning" => tracing::warn!(
            source = "rclone", remote = %remote, pin = %pin, subject = %subject, "{msg}"
        ),
        "notice" | "info" => tracing::info!(
            source = "rclone", remote = %remote, pin = %pin, subject = %subject, "{msg}"
        ),
        _ => tracing::debug!(
            source = "rclone", remote = %remote, pin = %pin, subject = %subject, "{msg}"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use onedrive_core::config::Config;

    fn pin() -> PinConfig {
        let config: Config = toml::from_str(
            r#"
            [[remote]]
            name = "od"
            mount_point = "~/OneDrive"
            [[remote.pin]]
            name = "Keepass"
            remote_path = "Dokumente/Keepass"
            local_path = "~/onedrive-keepass"
            "#,
        )
        .unwrap();
        config
            .remotes
            .into_iter()
            .next()
            .unwrap()
            .pins
            .into_iter()
            .next()
            .unwrap()
    }

    fn paths() -> Paths {
        Paths::rooted("/home/t")
    }

    #[test]
    fn builds_the_verified_command() {
        let argv = args(
            &pin(),
            "od",
            &paths(),
            Path::new("/w/filters.txt"),
            RcloneLogLevel::Notice,
            false,
        );

        assert_eq!(argv[0], "bisync");
        assert_eq!(argv[1], "/home/t/onedrive-keepass");
        assert_eq!(argv[2], "od:Dokumente/Keepass");

        let joined = argv.join(" ");
        for expected in [
            // R2 — a hash decides whether content differs.
            "--compare size,modtime,checksum",
            "--modify-window=1s",
            // R1 — the loser is kept, on both sides.
            "--conflict-resolve=newer",
            "--conflict-loser=pathname",
            "--conflict-suffix=conflict-local,conflict-remote",
            // R3 — refuse rather than guess.
            "--check-access",
            "--check-filename=.onedrive-check",
            "--max-delete=50",
            "--resilient",
            "--recover",
            "--ignore-case-sync",
            "--filters-file /w/filters.txt",
            "--use-json-log",
        ] {
            assert!(
                joined.contains(expected),
                "missing {expected} in:\n{joined}"
            );
        }
        assert!(!joined.contains("--resync"), "not a resync run: {joined}");
    }

    /// The first run has to prefer the newer side. rclone's default is `path1`,
    /// which would let a stale local copy overwrite newer remote content while
    /// establishing the baseline.
    #[test]
    fn a_resync_prefers_the_newer_side() {
        let argv = args(
            &pin(),
            "od",
            &paths(),
            Path::new("/w/f.txt"),
            RcloneLogLevel::Notice,
            true,
        );
        let joined = argv.join(" ");
        assert!(joined.contains("--resync"), "{joined}");
        assert!(joined.contains("--resync-mode=newer"), "{joined}");
    }

    #[test]
    fn max_delete_percent_is_passed_through() {
        let mut p = pin();
        p.max_delete_percent = 25;
        let joined = args(
            &p,
            "od",
            &paths(),
            Path::new("/w/f.txt"),
            RcloneLogLevel::Notice,
            false,
        )
        .join(" ");
        assert!(joined.contains("--max-delete=25"), "{joined}");
    }

    /// Excluding the sentinel makes `--check-access` fail to find it and aborts
    /// every run. This is the regression guard for that.
    #[test]
    fn filters_never_exclude_the_access_sentinel() {
        let text = filters(&pin());
        assert!(
            !text.contains(SENTINEL),
            "the sentinel must not appear in the filters at all:\n{text}"
        );
        assert!(text.contains("- .versions/**"), "{text}");
        assert!(text.contains("- *.kdbx.lock"), "{text}");
    }

    /// With a narrowing pattern list the sentinel needs an explicit include, or
    /// the trailing `- *` would exclude it.
    #[test]
    fn a_narrowed_pattern_list_still_admits_the_sentinel() {
        let mut p = pin();
        p.patterns = vec!["*.kdbx".into()];
        let text = filters(&p);

        assert!(text.contains("+ *.kdbx"), "{text}");
        assert!(
            text.contains(&format!("+ {SENTINEL}")),
            "the sentinel needs an explicit include when patterns narrow:\n{text}"
        );
        assert!(text.contains("- *"), "{text}");

        // And the include must come before the catch-all exclude.
        let include = text.find(&format!("+ {SENTINEL}")).unwrap();
        let exclude_all = text.rfind("- *\n").unwrap();
        assert!(include < exclude_all, "ordering is wrong:\n{text}");
    }

    #[test]
    fn an_empty_pattern_list_means_everything() {
        let text = filters(&pin());
        // No include/exclude pair at all, so nothing is narrowed.
        assert!(!text.contains("+ "), "{text}");
        assert!(!text.lines().any(|l| l == "- *"), "{text}");
    }

    #[test]
    fn needs_resync_when_there_are_no_prior_listings() {
        let dir = tempfile::tempdir().unwrap();
        let p = Paths::rooted(dir.path());
        assert!(
            needs_resync(&p, "od", "Keepass"),
            "a fresh workdir needs a resync"
        );

        let workdir = p.bisync_workdir("od", "Keepass");
        std::fs::create_dir_all(&workdir).unwrap();
        assert!(needs_resync(&p, "od", "Keepass"), "an empty workdir too");

        // A stray non-listing file is not a baseline.
        std::fs::write(workdir.join("filters.txt"), "- x").unwrap();
        assert!(needs_resync(&p, "od", "Keepass"));

        // The real thing: bisync leaves `.lst` listings behind.
        std::fs::write(workdir.join("local..remote.path1.lst"), "").unwrap();
        assert!(!needs_resync(&p, "od", "Keepass"));
    }

    // -- failure classification -------------------------------------------

    fn json(level: &str, msg: &str) -> String {
        format!(
            r#"{{"time":"2026-10-08T14:00:00Z","level":"{level}","msg":{}}}"#,
            serde_json::to_string(msg).unwrap()
        )
    }

    fn exit_status(code: i32) -> std::process::ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(code << 8)
    }

    /// A missing sentinel is the catastrophic case — an expired token or a wrong
    /// remote path — and must read as *refused*, not as an ordinary failure.
    #[test]
    fn a_missing_sentinel_is_a_refusal() {
        let log = [
            json(
                "error",
                "Access test failed: Path1 count 1, Path2 count 0 - .onedrive-check",
            ),
            json("error", "Bisync critical error: check file check failed"),
        ]
        .join("\n");

        match classify_failure(&log, exit_status(7)) {
            Outcome::Refused { code, msg } => {
                assert_eq!(code, Code::CheckAccess);
                assert!(msg.contains("Nothing was changed"), "{msg}");
                assert!(msg.contains("token expired"), "{msg}");
            }
            other => panic!("expected Refused, got {other:?}"),
        }
    }

    #[test]
    fn a_mass_deletion_is_a_refusal() {
        let log = json(
            "error",
            "Safety abort: too many deletes (>50%, 20 of 21) on Path1. Run with --force if desired.",
        );
        match classify_failure(&log, exit_status(1)) {
            Outcome::Refused { code, msg } => {
                assert_eq!(code, Code::CheckAccess);
                assert!(msg.contains("max_delete_percent"), "{msg}");
            }
            other => panic!("expected Refused, got {other:?}"),
        }
    }

    #[test]
    fn ordinary_failures_keep_their_rclone_code() {
        let log = json(
            "critical",
            "Failed to create file system: couldn't fetch token: oauth2: invalid_grant",
        );
        match classify_failure(&log, exit_status(1)) {
            Outcome::Failed { code, .. } => assert_eq!(code, Code::AuthExpired),
            other => panic!("expected Failed, got {other:?}"),
        }

        let log = json(
            "critical",
            "dial tcp: lookup graph.microsoft.com: no such host",
        );
        match classify_failure(&log, exit_status(1)) {
            Outcome::Failed { code, .. } => assert_eq!(code, Code::Offline),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    /// An empty *current* listing means the live side is gone, which is the
    /// catastrophic case and must be refused.
    #[test]
    fn an_empty_current_listing_is_a_refusal() {
        let log = json(
            "error",
            "Empty current Path2 listing. Cannot sync to an empty directory: /w/x.path2.lst-new",
        );
        match classify_failure(&log, exit_status(7)) {
            Outcome::Refused { code, msg } => {
                assert_eq!(code, Code::CheckAccess);
                assert!(msg.contains("Nothing was changed"), "{msg}");
            }
            other => panic!("expected Refused, got {other:?}"),
        }
    }

    /// An empty *prior* listing is our own damaged baseline, not a vanished
    /// remote. Refusing would block syncing permanently behind a message about
    /// the remote being gone, which would be both wrong and alarming.
    #[test]
    fn an_empty_prior_listing_asks_for_a_resync_rather_than_refusing() {
        let msg = "Empty prior Path1 listing. Cannot sync to an empty directory: /w/x.path1.lst";
        assert!(
            wants_resync(msg),
            "a damaged baseline should be rebuilt, not treated as a lost remote"
        );
        assert!(
            !matches!(
                classify_failure(&json("error", msg), exit_status(7)),
                Outcome::Refused { .. }
            ),
            "and it must not be classified as a refusal"
        );
    }

    /// bisync appends generic trailing lines after the real error; reporting the
    /// last one would consistently surface the least useful message.
    #[test]
    fn the_first_error_line_is_reported_not_the_last() {
        let log = [
            json(
                "error",
                "Empty current Path2 listing. Cannot sync to an empty directory",
            ),
            json(
                "error",
                "Bisync aborted. Error is retryable without --resync due to --resilient mode.",
            ),
        ]
        .join("\n");

        let detail = last_error_line(&log).unwrap();
        assert!(detail.starts_with("Empty current Path2"), "{detail}");
        assert!(!detail.contains("retryable"), "{detail}");
    }

    /// The case that actually happened: an rclone too old for a flag used here
    /// prints a plain-text usage error and exits 1 before its JSON logger
    /// exists at all, so `last_error_line` finds nothing. The fallback must
    /// still surface *that* message, not a bare exit status — otherwise the
    /// real cause (here, an outdated rclone in CI) is invisible.
    #[test]
    fn a_non_json_stderr_still_surfaces_its_real_message() {
        let log = "Error: unknown flag: --conflict-resolve\nUsage:\n  rclone bisync ...\n";
        match classify_failure(log, exit_status(1)) {
            Outcome::Failed { code, msg } => {
                assert_eq!(code, Code::Internal);
                assert!(msg.contains("unknown flag: --conflict-resolve"), "{msg}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    /// Truly empty stderr (the process produced nothing at all) is the one
    /// case left with no real message to show, so the bare status is correct.
    #[test]
    fn empty_stderr_falls_back_to_the_bare_status() {
        match classify_failure("", exit_status(1)) {
            Outcome::Failed { msg, .. } => assert!(msg.contains("exited with"), "{msg}"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn a_missing_baseline_asks_for_a_resync() {
        assert!(wants_resync(
            "Bisync critical error: cannot find prior Path1 or Path2 listings"
        ));
        assert!(wants_resync(
            "Bisync aborted. Must run --resync to recover."
        ));
        assert!(!wants_resync("Bisync successful"));
    }

    #[test]
    fn ansi_escapes_are_stripped_from_user_facing_messages() {
        let log = json(
            "error",
            "\u{1b}[31mBisync critical error: check file check failed\u{1b}[0m",
        );
        match classify_failure(&log, exit_status(7)) {
            Outcome::Refused { msg, .. } => {
                assert!(
                    !msg.contains('\u{1b}'),
                    "escapes leaked into the message: {msg:?}"
                );
                assert!(msg.contains("check file check failed"), "{msg}");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    /// The real stats line rclone emits, captured from a live run.
    #[test]
    fn parses_the_stats_object_rclone_emits() {
        let line = r#"{"time":"2026-10-08T14:11:58Z","level":"info","msg":"\nTransferred: 0 B\n","stats":{"bytes":123,"checks":4,"deletes":2,"errors":0,"renames":1,"transfers":3}}"#;
        let stats = parse_stats(line);
        assert_eq!(stats.transfers, 3);
        assert_eq!(stats.deletes, 2);
        assert_eq!(stats.renames, 1);
        assert_eq!(stats.errors, 0);
        assert_eq!(stats.bytes, 123);
    }

    #[test]
    fn stats_default_to_zero_when_absent() {
        assert_eq!(parse_stats("not json at all\n"), Stats::default());
        assert_eq!(
            parse_stats(&json("info", "Bisync successful")),
            Stats::default()
        );
    }

    #[tokio::test]
    async fn prepare_creates_the_workdir_filters_and_sentinel() {
        let dir = tempfile::tempdir().unwrap();
        let p = Paths::rooted(dir.path());
        let pin = pin();

        let filters_file = prepare(&pin, "od", &p).await.unwrap();

        assert!(filters_file.exists());
        assert!(p.bisync_workdir("od", "Keepass").is_dir());
        let local = pin.local_path(&p);
        assert!(local.is_dir());
        assert!(
            local.join(SENTINEL).exists(),
            "the local sentinel must exist or --check-access aborts"
        );
    }

    /// bisync hashes the filter file and demands a resync when it changes, so
    /// rewriting identical contents would force pointless resyncs.
    #[tokio::test]
    async fn prepare_does_not_rewrite_an_unchanged_filter_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = Paths::rooted(dir.path());
        let pin = pin();

        let filters_file = prepare(&pin, "od", &p).await.unwrap();
        let first = std::fs::metadata(&filters_file)
            .unwrap()
            .modified()
            .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        prepare(&pin, "od", &p).await.unwrap();

        let second = std::fs::metadata(&filters_file)
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(
            first, second,
            "an unchanged filter file must not be rewritten"
        );
    }

    #[tokio::test]
    async fn prepare_does_not_clobber_an_existing_sentinel() {
        let dir = tempfile::tempdir().unwrap();
        let p = Paths::rooted(dir.path());
        let pin = pin();

        let local = pin.local_path(&p);
        std::fs::create_dir_all(&local).unwrap();
        std::fs::write(local.join(SENTINEL), b"existing").unwrap();

        prepare(&pin, "od", &p).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(local.join(SENTINEL)).unwrap(),
            "existing"
        );
    }
}
