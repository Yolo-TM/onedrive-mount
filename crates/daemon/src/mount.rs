//! The mount supervisor.
//!
//! This module exists because of three specific bugs in the old
//! `mount_manager.rs`, and its structure is dictated by them:
//!
//! **Finding 1 — a mount that crashed twice was never retried again.** The old
//! code kept `restart_count` and `restart_not_before` *inside* the per-mount
//! entry, then `remove`d that entry before checking the backoff. The state it
//! needed to decide "retry later" was destroyed by the act of noticing the
//! crash, and nothing ever called `start()` again except a config edit or a
//! daemon restart. Here, [`Backoff`] lives in a map keyed by remote name that is
//! never cleared on failure — only on success.
//!
//! **Finding 2 — a hung FUSE mount froze the entire daemon.** The old health
//! check awaited `tokio::fs::metadata()` on the mount point directly inside the
//! main `select!`. A mount that was connected but not responding made that await
//! never return, so config reload, signal handling, status writes and sync
//! scheduling all stopped forever. Here every probe is wrapped in a timeout, and
//! a probe that has not come back is not re-issued — see [`Probe`].
//!
//! **Finding 5 — abandoned rclone children.** Handled in `rclone.rs` by
//! `kill_on_drop(true)`, but it matters here too: dropping a [`MountEntry`] is
//! sufficient to stop its child.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use onedrive_rc_core::Code;
use onedrive_rc_core::config::{RcloneLogLevel, RemoteConfig};
use onedrive_rc_core::ipc::MountState;
use onedrive_rc_core::paths::Paths;
use tokio::process::Child;
use tracing::{debug, error, info, warn};

use crate::odfs::Odfs;
use crate::rclone;

/// Backoff ladder, in seconds. Caps rather than terminating: the last value
/// repeats forever, because a daemon that has given up on mounting is useless.
const BACKOFF_LADDER: &[u64] = &[2, 5, 15, 60, 300];

/// How long a liveness probe may take before we treat the mount as hung.
///
/// Generous on purpose — a cold OneDrive directory listing can genuinely take a
/// second or two — but bounded, which is the entire point.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long after a mount is spawned before we expect it to be usable. rclone
/// has to authenticate and mount before the directory becomes readable.
const STARTUP_GRACE: Duration = Duration::from_secs(30);

/// How long to let rclone unmount itself before forcing it.
///
/// rclone handles `SIGTERM` by detaching the mount cleanly (verified against
/// 1.75.1), which is the difference between leaving `~/OneDrive` usable and
/// leaving it returning `ENOTCONN` until something detaches it.
const GRACEFUL_STOP: Duration = Duration::from_secs(5);

/// Whether the odfs overlay exists.
///
/// With it on, rclone mounts at the hidden backing path and odfs (built in P3)
/// sits at the user-facing one instead, giving offline directory listings.
/// `[remote.overlay] enabled = false` selects the pre-P3 behaviour per remote
/// — rclone straight at the path the user browses, no offline listings — which
/// is also the documented fallback if the double-FUSE read path ever costs
/// more latency than it is worth (§5).
const OVERLAY_IMPLEMENTED: bool = true;

/// Where rclone should actually mount for this remote.
///
/// One function so the supervisor, the status report and the probes cannot
/// disagree about it — a mismatch here would have `status` cheerfully reporting
/// a path that nothing is mounted on.
pub fn rclone_mount_point(remote: &RemoteConfig, paths: &Paths) -> PathBuf {
    if OVERLAY_IMPLEMENTED && remote.overlay.enabled {
        paths.backing_mount(&remote.name)
    } else {
        remote.mount_point(paths)
    }
}

// ---------------------------------------------------------------------------
// Backoff
// ---------------------------------------------------------------------------

/// Restart pacing for one remote. **Kept outside the mount entry on purpose.**
#[derive(Debug, Default, Clone)]
pub struct Backoff {
    attempts: u32,
    next_attempt_at: Option<tokio::time::Instant>,
}

impl Backoff {
    /// Whether we are allowed to try starting this mount right now.
    fn ready(&self) -> bool {
        match self.next_attempt_at {
            Some(at) => tokio::time::Instant::now() >= at,
            None => true,
        }
    }

    fn remaining(&self) -> Option<Duration> {
        let at = self.next_attempt_at?;
        at.checked_duration_since(tokio::time::Instant::now())
    }

    /// Record a failure and schedule the next attempt.
    fn record_failure(&mut self) -> Duration {
        let index = (self.attempts as usize).min(BACKOFF_LADDER.len() - 1);
        let base = BACKOFF_LADDER[index];
        self.attempts = self.attempts.saturating_add(1);

        // Jitter so several remotes failing together do not retry in lockstep.
        let jitter = Duration::from_millis(fastish_jitter(base));
        let delay = Duration::from_secs(base) + jitter;
        self.next_attempt_at = Some(tokio::time::Instant::now() + delay);
        delay
    }

    /// Clear the ladder once the mount is actually working.
    fn record_success(&mut self) {
        self.attempts = 0;
        self.next_attempt_at = None;
    }

    pub fn attempts(&self) -> u32 {
        self.attempts
    }
}

/// Up to 25% of the base delay, derived from the clock. Not cryptographic, and
/// does not need to be: it only has to decorrelate retries.
fn fastish_jitter(base_secs: u64) -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let span = (base_secs * 250).max(1);
    nanos % span
}

// ---------------------------------------------------------------------------
// Probe
// ---------------------------------------------------------------------------

/// Guards against piling up probes against an unresponsive mount.
///
/// A probe that times out leaves a blocking thread stuck in `stat()` on a dead
/// FUSE mount — the kernel will not let it go until the mount is detached. We
/// abandon waiting on it (that is what the timeout is for), but we must not
/// start another one every tick, or the blocking pool grows without bound. The
/// flag is cleared only when a probe actually returns.
#[derive(Debug, Default)]
struct Probe {
    in_flight: Arc<AtomicBool>,
}

