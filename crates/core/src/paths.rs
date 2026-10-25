//! Where everything lives on disk.
//!
//! Resolved once at startup into a [`Paths`] value that is threaded through the
//! daemon, rather than re-read from the environment at every call site. That
//! makes the layout testable without mutating process environment (which is
//! `unsafe` in edition 2024) and keeps the XDG rules in exactly one place.
//!
//! Note this moves state from `~/.local/share` (what the old `dirs::data_local_dir`
//! gave us) to `~/.local/state`, which is the correct XDG base for things that are
//! neither cache nor user-authored data.

use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

/// Application name used as the directory component under each XDG base.
pub const APP: &str = "onedrive-rc";

/// A unix socket path has to fit in `sockaddr_un.sun_path`, which is 108 bytes
/// including the terminating NUL on Linux. Staying well under that leaves room
/// for the `.sock` name without having to reason about exact byte counts.
const SOCKET_PATH_MAX: usize = 100;

/// Longest basename we create in the socket directory, so the directory can be
/// length-checked once rather than per socket. `rc-` + a remote name + `.sock`;
/// 48 covers any sane remote name.
const LONGEST_SOCKET_NAME: usize = 48;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    home: PathBuf,
    config: PathBuf,
    state: PathBuf,
    cache: PathBuf,
    runtime: PathBuf,
    /// Directory every unix socket goes in. Usually `runtime`, but falls back to
    /// a short `/tmp` path when `runtime` would overflow `sun_path`.
    socket_dir: PathBuf,
}

impl Paths {
    /// Resolve from the environment, following the XDG base directory spec.
    pub fn from_env() -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/"));

        let base = |var: &str, fallback: &str| -> PathBuf {
            match std::env::var_os(var) {
                Some(v) if !v.is_empty() => PathBuf::from(v),
                _ => home.join(fallback),
            }
        };

        let state = base("XDG_STATE_HOME", ".local/state");
        let runtime = match std::env::var_os("XDG_RUNTIME_DIR") {
            Some(v) if !v.is_empty() => PathBuf::from(v),
            // Fall back to the conventional location, deriving our own uid from
            // /proc rather than taking a libc dependency for one number.
            _ => match std::fs::metadata("/proc/self") {
                Ok(m) => PathBuf::from(format!("/run/user/{}", m.uid())),
                Err(_) => state.clone(),
            },
        };
        let runtime = runtime.join(APP);

