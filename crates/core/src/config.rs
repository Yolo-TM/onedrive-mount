//! The config file, its defaults, and its validation.
//!
//! Two things are different from the config this replaces, and both are
//! deliberate:
//!
//! 1. `#[serde(deny_unknown_fields)]` everywhere. The old loader accepted
//!    unknown keys silently, so a typo'd setting was ignored with no warning.
//! 2. [`Config::validate`] is exhaustive and is run at **startup** as well as on
//!    reload. The old `validate()` checked five fields and was never called on
//!    the startup path at all, so an invalid `vfs_cache_mode` reached rclone and
//!    killed the mount immediately.
//!
//! Validation also collects *every* problem rather than stopping at the first,
//! because fixing a config one error per daemon-restart is miserable.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::code::{Code, CodedError};
use crate::paths::{self, Paths};
use crate::units::{Dur, Size};

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

/// Verbosity of *our* log. Deliberately separate from [`RcloneLogLevel`]: the old
/// config drove both from one key, so debugging a sync rule also turned on
/// rclone's full VFS tracing and produced a 1.4 GB log file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Error,
    Warn,
    #[default]
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }
}

/// Verbosity passed to rclone children via `--log-level`. These are rclone's own
/// levels, which is why `notice` exists here and not in [`LogLevel`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RcloneLogLevel {
    Error,
    #[default]
    Notice,
    Info,
    Debug,
}

impl RcloneLogLevel {
    /// rclone wants these upper-case.
    pub const fn as_flag(self) -> &'static str {
        match self {
            Self::Error => "ERROR",
            Self::Notice => "NOTICE",
            Self::Info => "INFO",
            Self::Debug => "DEBUG",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VfsCacheMode {
    Off,
    Minimal,
    Writes,
    #[default]
    Full,
}

impl VfsCacheMode {
    pub const fn as_flag(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Minimal => "minimal",
            Self::Writes => "writes",
            Self::Full => "full",
        }
    }
}

/// What odfs does with a write that arrives while we are offline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OfflineWrites {
    /// Stage it and replay on reconnect, but only after re-verifying that the
    /// remote still matches the version the edit started from.
    #[default]
    Queue,
    /// Fail the write immediately with a clear error.
    Reject,
    /// Refuse writes entirely while offline.
    Readonly,
}

