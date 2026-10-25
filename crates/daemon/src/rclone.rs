//! Running rclone, and folding its log into ours.
//!
//! Three things here matter more than they look:
//!
//! 1. **`kill_on_drop(true)` on every child.** The old executor wrapped syncs in
//!    a 600s timeout but left `kill_on_drop` at its default of `false`, so a
//!    timed-out `rclone copy` kept running while the retry started *another* one
//!    in the opposite direction. Two rclone processes writing the same tree in
//!    opposite directions is a direct corruption path.
//! 2. **Flags are built from typed config values**, so an invalid duration or
//!    size is rejected at config load rather than making rclone exit instantly
//!    at startup.
//! 3. **rclone's log is captured and re-emitted into our stream.** rclone writes
//!    its JSON log to *stderr* when `--log-file` is omitted; `--log-file -` does
//!    not mean stdout, it creates a file literally named `-` (verified against
//!    rclone 1.75.1). Capturing it is what collapses the old two-files-two-formats
//!    situation into one greppable timeline.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result};
use onedrive_rc_core::Code;
use onedrive_rc_core::config::{RcloneLogLevel, RemoteConfig};
use onedrive_rc_core::paths::Paths;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tracing::{debug, error, info, warn};

/// How long to wait for a `rclone rc` call before giving up. The rc server is
/// local, so anything slower than this means the mount is wedged.
const RC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

// ---------------------------------------------------------------------------
// Command construction
// ---------------------------------------------------------------------------

/// Build the `rclone mount` argv for a remote.
///
/// Returns the arguments only; the caller adds the program. Kept pure so the
/// exact command line can be asserted in tests — the old code built a `Command`
/// directly, which made the flags untestable.
pub fn mount_args(
    remote: &RemoteConfig,
    paths: &Paths,
    mount_point: &Path,
    log_level: RcloneLogLevel,
) -> Vec<String> {
    let m = &remote.mount;
    let mut args: Vec<String> = vec![
        "mount".into(),
        format!("{}:", remote.name),
        mount_point.to_string_lossy().into_owned(),
    ];

    // Explicit rather than relying on the child's inherited HOME, so a daemon
    // started with a different HOME cannot silently fail to find the tokens.
    args.push(format!("--config={}", paths.rclone_config().display()));

    let cache_dir = remote.rclone_cache_dir(paths);
    args.push(format!("--cache-dir={}", cache_dir.display()));

    args.push(format!("--vfs-cache-mode={}", m.vfs_cache_mode.as_flag()));
    args.push(format!("--vfs-cache-max-age={}", m.vfs_cache_max_age));
    args.push(format!("--vfs-cache-max-size={}", m.vfs_cache_max_size));
    args.push(format!(
        "--vfs-cache-min-free-space={}",
        m.vfs_cache_min_free_space
    ));
    args.push(format!("--vfs-write-back={}", m.vfs_write_back));
    args.push(format!("--vfs-read-chunk-size={}", m.vfs_read_chunk_size));
    args.push(format!("--dir-cache-time={}", m.dir_cache_time));
    args.push(format!("--poll-interval={}", m.poll_interval));
    args.push(format!("--transfers={}", m.transfers));
    args.push(format!("--checkers={}", m.checkers));
    args.push(format!("--buffer-size={}", m.buffer_size));

    // Control channel, on a unix socket rather than a TCP port: nothing else on
    // the machine can reach it, and there is no port to collide with.
    args.push("--rc".into());
    args.push(format!(
        "--rc-addr=unix://{}",
        rc_socket(paths, &remote.name).display()
    ));
    args.push("--rc-no-auth".into());

    // Network timeouts, so a dead connection surfaces as an error we can act on
    // instead of hanging indefinitely.
    args.push("--timeout=30s".into());
    args.push("--contimeout=10s".into());
    args.push("--low-level-retries=5".into());

    // Structured log on stderr. Deliberately no --log-file: see the module docs.
    args.push("--use-json-log".into());
    args.push(format!("--log-level={}", log_level.as_flag()));

    args.extend(m.extra_flags.iter().cloned());
    args
}

