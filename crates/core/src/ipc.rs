//! The daemon's control protocol.
//!
//! Newline-delimited JSON over a unix socket. One request, one response, then
//! the connection closes — there is no session state, which keeps both ends
//! trivial and means a wedged client cannot hold daemon resources.
//!
//! [`StatusReport`] is deliberately the single source of truth for `status`,
//! `doctor` and the TUI, so the three can never disagree about health.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::code::Code;

/// Incremented on incompatible protocol changes so a mismatched CLI says so
/// plainly instead of failing to parse. The old GUI's equivalent failure mode
/// was a silent permanent "Waiting for daemon status…".
pub const PROTOCOL_VERSION: u32 = 2;

/// Refuse absurd frames rather than allocating on demand.
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Liveness and protocol handshake.
    Ping,
    /// Full health snapshot.
    Status,
    /// Re-read the config now instead of waiting for the inotify watch.
    Reload,
    /// Sync one pin, or all of them.
    PinSync { name: Option<String> },
    /// Force a remount.
    MountRemount { remote: Option<String> },
    /// Refresh the offline index.
    IndexRefresh { full: bool },
    /// Every conflict that is still open.
    ConflictList,
    /// Act on one conflict. The only request in the protocol that can delete a
    /// file, which is why it names an id rather than a path: the id has to have
    /// come from a [`Request::ConflictList`] the user actually looked at.
    ConflictResolve { id: i64, keep: Keep },
    /// Every write still queued in the outbox.
    OutboxList,
    /// Replay the outbox now instead of waiting for the next reconnect, for
    /// one remote or every enabled one.
    OutboxRetry { remote: Option<String> },
    /// Drop one queued write and delete its staged file. Names an id for the
    /// same reason [`Request::ConflictResolve`] does: it has to have come from
    /// a [`Request::OutboxList`] the user actually looked at.
    OutboxDiscard { id: i64 },
}

/// Which version of a conflicted file the user wants to keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Keep {
    /// The version that was edited on this computer.
    Local,
    /// The version that came down from OneDrive.
    Remote,
    /// Keep every file exactly as it is and stop alerting about it. For the
    /// case this whole design is built around — a `.kdbx` the user merges by
    /// hand — this is the normal answer.
    Both,
}

impl Keep {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Remote => "remote",
            Self::Both => "both",
        }
    }
}

/// One open conflict, as the CLI and TUI see it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictInfo {
    pub id: i64,
    /// The pin it belongs to, or `-` for an outbox conflict.
    pub subject: String,
    pub remote: String,
    /// The canonical file, relative to `folder`.
    pub file: String,
    /// Absolute path of the directory holding every version.
    pub folder: String,
    /// The extra versions kept beside the canonical file, newest last.
    pub copies: Vec<String>,
    pub detected_at: DateTime<Utc>,
}

/// One queued offline write, as the CLI and TUI see it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboxInfo {
    pub id: i64,
    pub remote: String,
    pub path: String,
    pub staged_path: String,
    pub queued_at: DateTime<Utc>,
    /// `"queued"` or `"failed"`.
    pub state: String,
    pub attempts: u32,
    pub last_err: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Response {
    Pong {
        version: String,
        protocol: u32,
    },
    Status(Box<StatusReport>),
    // A struct variant, not `Conflicts(Vec<ConflictInfo>)`: internally-tagged
    // enums serialize a newtype's content merged with the tag, which only
    // works when the content is itself a JSON object. A bare array cannot
    // carry the `"result"` field, so this failed at the first real conflict
    // list — `Status` only ever looked safe because `StatusReport` is a
    // struct.
    Conflicts {
        conflicts: Vec<ConflictInfo>,
    },
    Outbox {
        rows: Vec<OutboxInfo>,
    },
    Ok {
        msg: String,
    },
    Err {
        code: Code,
        msg: String,
        hint: Option<String>,
    },
}

impl Response {
    pub fn err(code: Code, subject: &str, msg: impl Into<String>) -> Self {
        Self::Err {
            code,
            msg: msg.into(),
            hint: code.hint(subject),
        }
    }
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

/// Overall health, which maps directly onto the process exit code so `status`
/// can drive a shell prompt or a monitoring check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Health {
    /// Everything is as it should be.
    Healthy,
    /// Working, but something needs a human eventually — notably an open
    /// conflict, a failing pin, or being offline.
    Degraded,
    /// Not doing its job: no daemon, a stale heartbeat, or a bad config.
    Broken,
}