        Self {
            config: base("XDG_CONFIG_HOME", ".config").join(APP),
            state: state.join(APP),
            cache: base("XDG_CACHE_HOME", ".cache").join(APP),
            socket_dir: choose_socket_dir(&runtime, uid()),
            runtime,
            home,
        }
    }

    /// Put every directory under one root. For tests.
    pub fn rooted(home: impl Into<PathBuf>) -> Self {
        let home = home.into();
        let runtime = home.join(".run").join(APP);
        Self {
            config: home.join(".config").join(APP),
            state: home.join(".local/state").join(APP),
            cache: home.join(".cache").join(APP),
            socket_dir: choose_socket_dir(&runtime, uid()),
            runtime,
            home,
        }
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    // -- config ------------------------------------------------------------

    pub fn config_dir(&self) -> &Path {
        &self.config
    }

    /// `~/.config/onedrive-rc/config.toml`
    pub fn config_file(&self) -> PathBuf {
        self.config.join("config.toml")
    }

    /// rclone's own config file, holding the remote definitions and OAuth
    /// tokens.
    ///
    /// Passed explicitly to every rclone child rather than left to the child's
    /// inherited `HOME`. Being explicit is what makes a test able to stand up an
    /// isolated remote, and it removes a latent production surprise: a daemon
    /// started with a different `HOME` than the user's would otherwise look for
    /// the tokens in the wrong place and report an auth failure.
    ///
    /// Honours `RCLONE_CONFIG`, which is rclone's own override.
    pub fn rclone_config(&self) -> PathBuf {
        if let Some(explicit) = std::env::var_os("RCLONE_CONFIG")
            && !explicit.is_empty()
        {
            return PathBuf::from(explicit);
        }
        // `self.config` is `<base>/onedrive-rc`; rclone lives beside us under the
        // same XDG config base.
        match self.config.parent() {
            Some(base) => base.join("rclone").join("rclone.conf"),
            None => self.home.join(".config/rclone/rclone.conf"),
        }
    }

    // -- state -------------------------------------------------------------

    pub fn state_dir(&self) -> &Path {
        &self.state
    }

    /// `~/.local/state/onedrive-rc/state.db`
    pub fn state_db(&self) -> PathBuf {
        self.state.join("state.db")
    }

    /// The `flock` target guaranteeing a single daemon instance.
    pub fn lock_file(&self) -> PathBuf {
        self.state.join("daemon.lock")
    }

    /// `~/.local/state/onedrive-rc/log`
    pub fn log_dir(&self) -> PathBuf {
        self.state.join("log")
    }

    /// Base name that `tracing-appender` suffixes with the rotation date.
    pub fn log_file_prefix(&self) -> &'static str {
        "onedrive-rcd.jsonl"
    }

    /// bisync's `--workdir`, holding the prior-run listings for one pin.
    pub fn bisync_workdir(&self, remote: &str, pin: &str) -> PathBuf {
        self.state.join("bisync").join(remote).join(pin)
    }

    /// Generated filter file handed to bisync via `--filters-file`.
    pub fn bisync_filters(&self, remote: &str, pin: &str) -> PathBuf {
        self.bisync_workdir(remote, pin).join("filters.txt")
    }

    // -- cache -------------------------------------------------------------

    pub fn cache_dir(&self) -> &Path {
        &self.cache
    }

    pub fn remote_cache_dir(&self, remote: &str) -> PathBuf {
        self.cache.join(remote)
    }

    /// The hidden `rclone mount` that odfs reads through. Not for direct use.
    pub fn backing_mount(&self, remote: &str) -> PathBuf {
        self.remote_cache_dir(remote).join("mount")
    }

    /// What we pass as rclone's `--cache-dir`; rclone creates `vfs/` and
    /// `vfsMeta/` beneath it, each mirroring remote paths 1:1.
    pub fn rclone_cache_dir(&self, remote: &str) -> PathBuf {
        self.remote_cache_dir(remote).join("rclone")
    }

    /// Cached file contents for `remote_path`, or `None` if the path escapes the cache.
    pub fn vfs_cached_file(&self, remote: &str, remote_path: &str) -> Option<PathBuf> {
        let base = self.rclone_cache_dir(remote).join("vfs").join(remote);
        join_guarded(&base, remote_path)
    }

    /// rclone's sidecar metadata for `remote_path`, recording which ranges are present.
    pub fn vfs_meta_file(&self, remote: &str, remote_path: &str) -> Option<PathBuf> {
        let base = self.rclone_cache_dir(remote).join("vfsMeta").join(remote);
        join_guarded(&base, remote_path)
    }

    // -- runtime -----------------------------------------------------------

    pub fn runtime_dir(&self) -> &Path {
        &self.runtime
    }

    /// Directory holding every unix socket we create.
    pub fn socket_dir(&self) -> &Path {
        &self.socket_dir
    }

    /// The IPC socket `onedrive-rc` connects to.
    ///
    /// Normally `$XDG_RUNTIME_DIR/onedrive-rc/onedrive-rcd.sock`. If that would exceed
    /// the kernel's `sun_path` limit it falls back to a short `/tmp` path (see
    /// [`choose_socket_dir`]); the daemon and the CLI derive this identically,
    /// so they always agree on where to meet.
    pub fn socket(&self) -> PathBuf {
        self.socket_dir.join("onedrive-rcd.sock")
    }

    /// rclone's remote-control socket for one remote.
    ///
    /// Shares the socket directory rather than living under the cache, for two
    /// reasons: a socket is runtime state, not cache; and the cache path is
    /// user-configurable and nested, which makes it the most likely thing to
    /// push the address past `sun_path`.
    pub fn rc_socket(&self, remote: &str) -> PathBuf {
        self.socket_dir.join(format!("rc-{remote}.sock"))
    }

    /// Whether the sockets had to fall back out of the runtime directory.
    pub fn socket_is_fallback(&self) -> bool {
        !self.socket_dir.starts_with(&self.runtime)
    }

    // -- helpers -----------------------------------------------------------

    /// Expand a leading `~` against this `Paths`' home. Ported from the old
    /// `paths::expand_tilde`, but without the global `dirs::home_dir()` lookup.
    pub fn expand(&self, path: &str) -> PathBuf {
        if let Some(rest) = path.strip_prefix("~/") {
            self.home.join(rest)
        } else if path == "~" {
            self.home.clone()
        } else {
            PathBuf::from(path)
        }
    }
}