impl Probe {
    /// Claim the right to probe, or `None` if one is still outstanding.
    fn claim(&self) -> Option<ProbeGuard> {
        self.in_flight
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| ProbeGuard(Arc::clone(&self.in_flight)))
    }
}

/// Clears the in-flight flag when the probe completes, however it completes.
struct ProbeGuard(Arc<AtomicBool>);

impl Drop for ProbeGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// What a liveness probe found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    /// Mounted and answering.
    Healthy,
    /// The directory exists but is not a FUSE mount — rclone is not serving it.
    NotMounted,
    /// `ENOTCONN`: a FUSE mount whose server has died. Must be detached before
    /// anything can mount there again.
    Stale,
    /// The probe did not come back within [`PROBE_TIMEOUT`].
    Hung,
}

/// Check that a mount point is actually usable.
///
/// Deliberately stronger than the old `st_dev` comparison, which only proved
/// *something* was mounted: a mount that is connected but not responding passed
/// that check while being useless. This opens the directory and reads an entry,
/// which is what a user actually does.
async fn probe(mount_point: PathBuf) -> Liveness {
    let path = mount_point.clone();
    let work = tokio::task::spawn_blocking(move || {
        // `read_dir` + one `next()` is the cheapest operation that proves the
        // FUSE server is answering requests.
        match std::fs::read_dir(&path) {
            Ok(mut entries) => match entries.next() {
                // Nothing in it. An empty remote is a legitimately working
                // mount, but so is a plain directory left behind after rclone
                // exited and FUSE detached — and those must not look alike, or
                // a dead mount reads as healthy forever. Only the device-id
                // check can tell them apart.
                None => Ok(is_mount_point(&path)),
                Some(Ok(_)) => Ok(true),
                Some(Err(e)) => Err(e),
            },
            Err(e) => Err(e),
        }
    });

    match tokio::time::timeout(PROBE_TIMEOUT, work).await {
        Ok(Ok(Ok(true))) => Liveness::Healthy,
        Ok(Ok(Ok(false))) => Liveness::NotMounted,
        Ok(Ok(Err(e))) => match e.raw_os_error() {
            // ENOTCONN — the classic post-suspend stale mount.
            Some(107) => Liveness::Stale,
            _ => Liveness::NotMounted,
        },
        // The blocking task itself panicked or was cancelled.
        Ok(Err(_)) => Liveness::NotMounted,
        Err(_) => Liveness::Hung,
    }
}

// ---------------------------------------------------------------------------
// Supervisor
// ---------------------------------------------------------------------------

/// One supervised mount.
struct MountEntry {
    /// Dropping this kills rclone, so the entry owns the process lifetime.
    child: Child,
    /// Recorded at spawn so the unmount paths cannot drift from where rclone
    /// was actually mounted, even if the config changes underneath us.
    mount_point: PathBuf,
    /// The argv this mount was started with. Compared against what the current
    /// config would produce, so an edited setting actually takes effect —
    /// otherwise a changed `vfs_cache_max_size` would sit unapplied until the
    /// next daemon restart, which is exactly the kind of "did that work?"
    /// uncertainty this rewrite is meant to remove.
    args: Vec<String>,
    started_at: tokio::time::Instant,
    mounted_since: Option<DateTime<Utc>>,
    /// True once a probe has succeeded, so startup grace is not re-applied.
    ever_healthy: bool,
}

/// Public view of a mount, for the status report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountStatus {
    pub state: MountState,
    pub since: Option<DateTime<Utc>>,
    pub error: Option<String>,
    pub code: Option<Code>,
}

impl MountStatus {
    fn unmounted() -> Self {
        Self {
            state: MountState::Unmounted,
            since: None,
            error: None,
            code: None,
        }
    }
}

pub struct Supervisor {
    paths: Paths,
    mounts: HashMap<String, MountEntry>,
    /// Survives child death — this is the fix for finding 1.
    backoff: HashMap<String, Backoff>,
    probes: HashMap<String, Probe>,
    status: HashMap<String, MountStatus>,
    /// odfs sessions, one per remote with `overlay.enabled`. Dropping an entry
    /// unmounts it — `fuser::BackgroundSession`'s whole contract. Kept
    /// separately from `mounts` because odfs's lifecycle is not tied to
    /// rclone's: odfs tolerates the backing mount being down (that is the
    /// entire point of it), so a flapping rclone child must never force an
    /// odfs remount, and vice versa.
    odfs: HashMap<String, fuser::BackgroundSession>,
    /// Where each odfs session was mounted, for detaching it by path.
    odfs_points: HashMap<String, PathBuf>,
    /// Remotes the user unmounted on purpose. Reconcile leaves them alone until
    /// they are mounted again, otherwise the next tick would undo the unmount.
    /// Deliberately not persisted: a daemon restart mounts everything again.
    held: HashSet<String>,
}

impl Supervisor {
    pub fn new(paths: Paths) -> Self {
        Self {
            paths,
            mounts: HashMap::new(),
            backoff: HashMap::new(),
            probes: HashMap::new(),
            status: HashMap::new(),
            odfs: HashMap::new(),
            odfs_points: HashMap::new(),
            held: HashSet::new(),
        }
    }

    pub fn status(&self, remote: &str) -> MountStatus {
        self.status
            .get(remote)
            .cloned()
            .unwrap_or_else(MountStatus::unmounted)
    }

    pub fn is_mounted(&self, remote: &str) -> bool {
        self.status
            .get(remote)
            .is_some_and(|s| s.state == MountState::Mounted)
    }