/// Where a remote's rc control socket lives.
///
/// Delegates to [`Paths`] so it shares the one length-guarded socket directory:
/// putting it under the cache made the address long enough to overflow
/// `sun_path`, and rclone's only report of that was `bind: invalid argument`
/// followed by an immediate exit.
pub fn rc_socket(paths: &Paths, remote: &str) -> PathBuf {
    paths.rc_socket(remote)
}

// ---------------------------------------------------------------------------
// Spawning
// ---------------------------------------------------------------------------

/// Spawn `rclone mount` and start folding its log into ours.
///
/// The returned [`Child`] is killed if dropped, so losing track of it is not
/// possible.
pub async fn spawn_mount(
    remote: &RemoteConfig,
    paths: &Paths,
    mount_point: &Path,
    log_level: RcloneLogLevel,
) -> Result<Child> {
    tokio::fs::create_dir_all(mount_point)
        .await
        .with_context(|| format!("creating {}", mount_point.display()))?;
    tokio::fs::create_dir_all(remote.rclone_cache_dir(paths))
        .await
        .context("creating the rclone cache directory")?;

    // A socket left behind by a killed rclone makes the new one fail to bind.
    let socket = rc_socket(paths, &remote.name);
    if let Some(parent) = socket.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    if let Err(e) = tokio::fs::remove_file(&socket).await
        && e.kind() != std::io::ErrorKind::NotFound
    {
        warn!(path = %socket.display(), error = %e, "could not remove the stale rc socket");
    }

    let args = mount_args(remote, paths, mount_point, log_level);
    debug!(remote = %remote.name, args = ?args, "spawning rclone mount");

    let mut child = Command::new("rclone")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // The whole point: an abandoned child cannot outlive us and keep
        // writing to the same tree.
        .kill_on_drop(true)
        .spawn()
        .context("spawning rclone (is it on PATH?)")?;

    // rclone's JSON log arrives on stderr; stdout is drained so a full pipe can
    // never block the child.
    if let Some(stderr) = child.stderr.take() {
        let remote_name = remote.name.clone();
        tokio::spawn(ingest_log(stderr, remote_name));
    }
    if let Some(stdout) = child.stdout.take() {
        tokio::spawn(drain(stdout));
    }

    Ok(child)
}

/// Read and discard, so a child writing to stdout cannot fill the pipe buffer
/// and block forever.
async fn drain(stdout: tokio::process::ChildStdout) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        debug!(source = "rclone", "{line}");
    }
}

// ---------------------------------------------------------------------------
// Log ingestion
// ---------------------------------------------------------------------------

/// One line of rclone's `--use-json-log` output.
#[derive(Debug, Deserialize)]
struct RcloneLine {
    #[serde(default)]
    level: String,
    #[serde(default)]
    msg: String,
    /// Which remote or file the message is about.
    #[serde(default)]
    object: Option<String>,
    /// rclone's own source location, e.g. `vfs/vfs.go:257`. Useful in a bug
    /// report, noise otherwise, so it is kept at debug level.
    #[serde(default)]
    source: Option<String>,
}

/// Read rclone's stderr and re-emit each line into our own log.
async fn ingest_log(stderr: tokio::process::ChildStderr, remote: String) {
    let mut lines = BufReader::new(stderr).lines();

    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        match serde_json::from_str::<RcloneLine>(line) {
            Ok(entry) => emit(&entry, &remote),
            // Not JSON: rclone emits a few plain-text lines before its logger is
            // configured, and FUSE itself can write here. Pass them through
            // rather than dropping them.
            Err(_) => info!(source = "rclone", remote = %remote, "{line}"),
        }
    }
    debug!(remote = %remote, "rclone log stream ended");
}