/// Our own uid, read from `/proc` to avoid a libc dependency for one number.
fn uid() -> u32 {
    std::fs::metadata("/proc/self")
        .map(|m| m.uid())
        .unwrap_or(0)
}

/// Pick a socket directory the kernel will actually accept addresses in.
///
/// `sockaddr_un` caps the whole path at 108 bytes, and the failure mode is an
/// opaque `bind: invalid argument` (or `path must be shorter than SUN_LEN`) at
/// bind time — not something you want to debug from a mount that silently will
/// not start. A long `XDG_RUNTIME_DIR` is unusual but entirely possible
/// (containers, sandboxes, test harnesses), so fall back to a short uid-keyed
/// path under `/tmp` that every participant computes the same way.
///
/// The budget leaves room for the longest name we put in here, which is
/// `rc-<remote>.sock`.
///
/// The fallback is keyed by uid *and* by a digest of the runtime directory it
/// stands in for. Keying on uid alone would make every long runtime path on a
/// machine collapse onto one directory — so two daemons with different roots
/// would fight over the same socket, and a test would happily connect to the
/// real daemon. Both ends still derive it from the same runtime path, so they
/// continue to agree.
fn choose_socket_dir(runtime: &Path, uid: u32) -> PathBuf {
    if runtime.as_os_str().len() + LONGEST_SOCKET_NAME <= SOCKET_PATH_MAX {
        return runtime.to_path_buf();
    }
    PathBuf::from(format!("/tmp/onedrive-rc-{uid}-{:08x}", digest(runtime)))
}