    /// Backoff attempts recorded for a remote. For tests and diagnostics.
    pub fn attempts(&self, remote: &str) -> u32 {
        self.backoff.get(remote).map(Backoff::attempts).unwrap_or(0)
    }

    /// Bring every enabled remote to its desired state, and tear down the rest.
    ///
    /// Called on a timer and on config reload. Idempotent, and it is the *only*
    /// place that starts a mount — so there is no path where a dead mount is
    /// left with nobody responsible for restarting it.
    pub async fn reconcile(&mut self, remotes: &[RemoteConfig], log_level: RcloneLogLevel) {
        let enabled: Vec<&RemoteConfig> = remotes.iter().filter(|r| r.enabled).collect();

        // Stop anything no longer wanted. Dropping the entry kills the child.
        let wanted: Vec<&str> = enabled.iter().map(|r| r.name.as_str()).collect();
        let unwanted: Vec<String> = self
            .mounts
            .keys()
            .filter(|name| !wanted.contains(&name.as_str()))
            .cloned()
            .collect();
        for name in unwanted {
            info!(remote = %name, "remote disabled or removed; unmounting");
            self.stop(&name).await;
        }
        self.status
            .retain(|name, _| wanted.contains(&name.as_str()));
        self.backoff
            .retain(|name, _| wanted.contains(&name.as_str()));
        self.probes
            .retain(|name, _| wanted.contains(&name.as_str()));
        // Dropping an entry here unmounts it.
        self.odfs.retain(|name, _| wanted.contains(&name.as_str()));

        for remote in enabled {
            if self.held.contains(&remote.name) {
                continue;
            }
            self.reconcile_one(remote, log_level).await;
            self.reconcile_odfs(remote).await;
        }
    }

    /// Bring odfs to the state `remote.overlay.enabled` asks for.
    ///
    /// Deliberately independent of whether the rclone backing mount is
    /// currently up: odfs degrades to the index and the VFS cache on its own
    /// when the backing mount is unreachable, which is the entire feature.
    async fn reconcile_odfs(&mut self, remote: &RemoteConfig) {
        let name = &remote.name;
        if !remote.overlay.enabled {
            if self.odfs.remove(name).is_some() {
                info!(remote = %name, "overlay disabled; unmounting odfs");
            }
            return;
        }
        if self.odfs.contains_key(name) {
            return;
        }
        self.start_odfs(remote).await;
    }

    async fn start_odfs(&mut self, remote: &RemoteConfig) {
        let name = remote.name.clone();
        let mount_point = remote.mount_point(&self.paths);

        // A session left behind by a killed daemon cannot be mounted over, and
        // it also makes `create_dir_all` fail with EEXIST (stat gives ENOTCONN),
        // so detach it *before* touching the directory.
        if matches!(probe(mount_point.clone()).await, Liveness::Stale) {
            warn!(remote = %name, path = %mount_point.display(), "odfs mount point is stale; detaching before remount");
            rclone::fusermount_unmount(&mount_point).await;
        }
        if let Err(e) = tokio::fs::create_dir_all(&mount_point).await {
            warn!(remote = %name, path = %mount_point.display(), error = %e, "could not create the odfs mount point");
            return;
        }

        // The actual `mount()` syscall and FUSE handshake are synchronous
        // (fuser's own design), so this runs off the async executor the same
        // way any other blocking filesystem call in this daemon does.
        let remote = remote.clone();
        let paths = self.paths.clone();
        let target = mount_point.clone();
        let result =
            tokio::task::spawn_blocking(move || -> anyhow::Result<fuser::BackgroundSession> {
                let fs = Odfs::new(&remote, &paths)?;
                Ok(fs.spawn(&target)?)
            })
            .await;

        match result {
            Ok(Ok(session)) => {
                info!(remote = %name, path = %mount_point.display(), "odfs mounted");
                self.odfs_points.insert(name.clone(), mount_point.clone());
                self.odfs.insert(name, session);
            }
            Ok(Err(e)) => warn!(remote = %name, error = %e, "could not mount odfs; will retry"),
            Err(e) => warn!(remote = %name, error = %e, "odfs mount task panicked"),
        }
    }

    async fn reconcile_one(&mut self, remote: &RemoteConfig, log_level: RcloneLogLevel) {
        let name = remote.name.clone();

        // Has the child exited? `try_wait` is non-blocking.
        if let Some(entry) = self.mounts.get_mut(&name) {
            match entry.child.try_wait() {
                Ok(Some(exit)) => {
                    // Note the ordering: the backoff map is updated, and only
                    // *then* is the entry removed. The old code did the reverse
                    // and lost the state it needed.
                    let delay = self
                        .backoff
                        .entry(name.clone())
                        .or_default()
                        .record_failure();
                    let attempts = self.attempts(&name);
                    warn!(
                        remote = %name,
                        subject = %format!("mount/{name}"),
                        %exit,
                        attempts,
                        retry_in = ?delay,
                        "rclone mount exited; will retry"
                    );
                    self.mounts.remove(&name);
                    self.set_failed(&name, format!("rclone exited with {exit}"), None);
                    return;
                }
                Ok(None) => {} // still running, fall through to probing
                Err(e) => {
                    warn!(remote = %name, error = %e, "could not check on the rclone child");
                    return;
                }
            }
        }

        if self.mounts.contains_key(&name) {
            // An edited mount setting only reaches rclone via a fresh process,
            // so compare what it was started with against what the config now
            // asks for.
            let desired = rclone::mount_args(
                remote,
                &self.paths,
                &rclone_mount_point(remote, &self.paths),
                log_level,
            );
            if self.mounts.get(&name).is_some_and(|e| e.args != desired) {
                info!(
                    remote = %name,
                    "mount settings changed; restarting the mount to apply them"
                );
                self.force_remount(&name).await;
                self.start(remote, log_level).await;
                return;
            }

            self.check_health(remote).await;
            return;
        }

        // Not running. Start it, if the ladder allows.
        let backoff = self.backoff.entry(name.clone()).or_default();
        if !backoff.ready() {
            if let Some(remaining) = backoff.remaining() {
                debug!(remote = %name, ?remaining, "waiting out the mount backoff");
            }
            return;
        }

        self.start(remote, log_level).await;
    }