/// How a pin resolves the case where both sides changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConflictPolicy {
    /// Newer content keeps the canonical name; the other version is kept beside
    /// it with a timestamped suffix. Nothing is ever discarded.
    #[default]
    KeepBoth,
    /// Newer content wins and the loser is only recoverable from `.versions/`.
    NewerWins,
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub daemon: DaemonConfig,
    #[serde(default, rename = "remote")]
    pub remotes: Vec<RemoteConfig>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonConfig {
    #[serde(default)]
    pub log_level: LogLevel,
    #[serde(default)]
    pub rclone_log_level: RcloneLogLevel,
    #[serde(default = "d::log_retention_days")]
    pub log_retention_days: u32,
    #[serde(default = "d::log_retention_size")]
    pub log_retention_size: Size,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            log_level: LogLevel::default(),
            rclone_log_level: RcloneLogLevel::default(),
            log_retention_days: d::log_retention_days(),
            log_retention_size: d::log_retention_size(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteConfig {
    /// The rclone remote name, without the trailing colon.
    pub name: String,
    /// Where odfs is mounted, i.e. the path the user actually browses.
    pub mount_point: String,
    #[serde(default = "d::yes")]
    pub enabled: bool,
    #[serde(default)]
    pub mount: MountConfig,
    #[serde(default)]
    pub overlay: OverlayConfig,
    #[serde(default)]
    pub index: IndexConfig,
    #[serde(default)]
    pub offline: OfflineConfig,
    #[serde(default, rename = "pin")]
    pub pins: Vec<PinConfig>,
}

impl RemoteConfig {
    pub fn mount_point(&self, p: &Paths) -> PathBuf {
        p.expand(&self.mount_point)
    }

    /// rclone's `--cache-dir`; defaults to our own cache tree for this remote.
    pub fn rclone_cache_dir(&self, p: &Paths) -> PathBuf {
        match &self.mount.cache_dir {
            Some(dir) => p.expand(dir),
            None => p.rclone_cache_dir(&self.name),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MountConfig {
    /// Overrides rclone's `--cache-dir`. Leave unset to use the default layout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_dir: Option<String>,
    #[serde(default)]
    pub vfs_cache_mode: VfsCacheMode,
    /// Raised from the old 72h: this is what decides whether a file you read last
    /// week is still readable when you are offline today.
    #[serde(default = "d::vfs_cache_max_age")]
    pub vfs_cache_max_age: Dur,
    #[serde(default = "d::vfs_cache_max_size")]
    pub vfs_cache_max_size: Size,
    #[serde(default = "d::vfs_cache_min_free_space")]
    pub vfs_cache_min_free_space: Size,
    #[serde(default = "d::vfs_write_back")]
    pub vfs_write_back: Dur,
    /// Safe to make long because `poll_interval` keeps it fresh (OneDrive supports
    /// change notification) and our index is the offline fallback.
    #[serde(default = "d::dir_cache_time")]
    pub dir_cache_time: Dur,
    #[serde(default = "d::poll_interval")]
    pub poll_interval: Dur,
    #[serde(default = "d::transfers")]
    pub transfers: u32,
    #[serde(default = "d::checkers")]
    pub checkers: u32,
    #[serde(default = "d::buffer_size")]
    pub buffer_size: Size,
    #[serde(default = "d::vfs_read_chunk_size")]
    pub vfs_read_chunk_size: Size,
    /// Escape hatch for rclone flags we do not model. Each entry is one argv
    /// element, so `--vfs-read-ahead=128M` works and `--vfs-read-ahead 128M`
    /// does not — validation rejects the latter rather than letting rclone fail.
    #[serde(default)]
    pub extra_flags: Vec<String>,
}

impl Default for MountConfig {
    fn default() -> Self {
        Self {
            cache_dir: None,
            vfs_cache_mode: VfsCacheMode::default(),
            vfs_cache_max_age: d::vfs_cache_max_age(),
            vfs_cache_max_size: d::vfs_cache_max_size(),
            vfs_cache_min_free_space: d::vfs_cache_min_free_space(),
            vfs_write_back: d::vfs_write_back(),
            dir_cache_time: d::dir_cache_time(),
            poll_interval: d::poll_interval(),
            transfers: d::transfers(),
            checkers: d::checkers(),
            buffer_size: d::buffer_size(),
            vfs_read_chunk_size: d::vfs_read_chunk_size(),
            extra_flags: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OverlayConfig {
    /// `false` exposes the rclone mount directly at `mount_point` and gives up
    /// offline directory listings. The documented fallback if the double-FUSE
    /// read path turns out to cost too much latency.
    #[serde(default = "d::yes")]
    pub enabled: bool,
}

impl Default for OverlayConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexConfig {
    #[serde(default = "d::full_crawl_interval")]
    pub full_crawl_interval: Dur,
    /// Update the index from directory listings the user's own browsing already
    /// triggers. Free, and it keeps hot paths fresh for offline use.
    #[serde(default = "d::yes")]
    pub opportunistic: bool,
    /// rclone `--tpslimit` for crawls, to stay under OneDrive's throttling.
    #[serde(default = "d::tpslimit")]
    pub tpslimit: u32,
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            full_crawl_interval: d::full_crawl_interval(),
            opportunistic: true,
            tpslimit: d::tpslimit(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OfflineConfig {
    #[serde(default)]
    pub writes: OfflineWrites,
    /// Visible on purpose, so pending uploads can be inspected in a file manager.
    #[serde(default = "d::outbox_dir")]
    pub outbox_dir: String,
}

impl Default for OfflineConfig {
    fn default() -> Self {
        Self {
            writes: OfflineWrites::default(),
            outbox_dir: d::outbox_dir(),
        }
    }
}

impl OfflineConfig {
    pub fn outbox_dir(&self, p: &Paths) -> PathBuf {
        p.expand(&self.outbox_dir)
    }
}

/// A folder kept fully available offline, synced bidirectionally.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinConfig {
    pub name: String,
    /// Path on the remote, relative to the remote root. No leading slash.
    pub remote_path: String,
    /// A real local directory. Must not live inside any mount point.
    pub local_path: String,
    #[serde(default = "d::pin_interval")]
    pub interval: Dur,
    /// Also sync on local change (debounced). This is what gets a KeePass save
    /// to OneDrive in seconds instead of up to a full interval later, which is
    /// itself the best conflict reducer available.
    #[serde(default = "d::yes")]
    pub sync_on_change: bool,
    #[serde(default)]
    pub conflict: ConflictPolicy,
    /// Let conflict copies propagate to the remote, so a conflict can be merged
    /// from any machine.
    #[serde(default = "d::yes")]
    pub upload_conflict_copies: bool,
    /// Abort the run if more than this **percentage** of files would be deleted.
    ///
    /// rclone's `--max-delete` is a percentage despite the name — verified: the
    /// abort reads `too many deletes (>10%, 20 of 21)`. Named explicitly here
    /// because reading it as a file count is the natural mistake, and a tight
    /// value silently stops legitimate deletes from ever propagating: on a
    /// one-file pin any delete is 100%.
    #[serde(default = "d::max_delete_percent")]
    pub max_delete_percent: u32,
    /// Pre-run snapshots into `<local_path>/.versions/`, never synced. The only
    /// recovery path for a propagated delete, which produces no conflict copy.
    #[serde(default = "d::yes")]
    pub versions: bool,
    #[serde(default = "d::versions_retention")]
    pub versions_retention: Dur,
    /// Files larger than this are not snapshotted.
    #[serde(default = "d::versions_max_size")]
    pub versions_max_size: Size,
    /// Include filters. Empty means "everything", which is the common case.
    #[serde(default)]
    pub patterns: Vec<String>,
    /// Defaults to `true`, unlike the sync rules this replaces: a fully
    /// configured rule that silently does nothing is a bad default.
    #[serde(default = "d::yes")]
    pub enabled: bool,
}

impl PinConfig {
    pub fn local_path(&self, p: &Paths) -> PathBuf {
        p.expand(&self.local_path)
    }

    /// `<local_path>/.versions` — inside the pin, excluded from sync by the
    /// generated filter file.
    pub fn versions_dir(&self, p: &Paths) -> PathBuf {
        self.local_path(p).join(".versions")
    }

    /// `<remote>:<remote_path>` as rclone wants it.
    pub fn remote_spec(&self, remote: &str) -> String {
        if self.remote_path.is_empty() {
            format!("{remote}:")
        } else {
            format!("{remote}:{}", self.remote_path)
        }
    }
}

/// Default values, in one place so they are easy to audit against the docs.
mod d {
    use super::{Dur, Size};

    pub fn yes() -> bool {
        true
    }
    pub fn log_retention_days() -> u32 {
        7
    }
    pub fn log_retention_size() -> Size {
        Size::from_bytes(200 << 20)
    }
    pub fn vfs_cache_max_age() -> Dur {
        Dur::from_secs(30 * 86_400)
    }
    pub fn vfs_cache_max_size() -> Size {
        Size::from_bytes(25 << 30)
    }
    pub fn vfs_cache_min_free_space() -> Size {
        Size::from_bytes(10 << 30)
    }
    pub fn vfs_write_back() -> Dur {
        Dur::from_secs(5)
    }
    pub fn dir_cache_time() -> Dur {
        Dur::from_secs(72 * 3600)
    }
    pub fn poll_interval() -> Dur {
        Dur::from_secs(30)
    }
    pub fn transfers() -> u32 {
        4
    }
    pub fn checkers() -> u32 {
        4
    }
    pub fn buffer_size() -> Size {
        Size::from_bytes(8 << 20)
    }
    pub fn vfs_read_chunk_size() -> Size {
        Size::from_bytes(32 << 20)
    }
    pub fn full_crawl_interval() -> Dur {
        Dur::from_secs(24 * 3600)
    }
    pub fn tpslimit() -> u32 {
        10
    }
    pub fn outbox_dir() -> String {
        "~/OneDrive.pending".into()
    }
    pub fn pin_interval() -> Dur {
        Dur::from_secs(300)
    }
    /// rclone's own default. Protective enough in combination with
    /// `--check-access` and the pre-run snapshots, without aborting on every
    /// legitimate delete in a small pin.
    pub fn max_delete_percent() -> u32 {
        50
    }
    pub fn versions_retention() -> Dur {
        Dur::from_secs(14 * 86_400)
    }
    pub fn versions_max_size() -> Size {
        Size::from_bytes(50 << 20)
    }
}

// ---------------------------------------------------------------------------
// Load / save
// ---------------------------------------------------------------------------

impl Config {
    /// Read and parse, without validating. Callers should go through
    /// [`Config::load_validated`] unless they specifically want the raw value.
    pub fn load(path: &Path) -> Result<Self, CodedError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| CodedError::config(format!("cannot read {}: {e}", path.display())))?;
        toml::from_str(&text)
            .map_err(|e| CodedError::config(format!("{} is invalid: {e}", path.display())))
    }

    /// Read, parse and validate. This is what both startup and reload use.
    pub fn load_validated(path: &Path, p: &Paths) -> Result<Self, CodedError> {
        let config = Self::load(path)?;
        config.validate(p)?;
        Ok(config)
    }

    /// Atomic write: temp file then rename, so a crash mid-write cannot leave a
    /// truncated config behind. Same pattern as the config this replaces.
    pub fn save(&self, path: &Path) -> Result<(), CodedError> {
        let io =
            |e: std::io::Error| CodedError::config(format!("cannot write {}: {e}", path.display()));

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(io)?;
        }
        let text = toml::to_string_pretty(self)
            .map_err(|e| CodedError::config(format!("cannot serialise config: {e}")))?;

        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text).map_err(io)?;
        std::fs::rename(&tmp, path).map_err(io)?;
        Ok(())
    }

    pub fn enabled_remotes(&self) -> impl Iterator<Item = &RemoteConfig> {
        self.remotes.iter().filter(|r| r.enabled)
    }

    pub fn remote(&self, name: &str) -> Option<&RemoteConfig> {
        self.remotes.iter().find(|r| r.name == name)
    }
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Every problem found in one pass, so a broken config can be fixed in one go.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Problems(pub Vec<String>);

impl Problems {
    fn push(&mut self, msg: impl Into<String>) {
        self.0.push(msg.into());
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Display for Problems {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let n = self.0.len();
        writeln!(
            f,
            "{n} problem{} in the config:",
            if n == 1 { "" } else { "s" }
        )?;
        for (i, problem) in self.0.iter().enumerate() {
            writeln!(f, "  {}. {problem}", i + 1)?;
        }
        Ok(())
    }
}

impl Config {
    /// Check everything that can be checked without touching the network.
    pub fn validate(&self, p: &Paths) -> Result<(), CodedError> {
        let mut problems = Problems::default();

        if self.daemon.log_retention_days == 0 {
            problems.push("daemon.log_retention_days must be at least 1");
        }
        if self.daemon.log_retention_size.as_bytes() < (1 << 20) {
            problems.push("daemon.log_retention_size must be at least 1M");
        }

        // Collect as we go so cross-cutting checks can run afterwards.
        let mut mount_points: Vec<(String, PathBuf)> = Vec::new();
        let mut local_dirs: Vec<(String, PathBuf)> = Vec::new();
        let mut remote_names: HashMap<&str, usize> = HashMap::new();
        let mut pin_names: HashMap<&str, usize> = HashMap::new();

        for remote in &self.remotes {
            let who = format!("remote {:?}", remote.name);

            if remote.name.trim().is_empty() {
                problems.push("a remote has an empty name");
            } else if !remote
                .name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            {
                problems.push(format!(
                    "{who}: name may only contain letters, digits, '-', '_' and '.'"
                ));
            }
            *remote_names.entry(remote.name.as_str()).or_default() += 1;

            if remote.mount_point.trim().is_empty() {
                problems.push(format!("{who}: mount_point is empty"));
            } else {
                let mp = remote.mount_point(p);
                if !mp.is_absolute() {
                    problems.push(format!(
                        "{who}: mount_point {:?} must be an absolute path (or start with ~/)",
                        remote.mount_point
                    ));
                } else {
                    mount_points.push((remote.name.clone(), mp));
                }
            }

            self.validate_mount(&who, remote, &mut problems);

            if remote.index.full_crawl_interval.is_zero() {
                problems.push(format!("{who}: index.full_crawl_interval must not be zero"));
            }
            if remote.index.tpslimit == 0 || remote.index.tpslimit > 100 {
                problems.push(format!("{who}: index.tpslimit must be between 1 and 100"));
            }

            if remote.offline.outbox_dir.trim().is_empty() {
                problems.push(format!("{who}: offline.outbox_dir is empty"));
            } else {
                let dir = remote.offline.outbox_dir(p);
                if !dir.is_absolute() {
                    problems.push(format!(
                        "{who}: offline.outbox_dir {:?} must be absolute (or start with ~/)",
                        remote.offline.outbox_dir
                    ));
                } else {
                    local_dirs.push((format!("{who} offline.outbox_dir"), dir));
                }
            }

            for pin in &remote.pins {
                let who = format!("{who} pin {:?}", pin.name);

                if pin.name.trim().is_empty() {
                    problems.push(format!("{}: a pin has an empty name", remote.name));
                }
                *pin_names.entry(pin.name.as_str()).or_default() += 1;

                if pin.remote_path.starts_with('/') {
                    problems.push(format!(
                        "{who}: remote_path {:?} must be relative to the remote root (drop the leading '/')",
                        pin.remote_path
                    ));
                }
                if pin.remote_path.split('/').any(|c| c == "..") {
                    problems.push(format!("{who}: remote_path must not contain '..'"));
                }

                if pin.local_path.trim().is_empty() {
                    problems.push(format!("{who}: local_path is empty"));
                } else {
                    let dir = pin.local_path(p);
                    if !dir.is_absolute() {
                        problems.push(format!(
                            "{who}: local_path {:?} must be absolute (or start with ~/)",
                            pin.local_path
                        ));
                    } else {
                        local_dirs.push((who.clone(), dir));
                    }
                }

                if pin.interval.is_zero() {
                    problems.push(format!("{who}: interval must not be zero"));
                }
                if pin.max_delete_percent == 0 || pin.max_delete_percent > 100 {
                    problems.push(format!(
                        "{who}: max_delete_percent must be between 1 and 100 (it is a percentage, not a file count)"
                    ));
                }
                if pin.versions && pin.versions_retention.is_zero() {
                    problems.push(format!(
                        "{who}: versions_retention must not be zero while versions = true"
                    ));
                }
                for pattern in &pin.patterns {
                    if pattern.trim().is_empty() {
                        problems.push(format!("{who}: patterns contains an empty entry"));
                    }
                }
            }
        }

        for (name, count) in remote_names {
            if count > 1 {
                problems.push(format!("remote name {name:?} is used {count} times"));
            }
        }
        // Pin names must be globally unique, not just unique per remote, because
        // the CLI and notifications address a pin by bare name.
        for (name, count) in pin_names {
            if count > 1 {
                problems.push(format!("pin name {name:?} is used {count} times"));
            }
        }

        check_no_overlap(&mount_points, &mut problems);

        // A local directory inside a mount point means "local" and "remote" are
        // the same storage seen through a VFS. Nothing stopped this before.
        for (who, dir) in &local_dirs {
            for (remote_name, mp) in &mount_points {
                if paths::is_inside(mp, dir) {
                    problems.push(format!(
                        "{who} ({}) is inside the mount point of remote {remote_name:?} ({}) — it must be a separate directory",
                        dir.display(),
                        mp.display()
                    ));
                }
            }
        }
        check_no_overlap(&local_dirs, &mut problems);

        if problems.is_empty() {
            Ok(())
        } else {
            Err(CodedError::new(
                Code::Config,
                "config",
                problems.to_string(),
            ))
        }
    }

    fn validate_mount(&self, who: &str, remote: &RemoteConfig, problems: &mut Problems) {
        let m = &remote.mount;

        if m.transfers == 0 || m.transfers > 64 {
            problems.push(format!("{who}: mount.transfers must be between 1 and 64"));
        }
        if m.checkers == 0 || m.checkers > 64 {
            problems.push(format!("{who}: mount.checkers must be between 1 and 64"));
        }
        if m.poll_interval.is_zero() {
            problems.push(format!(
                "{who}: mount.poll_interval must not be zero (it is what keeps dir_cache_time fresh)"
            ));
        }
        if m.dir_cache_time.is_zero() {
            problems.push(format!("{who}: mount.dir_cache_time must not be zero"));
        }
        // rclone enforces this itself and exits if violated, so catch it here
        // where we can say something useful about it.
        if !m.poll_interval.is_zero() && m.poll_interval >= m.dir_cache_time {
            problems.push(format!(
                "{who}: mount.poll_interval ({}) must be smaller than mount.dir_cache_time ({})",
                m.poll_interval, m.dir_cache_time
            ));
        }
        if m.buffer_size.as_bytes() == 0 {
            problems.push(format!("{who}: mount.buffer_size must not be zero"));
        }

        // Offline reads come from the VFS cache, which only exists in these modes.
        if remote.overlay.enabled
            && matches!(m.vfs_cache_mode, VfsCacheMode::Off | VfsCacheMode::Minimal)
        {
            problems.push(format!(
                "{who}: mount.vfs_cache_mode = {:?} cannot serve offline reads; use \"writes\" or \"full\"",
                m.vfs_cache_mode.as_flag()
            ));
        }

        for flag in &m.extra_flags {
            if !flag.starts_with('-') {
                problems.push(format!(
                    "{who}: mount.extra_flags entry {flag:?} does not look like a flag"
                ));
            } else if flag.contains(char::is_whitespace) {
                // Each entry is one argv element, so the space form is silently wrong.
                problems.push(format!(
                    "{who}: mount.extra_flags entry {flag:?} contains a space — write it as --flag=value"
                ));
            }
        }
    }
}

/// Flag any pair in `dirs` where one path contains the other.
fn check_no_overlap(dirs: &[(String, PathBuf)], problems: &mut Problems) {
    for (i, (who_a, a)) in dirs.iter().enumerate() {
        for (who_b, b) in &dirs[i + 1..] {
            if paths::is_inside(a, b) || paths::is_inside(b, a) {
                problems.push(format!(
                    "{who_a} ({}) and {who_b} ({}) overlap",
                    a.display(),
                    b.display()
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> Paths {
        Paths::rooted("/home/t")
    }

    /// The config from the overhaul doc, which must parse and validate cleanly.
    const DOCUMENTED: &str = r#"
[daemon]
log_level          = "info"
rclone_log_level   = "notice"
log_retention_days = 7
log_retention_size = "200M"

[[remote]]
name        = "onedrive-timm"
mount_point = "~/OneDrive"
enabled     = true

[remote.mount]
vfs_cache_max_age        = "30d"
vfs_cache_max_size       = "25G"
vfs_cache_min_free_space = "10G"
dir_cache_time           = "72h"
poll_interval            = "30s"
transfers                = 4
buffer_size              = "8M"
vfs_read_chunk_size      = "32M"

[remote.overlay]
enabled = true

[remote.index]
full_crawl_interval = "24h"
opportunistic       = true
tpslimit            = 10

[remote.offline]
writes     = "queue"
outbox_dir = "~/OneDrive.pending"

[[remote.pin]]
name                   = "Keepass"
remote_path            = "Dokumente/Keepass"
local_path             = "~/onedrive-keepass"
interval               = "5m"
sync_on_change         = true
conflict               = "keep-both"
upload_conflict_copies = true
max_delete_percent     = 50
versions               = true
versions_retention     = "14d"
versions_max_size      = "50M"
"#;

    #[test]
    fn documented_config_parses_and_validates() {
        let c: Config = toml::from_str(DOCUMENTED).unwrap();
        c.validate(&paths())
            .expect("documented config should be valid");

        assert_eq!(c.remotes.len(), 1);
        let r = &c.remotes[0];
        assert_eq!(r.name, "onedrive-timm");
        assert_eq!(r.mount_point(&paths()), Path::new("/home/t/OneDrive"));
        assert_eq!(r.mount.vfs_cache_max_age.as_secs(), 30 * 86_400);
        assert_eq!(r.mount.vfs_cache_mode, VfsCacheMode::Full);

        let pin = &r.pins[0];
        assert_eq!(pin.name, "Keepass");
        assert_eq!(
            pin.local_path(&paths()),
            Path::new("/home/t/onedrive-keepass")
        );
        assert_eq!(
            pin.versions_dir(&paths()),
            Path::new("/home/t/onedrive-keepass/.versions")
        );
        assert_eq!(
            pin.remote_spec("onedrive-timm"),
            "onedrive-timm:Dokumente/Keepass"
        );
        assert_eq!(pin.conflict, ConflictPolicy::KeepBoth);
        // Not stated in the file, so this is the default.
        assert!(pin.enabled, "pins must default to enabled");
    }

    #[test]
    fn minimal_config_fills_in_every_default() {
        let c: Config = toml::from_str(
            r#"
            [[remote]]
            name = "od"
            mount_point = "~/OneDrive"
            "#,
        )
        .unwrap();
        c.validate(&paths()).unwrap();

        let r = &c.remotes[0];
        assert!(r.enabled);
        assert!(r.overlay.enabled);
        assert!(r.index.opportunistic);
        assert_eq!(r.mount.transfers, 4);
        assert_eq!(r.mount.poll_interval.as_secs(), 30);
        assert_eq!(r.mount.dir_cache_time.as_secs(), 72 * 3600);
        assert_eq!(r.offline.writes, OfflineWrites::Queue);
        assert_eq!(r.offline.outbox_dir, "~/OneDrive.pending");
        assert_eq!(c.daemon.log_level, LogLevel::Info);
        assert_eq!(c.daemon.rclone_log_level, RcloneLogLevel::Notice);
        assert_eq!(
            r.rclone_cache_dir(&paths()),
            Path::new("/home/t/.cache/onedrive/od/rclone")
        );
    }

    #[test]
    fn empty_config_is_valid_so_a_fresh_install_starts() {
        let c: Config = toml::from_str("").unwrap();
        c.validate(&paths()).unwrap();
        assert!(c.remotes.is_empty());
    }

    /// The old loader ignored unknown keys, so a typo silently did nothing.
    #[test]
    fn unknown_keys_are_rejected() {
        let err = toml::from_str::<Config>(
            r#"
            [[remote]]
            name = "od"
            mount_point = "~/OneDrive"
            [remote.mount]
            vfs_cach_mode = "full"
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("vfs_cach_mode"), "{err}");
    }

    #[test]
    fn invalid_enum_values_are_rejected_at_parse_time() {
        // This is the typo that used to reach rclone and kill the mount instantly.
        let err = toml::from_str::<Config>(
            r#"
            [[remote]]
            name = "od"
            mount_point = "~/OneDrive"
            [remote.mount]
            vfs_cache_mode = "ful"
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("ful"), "{err}");
    }

    fn problems_for(toml_src: &str) -> String {
        let c: Config = toml::from_str(toml_src).unwrap();
        c.validate(&paths()).unwrap_err().msg
    }

    #[test]
    fn rejects_a_pin_inside_a_mount_point() {
        let msg = problems_for(
            r#"
            [[remote]]
            name = "od"
            mount_point = "~/OneDrive"
            [[remote.pin]]
            name = "Keepass"
            remote_path = "Dokumente/Keepass"
            local_path = "~/OneDrive/Dokumente/Keepass"
            "#,
        );
        assert!(msg.contains("is inside the mount point"), "{msg}");
    }

    #[test]
    fn rejects_overlapping_mount_points() {
        let msg = problems_for(
            r#"
            [[remote]]
            name = "a"
            mount_point = "~/OneDrive"
            [[remote]]
            name = "b"
            mount_point = "~/OneDrive/nested"
            "#,
        );
        assert!(msg.contains("overlap"), "{msg}");
    }

    #[test]
    fn rejects_duplicate_remote_and_pin_names() {
        let msg = problems_for(
            r#"
            [[remote]]
            name = "od"
            mount_point = "~/A"
            [[remote.pin]]
            name = "dup"
            remote_path = "x"
            local_path = "~/x"
            [[remote.pin]]
            name = "dup"
            remote_path = "y"
            local_path = "~/y"
            "#,
        );
        assert!(msg.contains("pin name \"dup\" is used 2 times"), "{msg}");

        let msg = problems_for(
            r#"
            [[remote]]
            name = "od"
            mount_point = "~/A"
            [[remote]]
            name = "od"
            mount_point = "~/B"
            "#,
        );
        assert!(msg.contains("remote name \"od\" is used 2 times"), "{msg}");
    }

    #[test]
    fn rejects_poll_interval_not_smaller_than_dir_cache_time() {
        let msg = problems_for(
            r#"
            [[remote]]
            name = "od"
            mount_point = "~/OneDrive"
            [remote.mount]
            poll_interval  = "10m"
            dir_cache_time = "5m"
            "#,
        );
        assert!(msg.contains("must be smaller than"), "{msg}");
    }

    #[test]
    fn rejects_cache_modes_that_cannot_serve_offline_reads() {
        let msg = problems_for(
            r#"
            [[remote]]
            name = "od"
            mount_point = "~/OneDrive"
            [remote.mount]
            vfs_cache_mode = "off"
            "#,
        );
        assert!(msg.contains("cannot serve offline reads"), "{msg}");

        // ...but it is fine if the overlay is disabled, since offline reads are
        // then not promised in the first place.
        let c: Config = toml::from_str(
            r#"
            [[remote]]
            name = "od"
            mount_point = "~/OneDrive"
            [remote.mount]
            vfs_cache_mode = "off"
            [remote.overlay]
            enabled = false
            "#,
        )
        .unwrap();
        c.validate(&paths()).unwrap();
    }

    /// The `extra_flags` footgun: one argv element per entry, so the space form
    /// never worked and rclone just rejected it.
    #[test]
    fn rejects_extra_flags_written_with_a_space() {
        let msg = problems_for(
            r#"
            [[remote]]
            name = "od"
            mount_point = "~/OneDrive"
            [remote.mount]
            extra_flags = ["--vfs-read-ahead 128M"]
            "#,
        );
        assert!(msg.contains("write it as --flag=value"), "{msg}");

        let c: Config = toml::from_str(
            r#"
            [[remote]]
            name = "od"
            mount_point = "~/OneDrive"
            [remote.mount]
            extra_flags = ["--vfs-read-ahead=128M"]
            "#,
        )
        .unwrap();
        c.validate(&paths()).unwrap();
    }

    #[test]
    fn rejects_bad_names_paths_and_ranges() {
        let msg = problems_for(
            r#"
            [daemon]
            log_retention_days = 0
            [[remote]]
            name = "has space"
            mount_point = "relative/path"
            [remote.mount]
            transfers = 0
            [remote.index]
            tpslimit = 0
            [[remote.pin]]
            name = "p"
            remote_path = "/absolute"
            local_path = "~/p"
            interval = "0s"
            "#,
        );
        for expected in [
            "log_retention_days must be at least 1",
            "name may only contain",
            "must be an absolute path",
            "mount.transfers must be between 1 and 64",
            "index.tpslimit must be between 1 and 100",
            "drop the leading '/'",
            "interval must not be zero",
        ] {
            assert!(msg.contains(expected), "missing {expected:?} in:\n{msg}");
        }
    }

    #[test]
    fn reports_every_problem_at_once() {
        let msg = problems_for(
            r#"
            [[remote]]
            name = ""
            mount_point = ""
            "#,
        );
        assert!(msg.starts_with("2 problems in the config:"), "{msg}");
    }

    #[test]
    fn round_trips_through_save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        let original: Config = toml::from_str(DOCUMENTED).unwrap();
        original.save(&path).unwrap();
        let reloaded = Config::load_validated(&path, &paths()).unwrap();
        assert_eq!(original, reloaded);

        // Durations and sizes survive as their canonical strings.
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("vfs_cache_max_age = \"30d\""), "{text}");
        assert!(text.contains("vfs_cache_max_size = \"25G\""), "{text}");
        assert!(text.contains("interval = \"5m\""), "{text}");
    }

    #[test]
    fn save_is_atomic_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("config.toml");
        Config::default().save(&path).unwrap();

        assert!(path.exists());
        assert!(!path.with_extension("toml.tmp").exists());
    }

    #[test]
    fn load_reports_a_missing_file_with_its_path() {
        let err = Config::load(Path::new("/nonexistent/onedrive/config.toml")).unwrap_err();
        assert_eq!(err.code, Code::Config);
        assert!(
            err.msg.contains("/nonexistent/onedrive/config.toml"),
            "{}",
            err.msg
        );
    }
}