fn emit(entry: &RcloneLine, remote: &str) {
    let code = classify(&entry.msg);
    let subject = format!("mount/{remote}");
    let msg = entry.msg.as_str();
    let object = entry.object.as_deref().unwrap_or("");
    let src = entry.source.as_deref().unwrap_or("");

    // rclone's levels, which are not quite ours: notice sits between info and
    // warning, and critical/emergency are both fatal.
    match entry.level.as_str() {
        "emergency" | "critical" | "error" => match code {
            Some(code) => error!(
                source = "rclone",
                remote = %remote,
                subject = %subject,
                code = code.as_str(),
                object = %object,
                rclone_source = %src,
                "{msg}"
            ),
            None => error!(
                source = "rclone",
                remote = %remote,
                subject = %subject,
                object = %object,
                rclone_source = %src,
                "{msg}"
            ),
        },
        "warning" => match code {
            Some(code) => warn!(
                source = "rclone",
                remote = %remote,
                subject = %subject,
                code = code.as_str(),
                object = %object,
                "{msg}"
            ),
            None => warn!(
                source = "rclone",
                remote = %remote,
                subject = %subject,
                object = %object,
                "{msg}"
            ),
        },
        // Levels are preserved rather than demoted. Demoting rclone's `info` to
        // our `debug` meant that setting `rclone_log_level = "info"` appeared to
        // do nothing unless you *also* set `log_level = "debug"` — two knobs for
        // one intent, which is the confusion this design is meant to remove.
        // Volume is already controlled by `rclone_log_level`, whose default
        // (`notice`) emits very little.
        "notice" | "info" => {
            info!(source = "rclone", remote = %remote, subject = %subject, "{msg}")
        }
        _ => debug!(source = "rclone", remote = %remote, subject = %subject, "{msg}"),
    }
}

/// Map an rclone message onto one of our stable codes.
///
/// This is what turns rclone's prose into something greppable with a fix hint
/// attached. The old daemon embedded whole rclone JSON blobs inside its own
/// `tracing` messages, which is why the logs were unreadable.
pub fn classify(msg: &str) -> Option<Code> {
    let m = msg.to_ascii_lowercase();

    // Order matters: auth failures often also mention the network.
    if m.contains("couldn't fetch token")
        || m.contains("failed to get token")
        || m.contains("invalid_grant")
        || m.contains("token expired")
        || m.contains("unauthenticated")
        || m.contains("401")
    {
        return Some(Code::AuthExpired);
    }
    if m.contains("no such host")
        || m.contains("network is unreachable")
        || m.contains("connection refused")
        || m.contains("dial tcp")
        || m.contains("i/o timeout")
    {
        return Some(Code::Offline);
    }
    if m.contains("429") || m.contains("too many requests") || m.contains("throttl") {
        return Some(Code::Throttled);
    }
    if m.contains("no space left") || m.contains("quota") || m.contains("disk full") {
        return Some(Code::DiskFull);
    }
    if m.contains("transport endpoint is not connected") || m.contains("endpoint not connected") {
        return Some(Code::MountStale);
    }
    None
}

// ---------------------------------------------------------------------------
// rc client
// ---------------------------------------------------------------------------