    async fn start(&mut self, remote: &RemoteConfig, log_level: RcloneLogLevel) {
        let name = remote.name.clone();
        let mount_point = rclone_mount_point(remote, &self.paths);

        // A mount point left in ENOTCONN by a killed rclone cannot be mounted
        // over. Detach it first — this is the post-suspend recovery path.
        if matches!(probe(mount_point.clone()).await, Liveness::Stale) {
            warn!(
                remote = %name,
                path = %mount_point.display(),
                code = Code::MountStale.as_str(),
                "mount point is in a stale FUSE state; detaching before remount"
            );
            rclone::fusermount_unmount(&mount_point).await;
        }

        let desired_args = rclone::mount_args(remote, &self.paths, &mount_point, log_level);
        info!(remote = %name, path = %mount_point.display(), "mounting");
        self.status.insert(
            name.clone(),
            MountStatus {
                state: MountState::Mounting,
                since: None,
                error: None,
                code: None,
            },
        );

        match rclone::spawn_mount(remote, &self.paths, &mount_point, log_level).await {
            Ok(child) => {
                self.mounts.insert(
                    name.clone(),
                    MountEntry {
                        child,
                        mount_point: mount_point.clone(),
                        args: desired_args,
                        started_at: tokio::time::Instant::now(),
                        mounted_since: None,
                        ever_healthy: false,
                    },
                );
            }
            Err(e) => {
                let delay = self
                    .backoff
                    .entry(name.clone())
                    .or_default()
                    .record_failure();
                error!(
                    remote = %name,
                    subject = %format!("mount/{name}"),
                    error = %e,
                    retry_in = ?delay,
                    "could not start rclone"
                );
                self.set_failed(&name, format!("{e:#}"), Some(Code::Dependency));
            }
        }
    }

    async fn check_health(&mut self, remote: &RemoteConfig) {
        let name = remote.name.clone();
        let Some(mount_point) = self.mounts.get(&name).map(|e| e.mount_point.clone()) else {
            return;
        };

        let probe_slot = self.probes.entry(name.clone()).or_default();

        // A probe already outstanding means the last one never came back, i.e.
        // the mount is hung. Do not pile on another.
        let Some(guard) = probe_slot.claim() else {
            warn!(
                remote = %name,
                subject = %format!("mount/{name}"),
                code = Code::MountStale.as_str(),
                "a liveness probe has not returned; treating the mount as hung"
            );
            self.handle_hung(&name).await;
            return;
        };

        let liveness = probe(mount_point.clone()).await;
        drop(guard);

        let entry = match self.mounts.get_mut(&name) {
            Some(entry) => entry,
            // Reconciled away while we were probing.
            None => return,
        };
        let within_grace = !entry.ever_healthy && entry.started_at.elapsed() < STARTUP_GRACE;

        match liveness {
            Liveness::Healthy => {
                let first_time = !entry.ever_healthy;
                entry.ever_healthy = true;
                if entry.mounted_since.is_none() {
                    entry.mounted_since = Some(Utc::now());
                }
                let since = entry.mounted_since;

                if first_time {
                    info!(
                        remote = %name,
                        path = %mount_point.display(),
                        "mount is up"
                    );
                }
                self.backoff
                    .entry(name.clone())
                    .or_default()
                    .record_success();
                self.status.insert(
                    name.clone(),
                    MountStatus {
                        state: MountState::Mounted,
                        since,
                        error: None,
                        code: None,
                    },
                );
            }

            Liveness::NotMounted if within_grace => {
                debug!(remote = %name, "still coming up");
            }

            Liveness::NotMounted => {
                warn!(
                    remote = %name,
                    subject = %format!("mount/{name}"),
                    "rclone is running but the mount point is not served; restarting it"
                );
                self.restart(&name, "mount point not served", None).await;
            }

            Liveness::Stale => {
                warn!(
                    remote = %name,
                    subject = %format!("mount/{name}"),
                    code = Code::MountStale.as_str(),
                    "mount is in a stale FUSE state; detaching and restarting"
                );
                rclone::fusermount_unmount(&mount_point).await;
                self.restart(&name, "stale FUSE mount", Some(Code::MountStale))
                    .await;
            }

            Liveness::Hung => {
                warn!(
                    remote = %name,
                    subject = %format!("mount/{name}"),
                    code = Code::MountStale.as_str(),
                    timeout = ?PROBE_TIMEOUT,
                    "mount did not answer; restarting it"
                );
                self.handle_hung(&name).await;
            }
        }
    }

    /// A hung mount has to be detached before rclone can be replaced, otherwise
    /// the new process cannot mount over it.
    async fn handle_hung(&mut self, name: &str) {
        let mount_point = self.recorded_mount_point(name);
        rclone::fusermount_unmount(&mount_point).await;
        self.restart(name, "mount stopped responding", Some(Code::MountStale))
            .await;
    }

    /// Kill the child and record a failure, so the ladder paces the restart.
    async fn restart(&mut self, name: &str, why: &str, code: Option<Code>) {
        let mount_point = self.recorded_mount_point(name);
        if let Some(entry) = self.mounts.remove(name) {
            // Terminate rather than drop, so we can wait and be sure the old
            // process is gone before starting another against the same path.
            self.terminate(entry, &mount_point).await;
        }
        let delay = self
            .backoff
            .entry(name.to_owned())
            .or_default()
            .record_failure();
        debug!(remote = %name, retry_in = ?delay, "mount restart scheduled");
        self.set_failed(name, why.to_owned(), code);
    }