impl Health {
    pub const fn exit_code(self) -> i32 {
        match self {
            Self::Healthy => 0,
            Self::Degraded => 1,
            Self::Broken => 2,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Degraded => "degraded",
            Self::Broken => "broken",
        }
    }

    /// Worst of the two.
    pub fn worst(self, other: Self) -> Self {
        self.max(other)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkState {
    Online,
    Offline,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MountState {
    #[default]
    Unmounted,
    Mounting,
    Mounted,
    Failed,
}

/// Where a report came from. A report read out of the database because the
/// socket did not answer is still useful, but must never be presented as live.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportSource {
    /// Answered by a running daemon.
    Daemon,
    /// Reconstructed from the state database; the daemon is not answering.
    Database,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteStatus {
    pub name: String,
    pub mount_point: String,
    pub state: MountState,
    pub since: Option<DateTime<Utc>>,
    pub error: Option<String>,
    /// Whether odfs is in front of the rclone mount for this remote.
    pub overlay: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinStatus {
    pub name: String,
    pub remote: String,
    pub local_path: String,
    pub state: String,
    pub last_ok: Option<DateTime<Utc>>,
    pub last_err: Option<String>,
    pub last_err_code: Option<Code>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct IndexStatus {
    pub files: u64,
    pub dirs: u64,
    pub last_crawl: Option<DateTime<Utc>>,
    /// True once `last_crawl` is older than the configured crawl interval, or
    /// if there has never been one.
    pub stale: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct OutboxStatus {
    pub pending: u64,
    pub oldest: Option<DateTime<Utc>>,
    pub stale: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogStatus {
    pub dir: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusReport {
    pub protocol: u32,
    pub source: ReportSource,
    pub version: String,
    pub pid: u32,
    pub started_at: DateTime<Utc>,
    pub heartbeat_at: DateTime<Utc>,
    /// Seconds since the heartbeat, as seen by whoever built the report.
    pub heartbeat_age_secs: i64,
    pub daemon_state: String,
    pub config_error: Option<String>,
    pub network: NetworkState,
    pub remotes: Vec<RemoteStatus>,
    pub pins: Vec<PinStatus>,
    pub index: IndexStatus,
    pub conflicts: u64,
    pub outbox: OutboxStatus,
    pub log: LogStatus,
}

impl StatusReport {
    /// How stale a heartbeat may be before we call the daemon dead. The daemon
    /// beats every 30s, so a minute of silence is unambiguous.
    pub const HEARTBEAT_TOLERANCE_SECS: i64 = 60;

    /// Roll everything up into one verdict. The reasons are returned alongside
    /// so the CLI can explain itself rather than just printing a colour.
    pub fn health(&self) -> (Health, Vec<String>) {
        let mut health = Health::Healthy;
        let mut why = Vec::new();

        let degrade = |h: Health, reason: String, health: &mut Health, why: &mut Vec<String>| {
            *health = health.worst(h);
            why.push(reason);
        };

        if self.protocol != PROTOCOL_VERSION {
            degrade(
                Health::Broken,
                format!(
                    "daemon speaks protocol {} but this CLI speaks {PROTOCOL_VERSION} — reinstall so both match",
                    self.protocol
                ),
                &mut health,
                &mut why,
            );
        }

        if self.source == ReportSource::Database {
            degrade(
                Health::Broken,
                "the daemon is not answering on its socket".into(),
                &mut health,
                &mut why,
            );
        }

        if self.heartbeat_age_secs > Self::HEARTBEAT_TOLERANCE_SECS {
            degrade(
                Health::Broken,
                format!(
                    "heartbeat is {}s old — the daemon is wedged or gone",
                    self.heartbeat_age_secs
                ),
                &mut health,
                &mut why,
            );
        }

        if let Some(err) = &self.config_error {
            degrade(
                Health::Broken,
                format!("config is invalid: {err}"),
                &mut health,
                &mut why,
            );
        }

        for remote in &self.remotes {
            match remote.state {
                // Named by remote rather than by path: the mount point is
                // already on screen next to this, and the absolute path is long
                // enough to bury the actual reason.
                MountState::Failed => degrade(
                    Health::Broken,
                    format!(
                        "mount for {} failed: {}",
                        remote.name,
                        remote.error.as_deref().unwrap_or("unknown error")
                    ),
                    &mut health,
                    &mut why,
                ),
                MountState::Unmounted => degrade(
                    Health::Degraded,
                    format!("{} is not mounted", remote.name),
                    &mut health,
                    &mut why,
                ),
                // Coming up is not the same as up: during the ~15s a OneDrive
                // mount takes to become readable, nothing under the mount point
                // works. Reporting that as healthy would make `status` exit 0
                // while the files are unreachable.
                MountState::Mounting => degrade(
                    Health::Degraded,
                    format!("{} is still mounting", remote.name),
                    &mut health,
                    &mut why,
                ),
                MountState::Mounted => {}
            }
        }

        if self.network == NetworkState::Offline {
            degrade(Health::Degraded, "offline".into(), &mut health, &mut why);
        }

        if self.conflicts > 0 {
            degrade(
                Health::Degraded,
                format!(
                    "{} unresolved conflict{} — see: onedrive-rc conflicts",
                    self.conflicts,
                    if self.conflicts == 1 { "" } else { "s" }
                ),
                &mut health,
                &mut why,
            );
        }

        for pin in &self.pins {
            if pin.state == "failed" || pin.state == "conflicted" {
                degrade(
                    Health::Degraded,
                    format!(
                        "pin {} is {}{}",
                        pin.name,
                        pin.state,
                        pin.last_err
                            .as_deref()
                            .map(|e| format!(": {e}"))
                            .unwrap_or_default()
                    ),
                    &mut health,
                    &mut why,
                );
            }
        }

        if self.outbox.stale {
            degrade(
                Health::Degraded,
                format!(
                    "{} offline write(s) still waiting to upload",
                    self.outbox.pending
                ),
                &mut health,
                &mut why,
            );
        }

        if self.index.stale {
            degrade(
                Health::Degraded,
                "the offline index is out of date".into(),
                &mut health,
                &mut why,
            );
        }

        (health, why)
    }
}

// ---------------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------------

/// Serialise one frame, including the trailing newline.
pub fn encode_frame<T: Serialize>(value: &T) -> Result<Vec<u8>, serde_json::Error> {
    let mut buf = serde_json::to_vec(value)?;
    buf.push(b'\n');
    Ok(buf)
}

/// Parse one frame.
pub fn decode_frame<T: for<'de> Deserialize<'de>>(line: &str) -> Result<T, serde_json::Error> {
    serde_json::from_str(line)
}

// ---------------------------------------------------------------------------
// Blocking client
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum ClientError {
    /// The socket is absent or refusing connections — almost always "daemon not
    /// running", which callers handle by falling back to the database.
    NotRunning(std::io::Error),
    Io(std::io::Error),
    Protocol(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotRunning(_) => f.write_str("onedrive-rcd is not running"),
            Self::Io(e) => write!(f, "talking to onedrive-rcd: {e}"),
            Self::Protocol(m) => write!(f, "unexpected reply from onedrive-rcd: {m}"),
        }
    }
}

impl std::error::Error for ClientError {}

/// Classify an I/O failure as "the daemon is not there" versus a real problem.
///
/// A daemon that dies mid-request produces `ECONNRESET`/`EPIPE`, which means
/// exactly the same thing to the user as a missing socket — and it must map to
/// [`ClientError::NotRunning`] so the CLI falls back to reading the database
/// instead of printing a bare errno.
fn classify(e: std::io::Error) -> ClientError {
    use std::io::ErrorKind::*;
    match e.kind() {
        NotFound | ConnectionRefused | AddrNotAvailable | ConnectionReset | ConnectionAborted
        | BrokenPipe | UnexpectedEof => ClientError::NotRunning(e),
        _ => ClientError::Io(e),
    }
}

/// Synchronous client, used by `onedrive-rc`. Blocking on purpose: the CLI has
/// one thing to ask and no reason to pull in an async runtime.
pub fn request(socket: &Path, req: &Request, timeout: Duration) -> Result<Response, ClientError> {
    let mut stream = UnixStream::connect(socket).map_err(classify)?;

    stream
        .set_read_timeout(Some(timeout))
        .map_err(ClientError::Io)?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(ClientError::Io)?;

    let frame = encode_frame(req).map_err(|e| ClientError::Protocol(e.to_string()))?;
    stream.write_all(&frame).map_err(classify)?;
    stream.flush().map_err(classify)?;

    let mut line = String::new();
    let mut reader = BufReader::new(stream).take(MAX_FRAME_BYTES as u64);
    reader.read_line(&mut line).map_err(classify)?;

    // A clean EOF with no bytes is the other shape of "it went away".
    if line.trim().is_empty() {
        return Err(ClientError::NotRunning(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "daemon closed the connection without replying",
        )));
    }
    decode_frame(line.trim()).map_err(|e| ClientError::Protocol(format!("{e}: {line}")))
}

// `Read::take` needs the trait in scope for the BufReader chain above.
use std::io::Read;

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.timestamp_opt(1_760_000_000, 0).unwrap()
    }

    fn healthy_report() -> StatusReport {
        StatusReport {
            protocol: PROTOCOL_VERSION,
            source: ReportSource::Daemon,
            version: env!("CARGO_PKG_VERSION").into(),
            pid: 1234,
            started_at: now(),
            heartbeat_at: now(),
            heartbeat_age_secs: 3,
            daemon_state: "online".into(),
            config_error: None,
            network: NetworkState::Online,
            remotes: vec![RemoteStatus {
                name: "od".into(),
                mount_point: "/home/t/OneDrive".into(),
                state: MountState::Mounted,
                since: Some(now()),
                error: None,
                overlay: true,
            }],
            pins: vec![PinStatus {
                name: "Keepass".into(),
                remote: "od".into(),
                local_path: "/home/t/onedrive-keepass".into(),
                state: "in-sync".into(),
                last_ok: Some(now()),
                last_err: None,
                last_err_code: None,
            }],
            index: IndexStatus {
                files: 412_330,
                dirs: 9_001,
                last_crawl: Some(now()),
                stale: false,
            },
            conflicts: 0,
            outbox: OutboxStatus::default(),
            log: LogStatus {
                dir: "/home/t/.local/state/onedrive-rc/log".into(),
                bytes: 12 << 20,
            },
        }
    }

    #[test]
    fn a_healthy_report_is_healthy_and_exits_zero() {
        let (health, why) = healthy_report().health();
        assert_eq!(health, Health::Healthy);
        assert!(why.is_empty(), "{why:?}");
        assert_eq!(health.exit_code(), 0);
    }

    #[test]
    fn an_open_conflict_degrades_but_does_not_break() {
        let mut r = healthy_report();
        r.conflicts = 2;
        let (health, why) = r.health();
        assert_eq!(health, Health::Degraded);
        assert_eq!(health.exit_code(), 1);
        assert!(
            why.iter().any(|w| w.contains("2 unresolved conflicts")),
            "{why:?}"
        );
    }

    #[test]
    fn being_offline_is_degraded_not_broken() {
        let mut r = healthy_report();
        r.network = NetworkState::Offline;
        let (health, why) = r.health();
        assert_eq!(health, Health::Degraded);
        assert!(why.iter().any(|w| w == "offline"), "{why:?}");
    }

    #[test]
    fn a_stale_heartbeat_is_broken() {
        let mut r = healthy_report();
        r.heartbeat_age_secs = 600;
        let (health, why) = r.health();
        assert_eq!(health, Health::Broken);
        assert_eq!(health.exit_code(), 2);
        assert!(why.iter().any(|w| w.contains("wedged")), "{why:?}");
    }

    #[test]
    fn a_database_sourced_report_is_never_presented_as_healthy() {
        let mut r = healthy_report();
        r.source = ReportSource::Database;
        let (health, why) = r.health();
        assert_eq!(health, Health::Broken);
        assert!(why.iter().any(|w| w.contains("not answering")), "{why:?}");
    }

    #[test]
    fn a_protocol_mismatch_is_reported_plainly() {
        let mut r = healthy_report();
        r.protocol = PROTOCOL_VERSION + 7;
        let (health, why) = r.health();
        assert_eq!(health, Health::Broken);
        assert!(why.iter().any(|w| w.contains("reinstall")), "{why:?}");
    }

    /// A mount that is still coming up is not usable yet, so `status` must not
    /// exit 0 on it — the files under the mount point are unreachable for the
    /// ~15s a cold OneDrive mount takes.
    #[test]
    fn a_mount_that_is_still_coming_up_is_degraded() {
        let mut r = healthy_report();
        r.remotes[0].state = MountState::Mounting;
        r.remotes[0].since = None;

        let (health, why) = r.health();
        assert_eq!(health, Health::Degraded);
        assert!(why.iter().any(|w| w.contains("still mounting")), "{why:?}");
    }

    #[test]
    fn a_failed_mount_breaks_and_an_unmounted_one_degrades() {
        let mut r = healthy_report();
        r.remotes[0].state = MountState::Failed;
        r.remotes[0].error = Some("rclone exited".into());
        assert_eq!(r.health().0, Health::Broken);

        let mut r = healthy_report();
        r.remotes[0].state = MountState::Unmounted;
        assert_eq!(r.health().0, Health::Degraded);
    }

    #[test]
    fn a_failed_pin_degrades_and_names_the_error() {
        let mut r = healthy_report();
        r.pins[0].state = "failed".into();
        r.pins[0].last_err = Some("token rejected".into());
        let (health, why) = r.health();
        assert_eq!(health, Health::Degraded);
        assert!(
            why.iter()
                .any(|w| w.contains("pin Keepass is failed: token rejected")),
            "{why:?}"
        );
    }

    #[test]
    fn config_errors_break_and_many_problems_accumulate() {
        let mut r = healthy_report();
        r.config_error = Some("2 problems".into());
        r.conflicts = 1;
        r.network = NetworkState::Offline;
        let (health, why) = r.health();
        assert_eq!(health, Health::Broken, "the worst condition wins");
        assert_eq!(why.len(), 3, "every reason is reported: {why:?}");
    }

    #[test]
    fn health_ordering_takes_the_worst() {
        assert_eq!(Health::Healthy.worst(Health::Degraded), Health::Degraded);
        assert_eq!(Health::Broken.worst(Health::Degraded), Health::Broken);
        assert_eq!(Health::Healthy.worst(Health::Healthy), Health::Healthy);
    }

    #[test]
    fn requests_and_responses_round_trip_as_single_lines() {
        for req in [
            Request::Ping,
            Request::Status,
            Request::Reload,
            Request::PinSync {
                name: Some("Keepass".into()),
            },
            Request::PinSync { name: None },
            Request::MountRemount { remote: None },
            Request::IndexRefresh { full: true },
            Request::ConflictList,
            Request::ConflictResolve {
                id: 1,
                keep: Keep::Both,
            },
            Request::OutboxList,
            Request::OutboxRetry {
                remote: Some("onedrive-timm".into()),
            },
            Request::OutboxRetry { remote: None },
            Request::OutboxDiscard { id: 7 },
        ] {
            let frame = encode_frame(&req).unwrap();
            assert_eq!(frame.iter().filter(|b| **b == b'\n').count(), 1);
            assert!(frame.ends_with(b"\n"));
            let text = String::from_utf8(frame).unwrap();
            assert_eq!(decode_frame::<Request>(text.trim()).unwrap(), req);
        }
    }

    #[test]
    fn status_response_round_trips() {
        let resp = Response::Status(Box::new(healthy_report()));
        let frame = encode_frame(&resp).unwrap();
        let text = String::from_utf8(frame).unwrap();
        assert_eq!(decode_frame::<Response>(text.trim()).unwrap(), resp);
    }

    /// Caught live: an internally-tagged enum cannot serialize a newtype
    /// variant whose content is a bare sequence, because there is nowhere to
    /// put the `"result"` tag alongside a JSON array. `Status` only looked
    /// safe because its content happens to be a struct. An empty list is the
    /// sharper regression case — `vec![]` has no fields to catch a wrong
    /// shape by accident.
    #[test]
    fn a_conflict_list_round_trips_empty_and_populated() {
        for conflicts in [
            vec![],
            vec![ConflictInfo {
                id: 3,
                subject: "Keepass".into(),
                remote: "od".into(),
                file: "Timm.kdbx".into(),
                folder: "/home/t/onedrive-keepass".into(),
                copies: vec!["Timm.conflict-remote-2026-10-08T124530Z.kdbx".into()],
                detected_at: now(),
            }],
        ] {
            let resp = Response::Conflicts { conflicts };
            let frame = encode_frame(&resp).unwrap();
            let text = String::from_utf8(frame).unwrap();
            assert_eq!(decode_frame::<Response>(text.trim()).unwrap(), resp);
        }
    }

    #[test]
    fn an_outbox_list_round_trips_empty_and_populated() {
        for rows in [
            vec![],
            vec![OutboxInfo {
                id: 5,
                remote: "od".into(),
                path: "notes.txt".into(),
                staged_path: "/home/t/OneDrive.pending/notes.txt".into(),
                queued_at: now(),
                state: "queued".into(),
                attempts: 0,
                last_err: None,
            }],
        ] {
            let resp = Response::Outbox { rows };
            let frame = encode_frame(&resp).unwrap();
            let text = String::from_utf8(frame).unwrap();
            assert_eq!(decode_frame::<Response>(text.trim()).unwrap(), resp);
        }
    }

    #[test]
    fn error_responses_carry_the_hint() {
        let resp = Response::err(Code::AuthExpired, "onedrive-timm", "token rejected");
        match resp {
            Response::Err { code, hint, .. } => {
                assert_eq!(code, Code::AuthExpired);
                assert_eq!(hint.unwrap(), "onedrive-rc setup --reauth onedrive-timm");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn requests_are_tagged_so_the_wire_format_is_self_describing() {
        let text = String::from_utf8(encode_frame(&Request::Status).unwrap()).unwrap();
        assert_eq!(text.trim(), r#"{"op":"status"}"#);
    }

    #[test]
    fn a_missing_socket_reports_not_running_rather_than_an_io_error() {
        let err = request(
            Path::new("/nonexistent/onedrive-rcd.sock"),
            &Request::Ping,
            Duration::from_millis(100),
        )
        .unwrap_err();
        assert!(matches!(err, ClientError::NotRunning(_)), "{err:?}");
        assert_eq!(err.to_string(), "onedrive-rcd is not running");
    }

    #[test]
    fn client_talks_to_a_real_socket() {
        use std::os::unix::net::UnixListener;

        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("test.sock");
        let listener = UnixListener::bind(&socket).unwrap();

        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(&stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert_eq!(decode_frame::<Request>(line.trim()).unwrap(), Request::Ping);

            let resp = Response::Pong {
                version: env!("CARGO_PKG_VERSION").into(),
                protocol: PROTOCOL_VERSION,
            };
            let mut stream = &stream;
            stream.write_all(&encode_frame(&resp).unwrap()).unwrap();
        });

        let resp = request(&socket, &Request::Ping, Duration::from_secs(5)).unwrap();
        assert_eq!(
            resp,
            Response::Pong {
                version: env!("CARGO_PKG_VERSION").into(),
                protocol: PROTOCOL_VERSION
            }
        );
        server.join().unwrap();
    }

    /// A daemon that accepts and then dies must read as "not running", because
    /// that is what makes the CLI fall back to the state database instead of
    /// printing a raw errno at the user.
    #[test]
    fn a_daemon_that_dies_mid_request_reads_as_not_running() {
        use std::os::unix::net::UnixListener;

        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("mute.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let _ = listener.accept();
            // Accept, then drop without replying.
        });

        let err = request(&socket, &Request::Ping, Duration::from_secs(2)).unwrap_err();
        assert!(matches!(err, ClientError::NotRunning(_)), "{err:?}");
        assert_eq!(err.to_string(), "onedrive-rcd is not running");
        server.join().unwrap();
    }

    #[test]
    fn garbage_from_the_socket_is_a_protocol_error() {
        use std::os::unix::net::UnixListener;

        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("garbage.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(&stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let mut stream = &stream;
            stream.write_all(b"not json at all\n").unwrap();
        });

        let err = request(&socket, &Request::Ping, Duration::from_secs(5)).unwrap_err();
        assert!(matches!(err, ClientError::Protocol(_)), "{err:?}");
        server.join().unwrap();
    }
}