/// Call an rc method on a running mount.
///
/// Uses `rclone rc --unix-socket` rather than speaking HTTP ourselves: it keeps
/// the control channel off the network entirely and avoids pulling an HTTP
/// client into a daemon that otherwise needs none.
pub async fn rc(paths: &Paths, remote: &str, method: &str, params: &[&str]) -> Result<String> {
    let socket = rc_socket(paths, remote);
    if !socket.exists() {
        anyhow::bail!("no rc socket for {remote} at {}", socket.display());
    }

    let mut cmd = Command::new("rclone");
    cmd.arg("rc")
        .arg("--unix-socket")
        .arg(&socket)
        .arg("--config")
        .arg(paths.rclone_config())
        .arg(method)
        .args(params)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let output = tokio::time::timeout(RC_TIMEOUT, cmd.output())
        .await
        .with_context(|| format!("rc {method} timed out after {RC_TIMEOUT:?}"))?
        .with_context(|| format!("running rclone rc {method}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        anyhow::bail!("rc {method} failed: {stderr}");
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Re-read directory listings from the remote.
///
/// Called on resume and on reconnect: after a suspend the VFS's cached
/// directories may be arbitrarily stale, and waiting out `dir_cache_time` (72h
/// by default) is not an option.
pub async fn vfs_refresh(paths: &Paths, remote: &str, recursive: bool) -> Result<()> {
    // The root is addressed by *omitting* `dir` entirely. Verified against
    // rclone 1.75.1: both `dir=` and `dir=/` come back
    // `{"": "file does not exist"}`, which rclone reports inside a successful
    // response — so getting this wrong fails silently rather than erroring.
    let params: Vec<&str> = if recursive {
        vec!["recursive=true"]
    } else {
        Vec::new()
    };

    let body = rc(paths, remote, "vfs/refresh", &params).await?;

    // rc returns HTTP 200 with a per-directory result map, so the exit status
    // alone does not tell us whether the refresh actually happened.
    if body.contains("does not exist") {
        anyhow::bail!("vfs/refresh was rejected: {}", body.trim());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Unmounting
// ---------------------------------------------------------------------------

/// Force-unmount a path with `fusermount3`, falling back to `fusermount`.
///
/// Needed before remounting over a stale mount point: a FUSE mount whose server
/// died leaves the directory returning `ENOTCONN`, and mounting on top of it
/// fails.
pub async fn fusermount_unmount(path: &Path) -> bool {
    for binary in ["fusermount3", "fusermount"] {
        let result = Command::new(binary)
            .arg("-uz") // lazy, so a busy mount point still detaches
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output()
            .await;

        match result {
            Ok(out) if out.status.success() => {
                debug!(path = %path.display(), binary, "unmounted");
                return true;
            }
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                // "not mounted" is the normal case on a clean start.
                debug!(path = %path.display(), binary, %stderr, "unmount did nothing");
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => warn!(path = %path.display(), binary, error = %e, "unmount failed"),
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use onedrive_rc_core::config::Config;

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

    fn paths() -> Paths {
        Paths::rooted("/home/t")
    }

    /// Where the supervisor would mount this remote — the hidden backing path,
    /// since the overlay is enabled by default and odfs sits at the
    /// user-facing one instead.
    fn mount_point() -> PathBuf {
        crate::mount::rclone_mount_point(&remote(), &paths())
    }

    #[test]
    fn builds_the_documented_mount_command() {
        let args = mount_args(&remote(), &paths(), &mount_point(), RcloneLogLevel::Notice);

        assert_eq!(args[0], "mount");
        assert_eq!(args[1], "onedrive-timm:");
        // The overlay is on by default, so rclone goes to the hidden backing
        // path; odfs is what appears at the path the user actually browses.
        assert_eq!(args[2], "/home/t/.cache/onedrive-rc/onedrive-timm/mount");

        let joined = args.join(" ");
        for expected in [
            "--vfs-cache-mode=full",
            "--vfs-cache-max-age=30d",
            "--vfs-cache-max-size=25G",
            "--vfs-cache-min-free-space=10G",
            // Rendered in canonical form, so the config's `72h` becomes `3d`.
            // Verified against rclone 1.75.1: day units are accepted.
            "--dir-cache-time=3d",
            "--poll-interval=30s",
            "--transfers=4",
            "--checkers=4",
            "--buffer-size=8M",
            "--vfs-read-chunk-size=32M",
            "--use-json-log",
            "--log-level=NOTICE",
            "--timeout=30s",
            "--contimeout=10s",
            "--rc-no-auth",
        ] {
            assert!(
                joined.contains(expected),
                "missing {expected} in:\n{joined}"
            );
        }
    }

    /// `--log-file -` would create a file named `-` rather than writing to
    /// stdout, so the flag must never appear.
    #[test]
    fn never_passes_log_file() {
        let joined =
            mount_args(&remote(), &paths(), &mount_point(), RcloneLogLevel::Debug).join(" ");
        assert!(!joined.contains("--log-file"), "{joined}");
        assert!(joined.contains("--log-level=DEBUG"), "{joined}");
    }

    #[test]
    fn rc_listens_on_a_unix_socket_not_a_port() {
        let args = mount_args(&remote(), &paths(), &mount_point(), RcloneLogLevel::Notice);
        let joined = args.join(" ");
        assert!(
            joined.contains("--rc-addr=unix:///home/t/.run/onedrive-rc/rc-onedrive-timm.sock"),
            "{joined}"
        );
        assert!(!joined.contains("--rc-addr=:"), "no TCP port: {joined}");
        assert_eq!(
            rc_socket(&paths(), "onedrive-timm"),
            Path::new("/home/t/.run/onedrive-rc/rc-onedrive-timm.sock"),
            "the rc socket shares the length-guarded socket directory"
        );
    }

    #[test]
    fn extra_flags_are_appended_verbatim() {
        let mut r = remote();
        r.mount.extra_flags = vec!["--vfs-read-ahead=128M".into(), "--no-modtime".into()];
        let args = mount_args(&r, &paths(), &mount_point(), RcloneLogLevel::Notice);

        assert_eq!(args[args.len() - 2], "--vfs-read-ahead=128M");
        assert_eq!(args[args.len() - 1], "--no-modtime");
    }

    #[test]
    fn a_custom_cache_dir_is_honoured() {
        let mut r = remote();
        r.mount.cache_dir = Some("~/elsewhere".into());
        let joined = mount_args(&r, &paths(), &mount_point(), RcloneLogLevel::Notice).join(" ");
        assert!(joined.contains("--cache-dir=/home/t/elsewhere"), "{joined}");
    }

    /// The classifier is what gives an rclone error a code and therefore a fix
    /// hint. These strings are taken from the real failures in the old daemon's
    /// log on this machine.
    #[test]
    fn classifies_the_failures_seen_in_practice() {
        assert_eq!(
            classify(
                "Failed to create file system for \"onedrive-timm:Dokumente/Keepass\": \
                 failed to get root: couldn't fetch token: Post \
                 \"https://login.microsoftonline.com/common/oauth2/v2.0/token\": \
                 dial tcp: lookup login.microsoftonline.com: no such host"
            ),
            Some(Code::AuthExpired),
            "auth wins over network when both are mentioned"
        );
        assert_eq!(
            classify(
                "Get \"https://graph.microsoft.com/v1.0/drives/x/root\": dial tcp: lookup graph.microsoft.com: no such host"
            ),
            Some(Code::Offline)
        );
        assert_eq!(
            classify("HTTP 429 Too Many Requests"),
            Some(Code::Throttled)
        );
        assert_eq!(
            classify("write error: no space left on device"),
            Some(Code::DiskFull)
        );
        assert_eq!(
            classify("transport endpoint is not connected"),
            Some(Code::MountStale)
        );
        assert_eq!(classify("quota exceeded"), Some(Code::DiskFull));
        assert_eq!(classify("invalid_grant"), Some(Code::AuthExpired));
    }

    #[test]
    fn ordinary_chatter_is_not_classified_as_a_failure() {
        for msg in [
            "poll-interval is not supported by this remote",
            "vfs cache: cleaned: objects 0 (was 0) in use 0, to upload 0",
            "changeNotify: relativePath=\"Bilder/2025\", type=0",
        ] {
            assert_eq!(classify(msg), None, "{msg}");
        }
    }

    #[test]
    fn parses_a_real_rclone_json_line() {
        let line = r#"{"time":"2026-10-08T12:43:32.7911113+02:00","level":"info","msg":"poll-interval is not supported by this remote","object":"OneDrive root ''","objectType":"*onedrive-rc.Fs","source":"vfs/vfs.go:257"}"#;
        let entry: RcloneLine = serde_json::from_str(line).unwrap();
        assert_eq!(entry.level, "info");
        assert_eq!(entry.msg, "poll-interval is not supported by this remote");
        assert_eq!(entry.object.as_deref(), Some("OneDrive root ''"));
        assert_eq!(entry.source.as_deref(), Some("vfs/vfs.go:257"));
    }

    #[test]
    fn a_json_line_missing_fields_still_parses() {
        let entry: RcloneLine = serde_json::from_str(r#"{"msg":"bare"}"#).unwrap();
        assert_eq!(entry.msg, "bare");
        assert_eq!(entry.level, "");
        assert!(entry.object.is_none());
    }

    #[tokio::test]
    async fn rc_against_a_missing_socket_fails_without_hanging() {
        let err = rc(&paths(), "nope", "core/stats", &[]).await.unwrap_err();
        assert!(err.to_string().contains("no rc socket"), "{err:#}");
    }

    #[tokio::test]
    async fn unmounting_something_not_mounted_is_harmless() {
        let dir = tempfile::tempdir().unwrap();
        // Returns false (nothing to do) and must not panic or hang.
        assert!(!fusermount_unmount(dir.path()).await);
    }
}