    /// Stop a mount and forget it. Used when a remote is disabled or removed,
    /// and on shutdown.
    pub async fn stop(&mut self, name: &str) {
        let mount_point = self.recorded_mount_point(name);

        if let Some(entry) = self.mounts.remove(name) {
            self.terminate(entry, &mount_point).await;
        }
        // Belt and braces: if rclone did not detach — killed, or already gone —
        // make sure the mount point is not left returning ENOTCONN.
        if matches!(probe(mount_point.clone()).await, Liveness::Stale) {
            rclone::fusermount_unmount(&mount_point).await;
        }

        // rclone does not always remove its own rc socket, and a leftover one
        // stops the socket directory being cleaned up at exit.
        let _ = tokio::fs::remove_file(rclone::rc_socket(&self.paths, name)).await;

        self.status
            .insert(name.to_owned(), MountStatus::unmounted());
    }

    /// Detach every FUSE mount before the machine suspends.
    ///
    /// A live mount whose server is frozen (or stuck on a dead network) leaves
    /// anything that touches it — Dolphin, direnv, a shell — in
    /// uninterruptible sleep inside `fuse_dentry_revalidate`, which the
    /// freezer cannot freeze: suspend then fails and retries for minutes. A
    /// lazy unmount plus dropping the sessions aborts the connections, so
    /// clients get `ENOTCONN` instead of hanging. The reconcile loop mounts
    /// everything again on resume.
    ///
    /// Bounded by `budget` as a whole; it must fit inside logind's inhibitor
    /// delay, so nothing here waits on rclone to flush.
    pub async fn release_for_suspend(&mut self, budget: Duration) {
        let deadline = tokio::time::Instant::now() + budget;

        // The overlay first: it is what the user's programs actually touch.
        let sessions: Vec<(String, fuser::BackgroundSession)> = self.odfs.drain().collect();
        for (name, _) in &sessions {
            if let Some(point) = self.odfs_points.get(name) {
                rclone::fusermount_unmount(point).await;
            }
        }
        // Dropping a session joins its thread, which can block on a handler
        // that is waiting on the (possibly hung) backing mount.
        let drop_sessions = tokio::task::spawn_blocking(move || drop(sessions));
        let _ = tokio::time::timeout_at(deadline, drop_sessions).await;

        let names: Vec<String> = self.mounts.keys().cloned().collect();
        for name in names {
            let Some(mut entry) = self.mounts.remove(&name) else {
                continue;
            };
            if let Some(pid) = entry.child.id() {
                // SAFETY: we own this child and have not yet reaped it.
                unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
            }
            rclone::fusermount_unmount(&entry.mount_point).await;
            if tokio::time::timeout_at(deadline, entry.child.wait())
                .await
                .is_err()
            {
                let _ = entry.child.start_kill();
            }
            let _ = tokio::fs::remove_file(rclone::rc_socket(&self.paths, &name)).await;
            self.status.insert(name, MountStatus::unmounted());
        }
        info!("mounts released for suspend");
    }

    /// Ask rclone to stop, and only force it if it will not.
    ///
    /// `SIGKILL` is the wrong default here: it leaves the FUSE mount attached
    /// but dead, so the user's directory returns `ENOTCONN` until something
    /// detaches it. `SIGTERM` makes rclone unmount itself cleanly (verified
    /// against 1.75.1).
    async fn terminate(&self, mut entry: MountEntry, mount_point: &Path) {
        let Some(pid) = entry.child.id() else {
            // Already exited and reaped.
            return;
        };

        // SAFETY: `pid` belongs to a child we own and have not yet waited on, so
        // it cannot have been recycled by another process.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };

        match tokio::time::timeout(GRACEFUL_STOP, entry.child.wait()).await {
            Ok(Ok(status)) => {
                debug!(path = %mount_point.display(), %status, "rclone stopped cleanly");
            }
            Ok(Err(e)) => debug!(error = %e, "waiting for rclone failed"),
            Err(_) => {
                warn!(
                    path = %mount_point.display(),
                    timeout = ?GRACEFUL_STOP,
                    "rclone did not stop on its own; killing it"
                );
                let _ = entry.child.kill().await;
            }
        }
    }

    /// Unmount the overlay for `name`, lazily first so a busy `~/OneDrive`
    /// (an open file, a shell inside it) cannot keep the old session mounted
    /// and make the replacement mount fail or stack on top of it.
    async fn detach_odfs(&mut self, name: &str) {
        let session = self.odfs.remove(name);
        let point = self.odfs_points.remove(name);
        if let Some(point) = point
            && session.is_some()
        {
            rclone::fusermount_unmount(&point).await;
        }
        // Joining the session thread can block on a handler waiting for the
        // backing mount, so keep it off the executor.
        if let Some(session) = session {
            let _ = tokio::time::timeout(
                Duration::from_secs(5),
                tokio::task::spawn_blocking(move || drop(session)),
            )
            .await;
        }
    }

    /// Unmount a remote on the user's say-so and keep it unmounted until
    /// [`Supervisor::mount_now`]. Driven by `onedrive-rc mount unmount`.
    pub async fn unmount_now(&mut self, name: &str) {
        info!(remote = %name, "unmount requested");
        self.held.insert(name.to_owned());
        self.detach_odfs(name).await;
        self.stop(name).await;
    }

    /// Lift a hold from [`Supervisor::unmount_now`]; the next reconcile mounts
    /// the remote. A no-op for a remote that is already mounted.
    pub fn mount_now(&mut self, name: &str) {
        info!(remote = %name, "mount requested");
        self.held.remove(name);
        self.backoff
            .entry(name.to_owned())
            .or_default()
            .record_success();
    }