/// FNV-1a. Not a security boundary — it only has to separate distinct paths.
fn digest(path: &Path) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in path.as_os_str().as_encoded_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// Join an untrusted relative path onto `base`, refusing anything that would
/// escape it (`..`, absolute paths, root). Used for cache lookups driven by
/// remote-supplied path strings.
/// Join `rel` onto `base`, refusing anything that could escape it — `..`, a
/// leading `/`, a Windows drive prefix. `None` means refuse, not "best effort".
pub fn join_guarded(base: &Path, rel: &str) -> Option<PathBuf> {
    let rel = Path::new(rel.trim_start_matches('/'));
    let mut out = base.to_path_buf();
    for component in rel.components() {
        match component {
            Component::Normal(c) => out.push(c),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(out)
}

/// Whether `child` is `parent` or lives beneath it, lexically.
///
/// Used by config validation to reject a pin whose `local_path` sits inside a
/// mount point — nothing stopped that before, and it makes "local" and "remote"
/// the same storage seen through a VFS, which is unsyncable by construction.
pub fn is_inside(parent: &Path, child: &Path) -> bool {
    let parent = normalise(parent);
    let child = normalise(child);
    child.starts_with(&parent)
}

/// Lexical cleanup: drop `.` components and collapse `a/b/..` to `a`.
/// Deliberately does not touch the filesystem, so it works for paths that do
/// not exist yet.
fn normalise(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in p.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> Paths {
        Paths::rooted("/home/t")
    }

    #[test]
    fn layout_matches_the_documented_paths() {
        let p = paths();
        assert_eq!(
            p.config_file(),
            Path::new("/home/t/.config/onedrive-rc/config.toml")
        );
        assert_eq!(
            p.state_db(),
            Path::new("/home/t/.local/state/onedrive-rc/state.db")
        );
        assert_eq!(
            p.lock_file(),
            Path::new("/home/t/.local/state/onedrive-rc/daemon.lock")
        );
        assert_eq!(p.log_dir(), Path::new("/home/t/.local/state/onedrive-rc/log"));
        assert_eq!(
            p.socket(),
            Path::new("/home/t/.run/onedrive-rc/onedrive-rcd.sock")
        );
        assert!(!p.socket_is_fallback());
        assert_eq!(
            p.backing_mount("timm"),
            Path::new("/home/t/.cache/onedrive-rc/timm/mount")
        );
        assert_eq!(
            p.bisync_workdir("timm", "Keepass"),
            Path::new("/home/t/.local/state/onedrive-rc/bisync/timm/Keepass")
        );
    }

    #[test]
    fn expands_tilde_only_at_the_start() {
        let p = paths();
        assert_eq!(
            p.expand("~/onedrive-keepass"),
            Path::new("/home/t/onedrive-keepass")
        );
        assert_eq!(p.expand("~"), Path::new("/home/t"));
        assert_eq!(p.expand("/etc/passwd"), Path::new("/etc/passwd"));
        assert_eq!(p.expand("relative/path"), Path::new("relative/path"));
        // A `~` in the middle is a literal, not an expansion.
        assert_eq!(p.expand("/tmp/~/x"), Path::new("/tmp/~/x"));
    }

    #[test]
    fn vfs_cache_paths_mirror_remote_paths() {
        let p = paths();
        assert_eq!(
            p.vfs_cached_file("timm", "Dokumente/Keepass/Timm.kdbx")
                .unwrap(),
            Path::new("/home/t/.cache/onedrive-rc/timm/rclone/vfs/timm/Dokumente/Keepass/Timm.kdbx")
        );
        assert_eq!(
            p.vfs_meta_file("timm", "Dokumente/Keepass/Timm.kdbx")
                .unwrap(),
            Path::new(
                "/home/t/.cache/onedrive-rc/timm/rclone/vfsMeta/timm/Dokumente/Keepass/Timm.kdbx"
            )
        );
    }

    #[test]
    fn cache_lookups_refuse_to_escape_the_cache_dir() {
        let p = paths();
        assert!(p.vfs_cached_file("timm", "../../../etc/passwd").is_none());
        assert!(p.vfs_cached_file("timm", "a/../../b").is_none());
        assert!(p.vfs_cached_file("timm", "/etc/passwd").is_some()); // leading / is stripped
        assert_eq!(
            p.vfs_cached_file("timm", "/etc/passwd").unwrap(),
            Path::new("/home/t/.cache/onedrive-rc/timm/rclone/vfs/timm/etc/passwd")
        );
        assert!(p.vfs_cached_file("timm", "./a/./b").is_some());
    }

    #[test]
    fn detects_containment_for_paths_that_do_not_exist() {
        assert!(is_inside(
            Path::new("/home/t/OneDrive"),
            Path::new("/home/t/OneDrive/Docs/a")
        ));
        assert!(is_inside(
            Path::new("/home/t/OneDrive"),
            Path::new("/home/t/OneDrive")
        ));
        assert!(!is_inside(
            Path::new("/home/t/OneDrive"),
            Path::new("/home/t/other")
        ));
        // Must not be fooled by a shared name prefix.
        assert!(!is_inside(
            Path::new("/home/t/OneDrive"),
            Path::new("/home/t/OneDrive2/x")
        ));
        // Must not be fooled by traversal.
        assert!(!is_inside(
            Path::new("/home/t/OneDrive"),
            Path::new("/home/t/OneDrive/../elsewhere")
        ));
        assert!(is_inside(
            Path::new("/home/t/OneDrive"),
            Path::new("/home/t/./OneDrive/x")
        ));
    }

    #[test]
    fn from_env_is_consistent_with_itself() {
        // Cannot safely mutate env in a test, so just assert internal consistency.
        let p = Paths::from_env();
        assert!(p.config_file().starts_with(p.config_dir()));
        assert!(p.state_db().starts_with(p.state_dir()));
        assert!(p.config_dir().ends_with(APP));
        // Either in the runtime dir, or the documented short fallback.
        assert_eq!(
            p.socket().starts_with(p.runtime_dir()),
            !p.socket_is_fallback()
        );
    }

    /// `sockaddr_un` caps the whole address at 108 bytes, and exceeding it fails
    /// at bind with an opaque message — observed in practice as rclone refusing
    /// to start with `bind: invalid argument`. A long runtime dir must therefore
    /// pick a short directory instead, and every participant has to land on the
    /// same one, which is why this is computed rather than configured.
    #[test]
    fn an_overlong_runtime_dir_falls_back_to_a_short_socket_dir() {
        let long = Path::new("/tmp").join("x".repeat(120)).join(APP);
        let dir = choose_socket_dir(&long, 1811);

        let text = dir.to_string_lossy();
        assert!(text.starts_with("/tmp/onedrive-rc-1811-"), "{text}");
        assert!(
            dir.as_os_str().len() + LONGEST_SOCKET_NAME <= SOCKET_PATH_MAX,
            "{text}"
        );
        // Deterministic, so both ends derive the same path.
        assert_eq!(dir, choose_socket_dir(&long, 1811));
    }

    /// Keying the fallback on uid alone would collapse every long runtime path
    /// onto one directory, so two daemons with different roots — or a test and
    /// the real daemon — would end up sharing a socket.
    #[test]
    fn distinct_long_runtime_dirs_get_distinct_socket_dirs() {
        let a = Path::new("/tmp").join("a".repeat(120)).join(APP);
        let b = Path::new("/tmp").join("b".repeat(120)).join(APP);
        assert_ne!(choose_socket_dir(&a, 1811), choose_socket_dir(&b, 1811));

        let one = Paths::rooted(Path::new("/tmp").join("one".repeat(50)));
        let two = Paths::rooted(Path::new("/tmp").join("two".repeat(50)));
        assert!(one.socket_is_fallback() && two.socket_is_fallback());
        assert_ne!(one.socket(), two.socket());
        assert_ne!(one.rc_socket("od"), two.rc_socket("od"));
    }

    #[test]
    fn a_normal_runtime_dir_is_used_as_is() {
        let dir = choose_socket_dir(Path::new("/run/user/1811/onedrive-rc"), 1811);
        assert_eq!(dir, Path::new("/run/user/1811/onedrive-rc"));
    }

    /// Every socket we create has to fit, including the longest name. This is
    /// the regression guard for the rc socket, which originally lived under the
    /// (deep, user-configurable) cache directory and silently broke mounting.
    #[test]
    fn every_socket_fits_in_sun_path() {
        for home in ["/home/t", &format!("/tmp/{}", "y".repeat(140))] {
            let p = Paths::rooted(home);
            for socket in [p.socket(), p.rc_socket("onedrive-timm")] {
                assert!(
                    socket.as_os_str().len() <= SOCKET_PATH_MAX,
                    "{} is {} bytes, over the {SOCKET_PATH_MAX} budget",
                    socket.display(),
                    socket.as_os_str().len()
                );
            }
        }
    }

    #[test]
    fn the_fallback_is_detected_and_is_uid_keyed() {
        let deep = Paths::rooted(Path::new("/tmp").join("y".repeat(140)));
        assert!(deep.socket_is_fallback());
        // `Path::starts_with` is component-wise, so compare as text here.
        assert!(
            deep.socket()
                .to_string_lossy()
                .starts_with("/tmp/onedrive-rc-"),
            "{}",
            deep.socket().display()
        );
        assert!(
            deep.rc_socket("od")
                .to_string_lossy()
                .starts_with("/tmp/onedrive-rc-"),
            "rc sockets must share the fallback: {}",
            deep.rc_socket("od").display()
        );

        // Two users never collide on the fallback path.
        assert_ne!(
            choose_socket_dir(Path::new(&"z".repeat(120)), 1000),
            choose_socket_dir(Path::new(&"z".repeat(120)), 1001)
        );
    }

    #[test]
    fn rc_sockets_are_per_remote() {
        let p = Paths::rooted("/home/t");
        assert_eq!(
            p.rc_socket("onedrive-timm"),
            Path::new("/home/t/.run/onedrive-rc/rc-onedrive-timm.sock")
        );
        assert_ne!(p.rc_socket("a"), p.rc_socket("b"));
        assert_ne!(p.rc_socket("a"), p.socket());
    }
}