    /// Whether the user unmounted this remote on purpose.
    pub fn is_held(&self, name: &str) -> bool {
        self.held.contains(name)
    }

    /// Force an immediate remount, bypassing the backoff. Driven by
    /// `onedrive-rc mount remount`.
    pub async fn force_remount(&mut self, name: &str) {
        info!(remote = %name, "remount requested");
        self.held.remove(name);
        self.detach_odfs(name).await;
        let mount_point = self.recorded_mount_point(name);
        if let Some(entry) = self.mounts.remove(name) {
            self.terminate(entry, &mount_point).await;
        }
        if matches!(probe(mount_point.clone()).await, Liveness::Stale) {
            rclone::fusermount_unmount(&mount_point).await;
        }
        // A manual remount is an explicit instruction, so clear the ladder
        // rather than making the user wait out a backoff they did not cause.
        self.backoff
            .entry(name.to_owned())
            .or_default()
            .record_success();
        self.status
            .insert(name.to_owned(), MountStatus::unmounted());
    }

    /// Stop everything. Called on shutdown.
    pub async fn stop_all(&mut self) {
        let names: Vec<String> = self.mounts.keys().cloned().collect();
        for name in names {
            self.stop(&name).await;
        }
        // Drop unmounts each session.
        self.odfs.clear();
    }

    /// Whether odfs is currently mounted for this remote. For tests and
    /// `status`.
    pub fn odfs_mounted(&self, remote: &str) -> bool {
        self.odfs.contains_key(remote)
    }

    /// Ask every healthy mount to re-read its directory listings.
    ///
    /// Called on resume and on reconnect: after a suspend the VFS's cached
    /// directories can be arbitrarily stale, and `dir_cache_time` is 3 days.
    pub async fn refresh_all(&self) {
        for name in self.mounts.keys() {
            if !self.is_mounted(name) {
                continue;
            }
            match rclone::vfs_refresh(&self.paths, name, false).await {
                Ok(()) => debug!(remote = %name, "directory cache refreshed"),
                Err(e) => debug!(remote = %name, error = %e, "vfs/refresh failed"),
            }
        }
    }

    fn set_failed(&mut self, name: &str, error: String, code: Option<Code>) {
        self.status.insert(
            name.to_owned(),
            MountStatus {
                state: MountState::Failed,
                since: None,
                error: Some(error),
                code,
            },
        );
    }

    /// Whether any supervised mount is currently unhealthy. Feeds the daemon
    /// state so `systemctl status` reflects reality.
    pub fn any_unhealthy(&self) -> bool {
        self.status
            .values()
            .any(|s| matches!(s.state, MountState::Failed | MountState::Unmounted))
    }

    /// Where a remote is (or was) mounted. Falls back to the backing path when
    /// we have no record, which only happens before the first start.
    fn recorded_mount_point(&self, remote: &str) -> PathBuf {
        self.mounts
            .get(remote)
            .map(|e| e.mount_point.clone())
            .unwrap_or_else(|| self.paths.backing_mount(remote))
    }
}

/// Whether `path` is a mount point, by comparing device ids with its parent.
///
/// Necessary but not sufficient for liveness: this was the old implementation's
/// *only* probe, and it passes for a mount that is connected yet not answering.
/// Here it is used solely to disambiguate the empty-directory case, where
/// reading the directory cannot tell a working empty mount from a plain
/// directory left behind by a dead rclone.
fn is_mount_point(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;

    let Ok(here) = std::fs::metadata(path) else {
        return false;
    };
    let Some(parent) = path.parent() else {
        return false;
    };
    let Ok(up) = std::fs::metadata(parent) else {
        return false;
    };
    here.dev() != up.dev()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_backoff_ladder_climbs_and_then_caps() {
        let mut b = Backoff::default();
        assert!(b.ready(), "a fresh backoff permits an immediate attempt");

        let mut delays = Vec::new();
        for _ in 0..8 {
            delays.push(b.record_failure().as_secs());
        }

        // Each step is at least the ladder value (jitter only adds).
        assert!(delays[0] >= 2 && delays[0] < 4, "{delays:?}");
        assert!(delays[1] >= 5 && delays[1] < 8, "{delays:?}");
        assert!(delays[2] >= 15 && delays[2] < 20, "{delays:?}");
        assert!(delays[3] >= 60 && delays[3] < 80, "{delays:?}");
        // Capped, not terminated — it keeps retrying forever.
        for d in &delays[4..] {
            assert!(*d >= 300 && *d < 380, "{delays:?}");
        }
        assert_eq!(b.attempts(), 8);
    }

    #[test]
    fn a_failure_blocks_the_next_attempt_until_the_delay_passes() {
        let mut b = Backoff::default();
        b.record_failure();
        assert!(!b.ready(), "must wait after a failure");
        assert!(b.remaining().is_some());
    }

    #[test]
    fn success_clears_the_ladder() {
        let mut b = Backoff::default();
        b.record_failure();
        b.record_failure();
        assert_eq!(b.attempts(), 2);

        b.record_success();
        assert_eq!(b.attempts(), 0);
        assert!(
            b.ready(),
            "a working mount is immediately retryable if it dies"
        );
        assert!(b.remaining().is_none());
    }

    #[test]
    fn jitter_stays_within_a_quarter_of_the_base() {
        for base in [2u64, 5, 15, 60, 300] {
            let j = fastish_jitter(base);
            assert!(j < base * 250, "base {base} jitter {j}");
        }
    }

    /// The core of finding 1: the backoff survives the entry being removed, so
    /// a mount that crashes repeatedly still gets retried.
    #[test]
    fn backoff_survives_losing_the_mount_entry() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Supervisor::new(Paths::rooted(dir.path()));

        // Simulate two crashes in quick succession.
        s.backoff.entry("od".into()).or_default().record_failure();
        s.mounts.remove("od");
        s.backoff.entry("od".into()).or_default().record_failure();
        s.mounts.remove("od");

        assert_eq!(s.attempts("od"), 2, "both failures were recorded");
        assert!(
            s.backoff.contains_key("od"),
            "the ladder must outlive the entry, or the mount is never retried"
        );
    }

    #[test]
    fn the_probe_guard_admits_one_probe_at_a_time() {
        let p = Probe::default();

        let first = p.claim().expect("first claim succeeds");
        assert!(
            p.claim().is_none(),
            "a second concurrent probe must be refused, or a hung mount leaks \
             a blocking thread every tick"
        );

        drop(first);
        assert!(
            p.claim().is_some(),
            "claimable again once the probe returns"
        );
    }

    #[tokio::test]
    async fn probing_a_real_directory_is_healthy() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), "x").unwrap();
        assert_eq!(probe(dir.path().to_path_buf()).await, Liveness::Healthy);
    }

    /// An empty *plain* directory is what a mount point looks like after rclone
    /// has exited and FUSE has detached. It must not read as healthy, or a dead
    /// mount is never noticed — reading the directory alone cannot tell it apart
    /// from a working but empty remote, so the device-id check decides.
    #[tokio::test]
    async fn an_empty_plain_directory_is_not_mounted() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("empty");
        std::fs::create_dir(&sub).unwrap();
        assert_eq!(probe(sub).await, Liveness::NotMounted);
    }

    /// The converse: a directory with content is serving reads, so it is
    /// healthy whether or not it happens to be a mount point. This is what
    /// makes the probe stronger than the old device-id-only check.
    #[tokio::test]
    async fn a_directory_that_serves_reads_is_healthy() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("populated");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("a"), "x").unwrap();
        assert_eq!(probe(sub).await, Liveness::Healthy);
    }

    #[tokio::test]
    async fn probing_a_missing_directory_is_not_mounted() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope");
        assert_eq!(probe(missing).await, Liveness::NotMounted);
    }

    #[test]
    fn mount_point_detection_distinguishes_a_plain_directory() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        assert!(!is_mount_point(&sub), "a plain subdirectory is not a mount");
        // The root of a tmpfs/overlay genuinely is one, so only assert the
        // negative case, which is the one we rely on.
    }

    #[tokio::test]
    async fn an_unknown_remote_reports_unmounted_rather_than_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let s = Supervisor::new(Paths::rooted(dir.path()));

        let status = s.status("never-heard-of-it");
        assert_eq!(status.state, MountState::Unmounted);
        assert!(!s.is_mounted("never-heard-of-it"));
        assert_eq!(s.attempts("never-heard-of-it"), 0);
    }

    #[tokio::test]
    async fn reconcile_with_no_remotes_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Supervisor::new(Paths::rooted(dir.path()));
        s.reconcile(&[], RcloneLogLevel::Notice).await;
        assert!(!s.any_unhealthy());
    }

    #[tokio::test]
    async fn a_disabled_remote_is_not_started() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Supervisor::new(Paths::rooted(dir.path()));

        let config: onedrive_rc_core::config::Config = toml::from_str(
            r#"
            [[remote]]
            name = "od"
            mount_point = "~/OneDrive"
            enabled = false
            "#,
        )
        .unwrap();

        s.reconcile(&config.remotes, RcloneLogLevel::Notice).await;
        assert!(s.mounts.is_empty(), "a disabled remote must not be mounted");
        assert!(s.status.is_empty());
    }

    /// A changed mount setting only reaches rclone through a new process, so the
    /// supervisor has to notice. The old daemon compared config fields by hand;
    /// comparing the generated argv cannot drift out of step with the flags we
    /// actually pass.
    #[test]
    fn a_changed_mount_setting_changes_the_argv() {
        let paths = Paths::rooted("/home/t");
        let config: onedrive_rc_core::config::Config = toml::from_str(
            r#"
            [[remote]]
            name = "od"
            mount_point = "~/OneDrive"
            "#,
        )
        .unwrap();
        let remote = &config.remotes[0];
        let point = rclone_mount_point(remote, &paths);

        let before = rclone::mount_args(remote, &paths, &point, RcloneLogLevel::Notice);

        let mut changed = remote.clone();
        changed.mount.vfs_cache_max_size = onedrive_rc_core::Size::parse("50G").unwrap();
        let after = rclone::mount_args(&changed, &paths, &point, RcloneLogLevel::Notice);
        assert_ne!(before, after, "a changed size must be visible in the argv");

        // The rclone log level is part of it too, which is what makes editing
        // `rclone_log_level` take effect without a daemon restart.
        let louder = rclone::mount_args(remote, &paths, &point, RcloneLogLevel::Info);
        assert_ne!(before, louder);

        // And an unrelated edit must *not* churn the mount.
        let mut unrelated = remote.clone();
        unrelated.pins.push(
            toml::from_str(
                r#"
                name = "p"
                remote_path = "a"
                local_path = "~/a"
                "#,
            )
            .unwrap(),
        );
        assert_eq!(
            before,
            rclone::mount_args(&unrelated, &paths, &point, RcloneLogLevel::Notice),
            "adding a pin must not restart the mount"
        );
    }

    #[tokio::test]
    async fn force_remount_clears_the_backoff() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Supervisor::new(Paths::rooted(dir.path()));

        s.backoff.entry("od".into()).or_default().record_failure();
        s.backoff.entry("od".into()).or_default().record_failure();
        assert_eq!(s.attempts("od"), 2);

        s.force_remount("od").await;
        assert_eq!(
            s.attempts("od"),
            0,
            "an explicit remount should not wait out a backoff the user did not cause"
        );
        assert!(s.backoff["od"].ready());
    }

    #[tokio::test]
    async fn releasing_for_suspend_stops_the_child_within_budget() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Supervisor::new(Paths::rooted(dir.path()));
        // A child that ignores nothing special: SIGTERM ends it.
        let child = tokio::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .unwrap();
        s.mounts.insert(
            "od".into(),
            MountEntry {
                child,
                mount_point: dir.path().join("mnt"),
                args: Vec::new(),
                started_at: tokio::time::Instant::now(),
                mounted_since: None,
                ever_healthy: false,
            },
        );

        let began = std::time::Instant::now();
        s.release_for_suspend(Duration::from_secs(3)).await;

        assert!(began.elapsed() < Duration::from_secs(4));
        assert!(s.mounts.is_empty());
        assert_eq!(s.status("od").state, MountState::Unmounted);
    }

    #[tokio::test]
    async fn an_unmounted_remote_stays_down_until_mounted_again() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Supervisor::new(Paths::rooted(dir.path()));

        s.unmount_now("od").await;
        assert!(s.is_held("od"));
        assert_eq!(s.status("od").state, MountState::Unmounted);

        s.mount_now("od");
        assert!(!s.is_held("od"));

        // A remount is an explicit "bring it up", so it lifts a hold too.
        s.unmount_now("od").await;
        s.force_remount("od").await;
        assert!(!s.is_held("od"));
    }

    #[tokio::test]
    async fn stopping_an_unmounted_remote_is_harmless() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Supervisor::new(Paths::rooted(dir.path()));
        s.stop("od").await;
        assert_eq!(s.status("od").state, MountState::Unmounted);
        s.stop_all().await;
    }

    #[tokio::test]
    async fn unhealthy_is_reported_for_failed_and_unmounted() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Supervisor::new(Paths::rooted(dir.path()));
        assert!(!s.any_unhealthy(), "nothing configured is not unhealthy");

        s.set_failed("od", "boom".into(), Some(Code::MountStale));
        assert!(s.any_unhealthy());
        let status = s.status("od");
        assert_eq!(status.state, MountState::Failed);
        assert_eq!(status.error.as_deref(), Some("boom"));
        assert_eq!(status.code, Some(Code::MountStale));
    }

    // -- odfs --------------------------------------------------------------
    //
    // odfs needs no rclone remote at all to mount — it is our own FUSE
    // filesystem, not rclone's — so these reconcile against a remote name
    // that does not exist in any rclone.conf. The rclone backing mount fails
    // and backs off exactly as it does in the tests above; the point is that
    // odfs mounts successfully anyway, which is the whole reason its lifecycle
    // is tracked separately from `mounts`.

    fn remote_config(extra: &str) -> onedrive_rc_core::config::Config {
        toml::from_str(&format!(
            r#"
            [[remote]]
            name = "od"
            mount_point = "~/OneDrive"
            {extra}
            "#
        ))
        .unwrap()
    }

    #[tokio::test]
    async fn odfs_mounts_even_though_the_rclone_backing_mount_cannot() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Supervisor::new(Paths::rooted(dir.path()));
        let config = remote_config("");

        s.reconcile(&config.remotes, RcloneLogLevel::Notice).await;

        assert!(
            s.odfs_mounted("od"),
            "odfs does not depend on rclone at all"
        );
        assert_ne!(
            s.status("od").state,
            MountState::Mounted,
            "the rclone backing mount, meanwhile, has nothing to mount"
        );

        // And it is a real, listable mount — empty, since nothing backs the
        // remote and there are no pins, but not an error.
        let listing = std::fs::read_dir(dir.path().join("OneDrive"));
        assert!(listing.is_ok(), "{listing:?}");

        s.stop_all().await;
    }

    #[tokio::test]
    async fn disabling_the_overlay_unmounts_odfs_on_the_next_reconcile() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Supervisor::new(Paths::rooted(dir.path()));

        let on = remote_config("");
        s.reconcile(&on.remotes, RcloneLogLevel::Notice).await;
        assert!(s.odfs_mounted("od"));

        let off = remote_config("[remote.overlay]\nenabled = false");
        s.reconcile(&off.remotes, RcloneLogLevel::Notice).await;
        assert!(
            !s.odfs_mounted("od"),
            "turning the overlay off must unmount it"
        );

        s.stop_all().await;
    }

    #[tokio::test]
    async fn removing_the_remote_unmounts_odfs_too() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Supervisor::new(Paths::rooted(dir.path()));

        let config = remote_config("");
        s.reconcile(&config.remotes, RcloneLogLevel::Notice).await;
        assert!(s.odfs_mounted("od"));

        s.reconcile(&[], RcloneLogLevel::Notice).await;
        assert!(
            !s.odfs_mounted("od"),
            "a removed remote must not stay mounted"
        );
    }

    #[tokio::test]
    async fn reconciling_again_does_not_remount_an_already_mounted_odfs() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Supervisor::new(Paths::rooted(dir.path()));
        let config = remote_config("");

        s.reconcile(&config.remotes, RcloneLogLevel::Notice).await;
        assert!(s.odfs_mounted("od"));
        s.reconcile(&config.remotes, RcloneLogLevel::Notice).await;
        assert!(
            s.odfs_mounted("od"),
            "a second reconcile must not disturb a session that is already up"
        );

        s.stop_all().await;
    }

    #[tokio::test]
    async fn stop_all_unmounts_odfs() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Supervisor::new(Paths::rooted(dir.path()));
        let config = remote_config("");

        s.reconcile(&config.remotes, RcloneLogLevel::Notice).await;
        assert!(s.odfs_mounted("od"));

        s.stop_all().await;
        assert!(!s.odfs_mounted("od"));
    }
}
