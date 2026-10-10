//! The state database.
//!
//! Replaces three separate files from the old design: `status.toml`, the
//! per-rule `baseline/*.toml` files, and `conflict-resolutions.toml`. Having one
//! transactional store matters for more than tidiness — the baseline files were
//! loaded with `unwrap_or_default()`, so a parse error silently became "no
//! baseline", which was itself one of the paths that led to data loss.
//!
//! The schema is created in full here even though phases P3/P4 are what populate
//! `node` and `outbox`; defining it up front keeps migrations from churning.

use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

use crate::code::Code;

/// Bumped only for incompatible changes; `PRAGMA user_version` tracks what a
/// given database has actually had applied.
pub const SCHEMA_VERSION: u32 = 1;

const V1: &str = r#"
-- The offline index: what exists on the remote, so listings work with no network.
-- Keyed by (remote, path) rather than path alone: two remotes can share a
-- relative path, and a bare `path` primary key would let one remote's crawl
-- silently clobber another's entries.
CREATE TABLE node (
  remote    TEXT    NOT NULL,
  path      TEXT    NOT NULL,          -- relative to the remote root; '' is the root itself
  path_ci   TEXT    NOT NULL,
  parent    TEXT    NOT NULL,          -- relative parent path; '' for the root's children
  name      TEXT    NOT NULL,
  is_dir    INTEGER NOT NULL,
  size      INTEGER NOT NULL,
  mtime     INTEGER NOT NULL,          -- unix seconds; OneDrive's precision is 1s
  quickxor  TEXT,
  -- The index generation (see `meta.index_generation`) this row was last
  -- confirmed to exist in — by a full crawl, or by an opportunistic listing
  -- in between. A full crawl prunes every row for its remote whose seen_gen
  -- is older than the generation it just finished, which is how a remote
  -- deletion reaches the offline index without an explicit delete path.
  seen_gen  INTEGER NOT NULL,
  PRIMARY KEY (remote, path)
);
CREATE INDEX node_parent ON node(remote, parent);
CREATE INDEX node_ci     ON node(remote, path_ci);

-- Read tracking, so "recently used" can be reported and reasoned about.
CREATE TABLE access (
  remote    TEXT    NOT NULL,
  path      TEXT    NOT NULL,
  last_read INTEGER NOT NULL,
  reads     INTEGER NOT NULL DEFAULT 1,
  PRIMARY KEY (remote, path)
);

-- Live state of each configured pin.
CREATE TABLE pin (
  name          TEXT PRIMARY KEY,
  remote        TEXT NOT NULL,
  remote_path   TEXT NOT NULL,
  local_path    TEXT NOT NULL,
  state         TEXT NOT NULL,
  last_ok       INTEGER,
  last_attempt  INTEGER,
  last_err      TEXT,
  last_err_code TEXT
);

-- One row per unresolved divergence. `notif_id` is the desktop notification we
-- re-assert while this row is open, which is how the alert persists.
CREATE TABLE conflict (
  id          INTEGER PRIMARY KEY,
  kind        TEXT    NOT NULL,        -- 'pin' | 'outbox'
  remote      TEXT    NOT NULL,
  subject     TEXT    NOT NULL,        -- pin name, or '-' for outbox conflicts
  path        TEXT    NOT NULL,
  winner      TEXT,
  loser_copy  TEXT,
  version_dir TEXT,
  detected_at INTEGER NOT NULL,
  resolved_at INTEGER,
  notif_id    INTEGER
);
CREATE INDEX conflict_open ON conflict(resolved_at);

-- Writes made while offline, awaiting a baseline-verified replay.
CREATE TABLE outbox (
  id            INTEGER PRIMARY KEY,
  remote        TEXT    NOT NULL,
  path          TEXT    NOT NULL,
  staged_path   TEXT    NOT NULL,
  base_size     INTEGER,
  base_quickxor TEXT,
  base_mtime    INTEGER,
  queued_at     INTEGER NOT NULL,
  state         TEXT    NOT NULL,      -- 'queued' | 'uploading' | 'failed'
  attempts      INTEGER NOT NULL DEFAULT 0,
  last_err      TEXT
);
CREATE INDEX outbox_state ON outbox(state);

-- Recent activity, for the TUI and `onedrive-tui status`. Pruned by row count.
CREATE TABLE event (
  id      INTEGER PRIMARY KEY,
  ts      INTEGER NOT NULL,
  level   TEXT    NOT NULL,
  code    TEXT,
  subject TEXT    NOT NULL,
  msg     TEXT    NOT NULL
);
CREATE INDEX event_ts ON event(ts);

-- Single-row liveness record. A heartbeat column is the thing the old
-- change-driven status.toml could not provide: it makes "healthy and idle"
-- distinguishable from "wedged".
CREATE TABLE daemon (
  id           INTEGER PRIMARY KEY CHECK (id = 1),
  pid          INTEGER NOT NULL,
  version      TEXT    NOT NULL,
  started_at   INTEGER NOT NULL,
  heartbeat_at INTEGER NOT NULL,
  state        TEXT    NOT NULL,
  config_error TEXT
);

-- Loose key/value for things like the crawl generation counter.
CREATE TABLE meta (
  k TEXT PRIMARY KEY,
  v TEXT NOT NULL
);
"#;

const MIGRATIONS: &[&str] = &[V1];

// ---------------------------------------------------------------------------
// Value types
// ---------------------------------------------------------------------------

/// What the daemon is currently doing, as surfaced to the CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DaemonState {
    #[default]
    Starting,
    Online,
    Offline,
    Suspending,
    Degraded,
    Stopping,
}

impl DaemonState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Online => "online",
            Self::Offline => "offline",
            Self::Suspending => "suspending",
            Self::Degraded => "degraded",
            Self::Stopping => "stopping",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "online" => Self::Online,
            "offline" => Self::Offline,
            "suspending" => Self::Suspending,
            "degraded" => Self::Degraded,
            "stopping" => Self::Stopping,
            _ => Self::Starting,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonRecord {
    pub pid: u32,
    pub version: String,
    pub started_at: DateTime<Utc>,
    pub heartbeat_at: DateTime<Utc>,
    pub state: DaemonState,
    pub config_error: Option<String>,
}

impl DaemonRecord {
    /// Whether the heartbeat is recent enough to believe the daemon is alive.
    ///
    /// Used by the CLI when it cannot reach the socket: a stale heartbeat means
    /// the daemon died without cleaning up, which the old design could not tell
    /// apart from a healthy idle daemon.
    pub fn is_fresh(&self, now: DateTime<Utc>, tolerance_secs: i64) -> bool {
        (now - self.heartbeat_at).num_seconds() <= tolerance_secs
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PinState {
    #[default]
    Unknown,
    Syncing,
    InSync,
    Conflicted,
    Failed,
    Deferred,
}

impl PinState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Syncing => "syncing",
            Self::InSync => "in-sync",
            Self::Conflicted => "conflicted",
            Self::Failed => "failed",
            Self::Deferred => "deferred",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "syncing" => Self::Syncing,
            "in-sync" => Self::InSync,
            "conflicted" => Self::Conflicted,
            "failed" => Self::Failed,
            "deferred" => Self::Deferred,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinRecord {
    pub name: String,
    pub remote: String,
    pub remote_path: String,
    pub local_path: String,
    pub state: PinState,
    pub last_ok: Option<DateTime<Utc>>,
    pub last_attempt: Option<DateTime<Utc>>,
    pub last_err: Option<String>,
    pub last_err_code: Option<Code>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictRecord {
    pub id: i64,
    pub kind: String,
    pub remote: String,
    pub subject: String,
    pub path: String,
    pub winner: Option<String>,
    pub loser_copy: Option<String>,
    pub version_dir: Option<String>,
    pub detected_at: DateTime<Utc>,
    pub notif_id: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub id: i64,
    pub ts: DateTime<Utc>,
    pub level: String,
    pub code: Option<Code>,
    pub subject: String,
    pub msg: String,
}

/// One write made while offline, staged on disk and awaiting a
/// baseline-verified replay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxRecord {
    pub id: i64,
    pub remote: String,
    pub path: String,
    pub staged_path: String,
    /// What the remote looked like when the edit started — `None` for a
    /// brand-new file the index had never seen. Replay re-checks the remote
    /// against this before uploading; a mismatch means someone else changed
    /// it too, and the upload becomes a conflict copy instead of overwriting.
    pub base_size: Option<u64>,
    pub base_quickxor: Option<String>,
    pub base_mtime: Option<DateTime<Utc>>,
    pub queued_at: DateTime<Utc>,
    /// `'queued'` or `'failed'` — there is no `'uploading'` in practice: replay
    /// runs one file at a time, so nothing is ever caught mid-transition.
    pub state: String,
    pub attempts: u32,
    pub last_err: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IndexStats {
    pub files: u64,
    pub dirs: u64,
    pub last_crawl: Option<DateTime<Utc>>,
}

/// One file or directory as recorded in the offline index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRecord {
    /// Relative to the remote root; `""` is the root itself.
    pub path: String,
    /// Relative parent path; `""` for the root's children.
    pub parent: String,
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: DateTime<Utc>,
    pub quickxor: Option<String>,
}

/// What the crawler and odfs's opportunistic updates feed into the index.
///
/// Carries no `remote`: every caller already has one remote in scope for a
/// whole batch, and `parent`/`name`/`path_ci` are derived from `path` rather
/// than trusted from the caller, so there is exactly one place that can get
/// the split wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeInput {
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: DateTime<Utc>,
    pub quickxor: Option<String>,
}

/// Split `"a/b/c"` into parent `"a/b"` and name `"c"`; a path with no `/` is a
/// direct child of the root, whose own parent is the empty string.
fn split_path(path: &str) -> (&str, &str) {
    match path.rsplit_once('/') {
        Some((parent, name)) => (parent, name),
        None => ("", path),
    }
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

pub struct Store {
    conn: Connection,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store").finish_non_exhaustive()
    }
}

impl Store {
    /// Open read-write, creating and migrating as needed.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;

        // WAL so the CLI can read while the daemon writes; NORMAL sync is the
        // right trade for state we can always rebuild from the remote.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;

        let mut store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    /// In-memory database, for tests.
    pub fn open_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let mut store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    /// Open read-only. This is the CLI's fallback when the daemon is not
    /// answering on its socket, so it can still report *that* the daemon is down
    /// and when it last ran.
    pub fn open_readonly(path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("opening {} read-only", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(2))?;
        Ok(Self { conn })
    }

    fn migrate(&mut self) -> Result<()> {
        let current: u32 = self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))?;

        if current as usize > MIGRATIONS.len() {
            anyhow::bail!(
                "state database is at schema version {current}, but this build only knows {}. \
                 Upgrade onedrived, or move {} aside to start fresh.",
                MIGRATIONS.len(),
                "state.db"
            );
        }

        for (i, sql) in MIGRATIONS.iter().enumerate().skip(current as usize) {
            let version = i as u32 + 1;
            let tx = self.conn.transaction()?;
            tx.execute_batch(sql)
                .with_context(|| format!("applying schema migration {version}"))?;
            tx.pragma_update(None, "user_version", version)?;
            tx.commit()?;
        }
        Ok(())
    }

    pub fn schema_version(&self) -> Result<u32> {
        Ok(self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))?)
    }

    // -- meta --------------------------------------------------------------

    pub fn meta_get(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT v FROM meta WHERE k = ?1", params![key], |r| {
                r.get(0)
            })
            .optional()?)
    }

    pub fn meta_set(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta (k, v) VALUES (?1, ?2)
             ON CONFLICT(k) DO UPDATE SET v = excluded.v",
            params![key, value],
        )?;
        Ok(())
    }

    // -- daemon liveness ---------------------------------------------------

    /// Record that this process owns the daemon role. Resets the heartbeat.
    pub fn daemon_started(&self, pid: u32, version: &str, now: DateTime<Utc>) -> Result<()> {
        self.conn.execute(
            "INSERT INTO daemon (id, pid, version, started_at, heartbeat_at, state, config_error)
             VALUES (1, ?1, ?2, ?3, ?3, ?4, NULL)
             ON CONFLICT(id) DO UPDATE SET
               pid = excluded.pid, version = excluded.version,
               started_at = excluded.started_at, heartbeat_at = excluded.heartbeat_at,
               state = excluded.state, config_error = NULL",
            params![
                pid,
                version,
                now.timestamp(),
                DaemonState::Starting.as_str()
            ],
        )?;
        Ok(())
    }

    /// Advance the heartbeat. Called on a timer; this is what systemd's watchdog
    /// and the CLI's freshness check both key off.
    pub fn heartbeat(&self, now: DateTime<Utc>) -> Result<()> {
        self.conn.execute(
            "UPDATE daemon SET heartbeat_at = ?1 WHERE id = 1",
            params![now.timestamp()],
        )?;
        Ok(())
    }

    pub fn set_daemon_state(&self, state: DaemonState) -> Result<()> {
        self.conn.execute(
            "UPDATE daemon SET state = ?1 WHERE id = 1",
            params![state.as_str()],
        )?;
        Ok(())
    }

    pub fn set_config_error(&self, error: Option<&str>) -> Result<()> {
        self.conn.execute(
            "UPDATE daemon SET config_error = ?1 WHERE id = 1",
            params![error],
        )?;
        Ok(())
    }

    pub fn daemon(&self) -> Result<Option<DaemonRecord>> {
        Ok(self
            .conn
            .query_row(
                "SELECT pid, version, started_at, heartbeat_at, state, config_error
                 FROM daemon WHERE id = 1",
                [],
                |r| {
                    Ok(DaemonRecord {
                        pid: r.get(0)?,
                        version: r.get(1)?,
                        started_at: ts(r.get(2)?),
                        heartbeat_at: ts(r.get(3)?),
                        state: DaemonState::parse(&r.get::<_, String>(4)?),
                        config_error: r.get(5)?,
                    })
                },
            )
            .optional()?)
    }

    /// Mark a clean shutdown. Leaving the row but flipping the state means the
    /// CLI can distinguish "stopped on purpose" from "heartbeat went stale".
    pub fn daemon_stopped(&self) -> Result<()> {
        self.set_daemon_state(DaemonState::Stopping)
    }

    // -- events ------------------------------------------------------------

    pub fn log_event(
        &self,
        now: DateTime<Utc>,
        level: &str,
        code: Option<Code>,
        subject: &str,
        msg: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO event (ts, level, code, subject, msg) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                now.timestamp(),
                level,
                code.map(|c| c.as_str()),
                subject,
                msg
            ],
        )?;
        Ok(())
    }

    pub fn recent_events(&self, limit: u32) -> Result<Vec<Event>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, ts, level, code, subject, msg FROM event ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit], |r| {
                Ok(Event {
                    id: r.get(0)?,
                    ts: ts(r.get(1)?),
                    level: r.get(2)?,
                    code: r.get::<_, Option<String>>(3)?.and_then(|c| Code::parse(&c)),
                    subject: r.get(4)?,
                    msg: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Keep the newest `keep` events. Called alongside log pruning.
    pub fn prune_events(&self, keep: u32) -> Result<usize> {
        Ok(self.conn.execute(
            "DELETE FROM event WHERE id NOT IN
               (SELECT id FROM event ORDER BY id DESC LIMIT ?1)",
            params![keep],
        )?)
    }

    // -- pins --------------------------------------------------------------

    /// Insert or refresh a pin's identity, preserving its live state columns.
    pub fn register_pin(
        &self,
        name: &str,
        remote: &str,
        remote_path: &str,
        local_path: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO pin (name, remote, remote_path, local_path, state)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(name) DO UPDATE SET
               remote = excluded.remote,
               remote_path = excluded.remote_path,
               local_path = excluded.local_path",
            params![
                name,
                remote,
                remote_path,
                local_path,
                PinState::Unknown.as_str()
            ],
        )?;
        Ok(())
    }

    /// Drop pins that are no longer in the config.
    pub fn retain_pins(&self, keep: &[String]) -> Result<usize> {
        if keep.is_empty() {
            return Ok(self.conn.execute("DELETE FROM pin", [])?);
        }
        let placeholders = vec!["?"; keep.len()].join(",");
        let sql = format!("DELETE FROM pin WHERE name NOT IN ({placeholders})");
        let params: Vec<&dyn rusqlite::ToSql> =
            keep.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
        Ok(self.conn.execute(&sql, params.as_slice())?)
    }

    pub fn set_pin_state(
        &self,
        name: &str,
        state: PinState,
        now: DateTime<Utc>,
        err: Option<(Code, &str)>,
    ) -> Result<()> {
        let ok_at = matches!(state, PinState::InSync).then(|| now.timestamp());
        self.conn.execute(
            "UPDATE pin SET
               state = ?2,
               last_attempt = ?3,
               last_ok = COALESCE(?4, last_ok),
               last_err = ?5,
               last_err_code = ?6
             WHERE name = ?1",
            params![
                name,
                state.as_str(),
                now.timestamp(),
                ok_at,
                err.map(|(_, m)| m),
                err.map(|(c, _)| c.as_str()),
            ],
        )?;
        Ok(())
    }

    pub fn pins(&self) -> Result<Vec<PinRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT name, remote, remote_path, local_path, state,
                    last_ok, last_attempt, last_err, last_err_code
             FROM pin ORDER BY name",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(PinRecord {
                    name: r.get(0)?,
                    remote: r.get(1)?,
                    remote_path: r.get(2)?,
                    local_path: r.get(3)?,
                    state: PinState::parse(&r.get::<_, String>(4)?),
                    last_ok: r.get::<_, Option<i64>>(5)?.map(ts),
                    last_attempt: r.get::<_, Option<i64>>(6)?.map(ts),
                    last_err: r.get(7)?,
                    last_err_code: r.get::<_, Option<String>>(8)?.and_then(|c| Code::parse(&c)),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // -- conflicts ---------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    pub fn insert_conflict(
        &self,
        kind: &str,
        remote: &str,
        subject: &str,
        path: &str,
        winner: Option<&str>,
        loser_copy: Option<&str>,
        version_dir: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO conflict
               (kind, remote, subject, path, winner, loser_copy, version_dir, detected_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                kind,
                remote,
                subject,
                path,
                winner,
                loser_copy,
                version_dir,
                now.timestamp()
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn open_conflicts(&self) -> Result<Vec<ConflictRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, remote, subject, path, winner, loser_copy, version_dir,
                    detected_at, notif_id
             FROM conflict WHERE resolved_at IS NULL ORDER BY detected_at",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(ConflictRecord {
                    id: r.get(0)?,
                    kind: r.get(1)?,
                    remote: r.get(2)?,
                    subject: r.get(3)?,
                    path: r.get(4)?,
                    winner: r.get(5)?,
                    loser_copy: r.get(6)?,
                    version_dir: r.get(7)?,
                    detected_at: ts(r.get(8)?),
                    notif_id: r.get(9)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Idempotent: returns the existing open conflict for this path, if any, so
    /// a repeated detection does not pile up duplicate rows and notifications.
    pub fn open_conflict_for(&self, subject: &str, path: &str) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id FROM conflict
                 WHERE subject = ?1 AND path = ?2 AND resolved_at IS NULL",
                params![subject, path],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn set_conflict_notification(&self, id: i64, notif_id: Option<u32>) -> Result<()> {
        self.conn.execute(
            "UPDATE conflict SET notif_id = ?2 WHERE id = ?1",
            params![id, notif_id],
        )?;
        Ok(())
    }

    pub fn resolve_conflict(&self, id: i64, now: DateTime<Utc>) -> Result<()> {
        self.conn.execute(
            "UPDATE conflict SET resolved_at = ?2 WHERE id = ?1 AND resolved_at IS NULL",
            params![id, now.timestamp()],
        )?;
        Ok(())
    }

    /// Conflicts that are resolved but whose notification is still on screen.
    ///
    /// A row can be resolved from anywhere — the auto-detect sweep, an explicit
    /// `conflicts resolve`, a future TUI — but only the task holding the D-Bus
    /// connection can withdraw the notification. This is the handover: whoever
    /// resolves the row leaves the `notif_id` set, and that task clears it.
    pub fn conflicts_awaiting_withdrawal(&self) -> Result<Vec<(i64, String, String, u32)>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, subject, path, notif_id FROM conflict
             WHERE resolved_at IS NOT NULL AND notif_id IS NOT NULL",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// A conflict by id, whether open or resolved.
    pub fn conflict(&self, id: i64) -> Result<Option<(ConflictRecord, bool)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, kind, remote, subject, path, winner, loser_copy, version_dir,
                        detected_at, notif_id, resolved_at
                 FROM conflict WHERE id = ?1",
                params![id],
                |r| {
                    let resolved: Option<i64> = r.get(10)?;
                    Ok((
                        ConflictRecord {
                            id: r.get(0)?,
                            kind: r.get(1)?,
                            remote: r.get(2)?,
                            subject: r.get(3)?,
                            path: r.get(4)?,
                            winner: r.get(5)?,
                            loser_copy: r.get(6)?,
                            version_dir: r.get(7)?,
                            detected_at: ts(r.get(8)?),
                            notif_id: r.get(9)?,
                        },
                        resolved.is_some(),
                    ))
                },
            )
            .optional()?)
    }

    // -- outbox ------------------------------------------------------------

    pub fn outbox_len(&self) -> Result<u64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM outbox WHERE state != 'done'",
            [],
            |r| r.get(0),
        )?)
    }

    /// Timestamp of the oldest still-pending offline write, for staleness alerts.
    pub fn outbox_oldest(&self) -> Result<Option<DateTime<Utc>>> {
        Ok(self
            .conn
            .query_row(
                "SELECT MIN(queued_at) FROM outbox WHERE state != 'done'",
                [],
                |r| r.get::<_, Option<i64>>(0),
            )?
            .map(ts))
    }

    /// Stage a write for later replay, unless this path is already queued —
    /// staging again must not reset the baseline a second edit should still
    /// be checked against, so a repeat call is a no-op that returns the
    /// existing row's id.
    #[allow(clippy::too_many_arguments)]
    pub fn queue_outbox(
        &self,
        remote: &str,
        path: &str,
        staged_path: &str,
        base_size: Option<u64>,
        base_quickxor: Option<&str>,
        base_mtime: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) -> Result<i64> {
        if let Some(existing) = self.outbox_for(remote, path)? {
            return Ok(existing.id);
        }
        self.conn.execute(
            "INSERT INTO outbox
               (remote, path, staged_path, base_size, base_quickxor, base_mtime,
                queued_at, state, attempts)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'queued', 0)",
            params![
                remote,
                path,
                staged_path,
                base_size,
                base_quickxor,
                base_mtime.map(|t| t.timestamp()),
                now.timestamp(),
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// The outbox row for this path, if one is queued or failed — not yet
    /// resolved either way.
    pub fn outbox_for(&self, remote: &str, path: &str) -> Result<Option<OutboxRecord>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, remote, path, staged_path, base_size, base_quickxor,
                        base_mtime, queued_at, state, attempts, last_err
                 FROM outbox WHERE remote = ?1 AND path = ?2 AND state != 'done'",
                params![remote, path],
                outbox_from_row,
            )
            .optional()?)
    }

    pub fn outbox_record(&self, id: i64) -> Result<Option<OutboxRecord>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, remote, path, staged_path, base_size, base_quickxor,
                        base_mtime, queued_at, state, attempts, last_err
                 FROM outbox WHERE id = ?1",
                params![id],
                outbox_from_row,
            )
            .optional()?)
    }

    /// Every outbox row not yet resolved, oldest first — what
    /// `onedrive-tui outbox ls` shows and what a replay pass works through.
    pub fn open_outbox(&self) -> Result<Vec<OutboxRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, remote, path, staged_path, base_size, base_quickxor,
                    base_mtime, queued_at, state, attempts, last_err
             FROM outbox WHERE state != 'done' ORDER BY queued_at",
        )?;
        let rows = stmt
            .query_map([], outbox_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Record a failed replay attempt. The row stays queued for the next
    /// retry — a replay failure is never silently dropped.
    pub fn mark_outbox_failed(&self, id: i64, err: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE outbox SET state = 'failed', attempts = attempts + 1, last_err = ?2
             WHERE id = ?1",
            params![id, err],
        )?;
        Ok(())
    }

    /// Remove a row once it is resolved — replayed successfully, or
    /// explicitly discarded. There is no terminal `'done'` state kept around;
    /// a resolved write simply stops being in the table.
    pub fn remove_outbox(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM outbox WHERE id = ?1", params![id])?;
        Ok(())
    }

    // -- index -------------------------------------------------------------

    pub fn index_stats(&self) -> Result<IndexStats> {
        let (files, dirs): (u64, u64) = self.conn.query_row(
            "SELECT
               COALESCE(SUM(CASE WHEN is_dir = 0 THEN 1 ELSE 0 END), 0),
               COALESCE(SUM(CASE WHEN is_dir = 1 THEN 1 ELSE 0 END), 0)
             FROM node",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;

        let last_crawl = self
            .meta_get("last_full_crawl")?
            .and_then(|v| v.parse::<i64>().ok())
            .map(ts);

        Ok(IndexStats {
            files,
            dirs,
            last_crawl,
        })
    }

    /// The index generation last confirmed by a completed full crawl, or by an
    /// opportunistic update since. Starts at 0, so nothing is prunable before
    /// any crawl has ever run.
    pub fn index_generation(&self) -> Result<i64> {
        Ok(self
            .meta_get("index_generation")?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0))
    }

    pub fn set_index_generation(&self, generation: i64) -> Result<()> {
        self.meta_set("index_generation", &generation.to_string())
    }

    /// Insert or refresh a batch of nodes for one remote, in a single
    /// transaction — a crawl of thousands of entries should not be thousands
    /// of commits. `generation` marks every row as confirmed as of that generation;
    /// a full crawl passes its own new generation, an opportunistic update
    /// passes the last-known one (see [`Self::index_generation`]) since it is
    /// only re-confirming entries, not establishing a complete listing.
    pub fn upsert_nodes(&self, remote: &str, nodes: &[NodeInput], generation: i64) -> Result<()> {
        if nodes.is_empty() {
            return Ok(());
        }
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO node (remote, path, path_ci, parent, name, is_dir, size, mtime, quickxor, seen_gen)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT(remote, path) DO UPDATE SET
                   is_dir = excluded.is_dir,
                   size = excluded.size,
                   mtime = excluded.mtime,
                   -- An opportunistic update has no hash (a `stat()` cannot
                   -- produce one) and must not blank out a hash a full crawl
                   -- already recorded.
                   quickxor = COALESCE(excluded.quickxor, quickxor),
                   seen_gen = excluded.seen_gen",
            )?;
            for node in nodes {
                let (parent, name) = split_path(&node.path);
                stmt.execute(params![
                    remote,
                    node.path,
                    node.path.to_lowercase(),
                    parent,
                    name,
                    node.is_dir,
                    node.size,
                    node.mtime.timestamp(),
                    node.quickxor,
                    generation,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Remove every node for `remote` not confirmed as of `generation` — the entries
    /// a full crawl did not see, which means they no longer exist upstream.
    /// Only meant to run after a crawl completed successfully: a partial
    /// listing pruned this way would delete everything outside the part that
    /// was actually walked.
    pub fn prune_nodes_before(&self, remote: &str, generation: i64) -> Result<u64> {
        Ok(self.conn.execute(
            "DELETE FROM node WHERE remote = ?1 AND seen_gen < ?2",
            params![remote, generation],
        )? as u64)
    }

    pub fn node(&self, remote: &str, path: &str) -> Result<Option<NodeRecord>> {
        Ok(self
            .conn
            .query_row(
                "SELECT path, parent, name, is_dir, size, mtime, quickxor
                 FROM node WHERE remote = ?1 AND path = ?2",
                params![remote, path],
                node_from_row,
            )
            .optional()?)
    }

    /// Resolve a path case-insensitively, via the indexed `path_ci` column —
    /// OneDrive is itself case-insensitive, so a path typed in the wrong case
    /// still has to resolve. ASCII-folds only, same limit rclone's own
    /// `--ignore-case-sync` has.
    pub fn node_by_path_ci(&self, remote: &str, path_ci: &str) -> Result<Option<NodeRecord>> {
        Ok(self
            .conn
            .query_row(
                "SELECT path, parent, name, is_dir, size, mtime, quickxor
                 FROM node WHERE remote = ?1 AND path_ci = ?2",
                params![remote, path_ci],
                node_from_row,
            )
            .optional()?)
    }

    /// Every direct child of `parent`, ordered by name — what a `readdir`
    /// serves straight from the index with no network involved.
    pub fn children(&self, remote: &str, parent: &str) -> Result<Vec<NodeRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, parent, name, is_dir, size, mtime, quickxor
             FROM node WHERE remote = ?1 AND parent = ?2 ORDER BY name",
        )?;
        let rows = stmt
            .query_map(params![remote, parent], node_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// One child of `parent` by name, case-insensitively — what a `lookup`
    /// resolves. ASCII-folds only; see [`Self::node_by_path_ci`].
    pub fn find_child(&self, remote: &str, parent: &str, name: &str) -> Result<Option<NodeRecord>> {
        Ok(self
            .conn
            .query_row(
                "SELECT path, parent, name, is_dir, size, mtime, quickxor
                 FROM node WHERE remote = ?1 AND parent = ?2 AND lower(name) = lower(?3)",
                params![remote, parent, name],
                node_from_row,
            )
            .optional()?)
    }

    /// Paths whose relative path contains `needle`, case-insensitively,
    /// ordered by path. What `onedrive-tui find` runs against — a plain
    /// substring match, not a glob engine, which is enough to locate a file by
    /// name without pulling in a pattern-matching dependency for it.
    pub fn search(&self, remote: &str, needle: &str) -> Result<Vec<NodeRecord>> {
        let pattern = format!(
            "%{}%",
            needle
                .to_lowercase()
                .replace('%', "\\%")
                .replace('_', "\\_")
        );
        let mut stmt = self.conn.prepare(
            "SELECT path, parent, name, is_dir, size, mtime, quickxor
             FROM node WHERE remote = ?1 AND path_ci LIKE ?2 ESCAPE '\\' ORDER BY path",
        )?;
        let rows = stmt
            .query_map(params![remote, pattern], node_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Record that a path was read, for a future "recently used" view. Best
    /// effort: a failure here must not fail the read it is tracking.
    pub fn record_access(&self, remote: &str, path: &str, now: DateTime<Utc>) -> Result<()> {
        self.conn.execute(
            "INSERT INTO access (remote, path, last_read, reads) VALUES (?1, ?2, ?3, 1)
             ON CONFLICT(remote, path) DO UPDATE SET
               last_read = excluded.last_read, reads = reads + 1",
            params![remote, path, now.timestamp()],
        )?;
        Ok(())
    }
}

fn node_from_row(r: &rusqlite::Row) -> rusqlite::Result<NodeRecord> {
    Ok(NodeRecord {
        path: r.get(0)?,
        parent: r.get(1)?,
        name: r.get(2)?,
        is_dir: r.get(3)?,
        size: r.get(4)?,
        mtime: ts(r.get(5)?),
        quickxor: r.get(6)?,
    })
}

fn outbox_from_row(r: &rusqlite::Row) -> rusqlite::Result<OutboxRecord> {
    Ok(OutboxRecord {
        id: r.get(0)?,
        remote: r.get(1)?,
        path: r.get(2)?,
        staged_path: r.get(3)?,
        base_size: r.get(4)?,
        base_quickxor: r.get(5)?,
        base_mtime: r.get::<_, Option<i64>>(6)?.map(ts),
        queued_at: ts(r.get(7)?),
        state: r.get(8)?,
        attempts: r.get(9)?,
        last_err: r.get(10)?,
    })
}

/// Unix seconds to UTC. Out-of-range values clamp rather than panic; a corrupt
/// timestamp should degrade the display, not take the daemon down.
fn ts(secs: i64) -> DateTime<Utc> {
    match Utc.timestamp_opt(secs, 0) {
        chrono::LocalResult::Single(dt) => dt,
        _ => DateTime::<Utc>::from_timestamp_nanos(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        Utc.timestamp_opt(1_760_000_000, 0).unwrap()
    }

    #[test]
    fn opening_creates_the_schema_at_the_current_version() {
        let s = Store::open_memory().unwrap();
        assert_eq!(s.schema_version().unwrap(), SCHEMA_VERSION);
        // Every table the later phases rely on exists already.
        for table in [
            "node", "access", "pin", "conflict", "outbox", "event", "daemon", "meta",
        ] {
            let n: u32 = s
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    params![table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "missing table {table}");
        }
    }

    #[test]
    fn migration_is_idempotent_across_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");

        {
            let s = Store::open(&path).unwrap();
            s.meta_set("k", "v").unwrap();
        }
        {
            let s = Store::open(&path).unwrap();
            assert_eq!(s.schema_version().unwrap(), SCHEMA_VERSION);
            assert_eq!(s.meta_get("k").unwrap().as_deref(), Some("v"));
        }
    }

    #[test]
    fn a_future_schema_is_refused_rather_than_corrupted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        {
            let s = Store::open(&path).unwrap();
            s.conn.pragma_update(None, "user_version", 99u32).unwrap();
        }
        let err = Store::open(&path).unwrap_err();
        assert!(err.to_string().contains("schema version 99"), "{err}");
    }

    #[test]
    fn creates_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b/c/state.db");
        Store::open(&path).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn meta_set_overwrites() {
        let s = Store::open_memory().unwrap();
        assert_eq!(s.meta_get("missing").unwrap(), None);
        s.meta_set("k", "1").unwrap();
        s.meta_set("k", "2").unwrap();
        assert_eq!(s.meta_get("k").unwrap().as_deref(), Some("2"));
    }

    #[test]
    fn heartbeat_distinguishes_idle_from_wedged() {
        let s = Store::open_memory().unwrap();
        s.daemon_started(4242, "0.3.0", now()).unwrap();

        let d = s.daemon().unwrap().unwrap();
        assert_eq!(d.pid, 4242);
        assert_eq!(d.state, DaemonState::Starting);
        assert_eq!(d.started_at, now());
        assert!(d.config_error.is_none());

        // A heartbeat 5s old is fresh; 10 minutes old is not.
        assert!(d.is_fresh(now() + chrono::Duration::seconds(5), 60));
        assert!(!d.is_fresh(now() + chrono::Duration::seconds(600), 60));

        let later = now() + chrono::Duration::seconds(300);
        s.heartbeat(later).unwrap();
        s.set_daemon_state(DaemonState::Online).unwrap();
        let d = s.daemon().unwrap().unwrap();
        assert_eq!(d.heartbeat_at, later);
        assert_eq!(d.state, DaemonState::Online);
        // started_at must survive a heartbeat.
        assert_eq!(d.started_at, now());
        assert!(d.is_fresh(later, 60));
    }

    #[test]
    fn restart_replaces_the_daemon_row_and_clears_stale_config_errors() {
        let s = Store::open_memory().unwrap();
        s.daemon_started(1, "0.3.0", now()).unwrap();
        s.set_config_error(Some("bad")).unwrap();
        assert!(s.daemon().unwrap().unwrap().config_error.is_some());

        let restart = now() + chrono::Duration::seconds(10);
        s.daemon_started(2, "0.3.0", restart).unwrap();
        let d = s.daemon().unwrap().unwrap();
        assert_eq!(d.pid, 2);
        assert_eq!(d.started_at, restart);
        assert!(
            d.config_error.is_none(),
            "a restart must not inherit the old error"
        );
    }

    #[test]
    fn clean_shutdown_is_distinguishable_from_a_stale_heartbeat() {
        let s = Store::open_memory().unwrap();
        s.daemon_started(1, "0.3.0", now()).unwrap();
        s.daemon_stopped().unwrap();
        assert_eq!(s.daemon().unwrap().unwrap().state, DaemonState::Stopping);
    }

    #[test]
    fn events_come_back_newest_first_and_prune_to_a_cap() {
        let s = Store::open_memory().unwrap();
        for i in 0..10 {
            s.log_event(
                now() + chrono::Duration::seconds(i),
                "info",
                Some(Code::Offline),
                "pin/Keepass",
                &format!("event {i}"),
            )
            .unwrap();
        }

        let recent = s.recent_events(3).unwrap();
        assert_eq!(recent.len(), 3);
        assert_eq!(recent[0].msg, "event 9");
        assert_eq!(recent[0].code, Some(Code::Offline));
        assert_eq!(recent[0].subject, "pin/Keepass");

        assert_eq!(s.prune_events(4).unwrap(), 6);
        let left = s.recent_events(100).unwrap();
        assert_eq!(left.len(), 4);
        assert_eq!(left[3].msg, "event 6");
    }

    #[test]
    fn an_unknown_event_code_degrades_to_none_instead_of_failing() {
        let s = Store::open_memory().unwrap();
        s.conn
            .execute(
                "INSERT INTO event (ts, level, code, subject, msg)
                 VALUES (?1, 'warn', 'E_FROM_THE_FUTURE', 'x', 'm')",
                params![now().timestamp()],
            )
            .unwrap();
        assert_eq!(s.recent_events(1).unwrap()[0].code, None);
    }

    #[test]
    fn pins_register_update_and_prune() {
        let s = Store::open_memory().unwrap();
        s.register_pin(
            "Keepass",
            "od",
            "Dokumente/Keepass",
            "/home/t/onedrive-keepass",
        )
        .unwrap();
        s.register_pin("Other", "od", "Other", "/home/t/other")
            .unwrap();

        let pins = s.pins().unwrap();
        assert_eq!(pins.len(), 2);
        assert_eq!(pins[0].name, "Keepass");
        assert_eq!(pins[0].state, PinState::Unknown);

        s.set_pin_state("Keepass", PinState::InSync, now(), None)
            .unwrap();
        let p = &s.pins().unwrap()[0];
        assert_eq!(p.state, PinState::InSync);
        assert_eq!(p.last_ok, Some(now()));
        assert!(p.last_err.is_none());

        // A later failure records the error but must not erase last_ok, which is
        // what tells the user how stale their offline copy is.
        let later = now() + chrono::Duration::seconds(60);
        s.set_pin_state(
            "Keepass",
            PinState::Failed,
            later,
            Some((Code::AuthExpired, "token")),
        )
        .unwrap();
        let p = &s.pins().unwrap()[0];
        assert_eq!(p.state, PinState::Failed);
        assert_eq!(p.last_ok, Some(now()));
        assert_eq!(p.last_attempt, Some(later));
        assert_eq!(p.last_err_code, Some(Code::AuthExpired));

        // Re-registering keeps live state.
        s.register_pin("Keepass", "od", "Dokumente/Keepass", "/new/path")
            .unwrap();
        let p = &s.pins().unwrap()[0];
        assert_eq!(p.local_path, "/new/path");
        assert_eq!(p.state, PinState::Failed);

        assert_eq!(s.retain_pins(&["Keepass".to_string()]).unwrap(), 1);
        assert_eq!(s.pins().unwrap().len(), 1);
        assert_eq!(s.retain_pins(&[]).unwrap(), 1);
        assert!(s.pins().unwrap().is_empty());
    }

    #[test]
    fn conflicts_stay_open_until_resolved_and_do_not_duplicate() {
        let s = Store::open_memory().unwrap();
        let id = s
            .insert_conflict(
                "pin",
                "od",
                "Keepass",
                "Timm.kdbx",
                Some("Timm.kdbx"),
                Some("Timm.conflict-remote-20261008T1042.kdbx"),
                Some("/home/t/onedrive-keepass/.versions/2026-10-08T10-42-13Z"),
                now(),
            )
            .unwrap();

        let open = s.open_conflicts().unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].id, id);
        assert_eq!(open[0].path, "Timm.kdbx");
        assert!(open[0].loser_copy.is_some());
        assert!(open[0].notif_id.is_none());

        // The re-assert path looks the conflict up rather than inserting again.
        assert_eq!(
            s.open_conflict_for("Keepass", "Timm.kdbx").unwrap(),
            Some(id)
        );
        s.set_conflict_notification(id, Some(77)).unwrap();
        assert_eq!(s.open_conflicts().unwrap()[0].notif_id, Some(77));

        s.resolve_conflict(id, now()).unwrap();
        assert!(s.open_conflicts().unwrap().is_empty());
        assert_eq!(s.open_conflict_for("Keepass", "Timm.kdbx").unwrap(), None);
    }

    /// A conflict can be resolved from a request handler, but only the task
    /// holding the D-Bus connection can take the notification down. The
    /// lingering `notif_id` is the handover.
    #[test]
    fn a_resolved_conflict_hands_its_notification_over_to_be_withdrawn() {
        let s = Store::open_memory().unwrap();
        let id = s
            .insert_conflict(
                "pin",
                "od",
                "Keepass",
                "Timm.kdbx",
                None,
                None,
                Some("/f"),
                now(),
            )
            .unwrap();

        // Nothing to withdraw while it is open, however it is notified.
        s.set_conflict_notification(id, Some(1014)).unwrap();
        assert!(s.conflicts_awaiting_withdrawal().unwrap().is_empty());

        s.resolve_conflict(id, now()).unwrap();
        assert_eq!(
            s.conflicts_awaiting_withdrawal().unwrap(),
            vec![(id, "Keepass".to_string(), "Timm.kdbx".to_string(), 1014)]
        );

        // Clearing the id is what stops it being swept again every tick.
        s.set_conflict_notification(id, None).unwrap();
        assert!(s.conflicts_awaiting_withdrawal().unwrap().is_empty());
    }

    /// Resolving needs to tell "already dealt with" apart from "never existed",
    /// because the two deserve different answers.
    #[test]
    fn a_conflict_can_be_looked_up_by_id_open_or_resolved() {
        let s = Store::open_memory().unwrap();
        assert!(s.conflict(1).unwrap().is_none());

        let id = s
            .insert_conflict(
                "pin",
                "od",
                "Keepass",
                "Timm.kdbx",
                None,
                None,
                Some("/f"),
                now(),
            )
            .unwrap();

        let (row, resolved) = s.conflict(id).unwrap().unwrap();
        assert_eq!(row.path, "Timm.kdbx");
        assert_eq!(row.version_dir.as_deref(), Some("/f"));
        assert!(!resolved);

        s.resolve_conflict(id, now()).unwrap();
        assert!(s.conflict(id).unwrap().unwrap().1, "now resolved");
    }

    #[test]
    fn outbox_and_index_start_empty() {
        let s = Store::open_memory().unwrap();
        assert_eq!(s.outbox_len().unwrap(), 0);
        assert_eq!(s.outbox_oldest().unwrap(), None);

        let stats = s.index_stats().unwrap();
        assert_eq!(stats.files, 0);
        assert_eq!(stats.dirs, 0);
        assert_eq!(stats.last_crawl, None);

        s.meta_set("last_full_crawl", &now().timestamp().to_string())
            .unwrap();
        assert_eq!(s.index_stats().unwrap().last_crawl, Some(now()));
    }

    #[test]
    fn queuing_an_outbox_write_round_trips_the_baseline() {
        let s = Store::open_memory().unwrap();
        let id = s
            .queue_outbox(
                "od",
                "Dokumente/notes.txt",
                "/home/t/OneDrive.pending/od/Dokumente/notes.txt",
                Some(5),
                Some("abc123"),
                Some(now()),
                now(),
            )
            .unwrap();

        let row = s.outbox_record(id).unwrap().unwrap();
        assert_eq!(row.remote, "od");
        assert_eq!(row.path, "Dokumente/notes.txt");
        assert_eq!(row.base_size, Some(5));
        assert_eq!(row.base_quickxor.as_deref(), Some("abc123"));
        assert_eq!(row.base_mtime, Some(now()));
        assert_eq!(row.state, "queued");
        assert_eq!(row.attempts, 0);

        assert_eq!(s.outbox_len().unwrap(), 1);
        assert_eq!(s.outbox_oldest().unwrap(), Some(now()));
    }

    /// A brand-new file has no baseline at all — the index never saw it — and
    /// that has to be representable, not forced into a fake zero.
    #[test]
    fn a_new_files_outbox_row_can_have_no_baseline() {
        let s = Store::open_memory().unwrap();
        let id = s
            .queue_outbox("od", "new.txt", "/staged/new.txt", None, None, None, now())
            .unwrap();
        let row = s.outbox_record(id).unwrap().unwrap();
        assert_eq!(row.base_size, None);
        assert_eq!(row.base_quickxor, None);
        assert_eq!(row.base_mtime, None);
    }

    /// Staging the same path twice must not reset the baseline a later check
    /// relies on — the second edit is still checked against what the remote
    /// looked like when the *first* one started.
    #[test]
    fn restaging_the_same_path_keeps_the_original_baseline() {
        let s = Store::open_memory().unwrap();
        let first = s
            .queue_outbox(
                "od",
                "a.txt",
                "/staged/a.txt",
                Some(1),
                Some("orig"),
                None,
                now(),
            )
            .unwrap();
        let second = s
            .queue_outbox(
                "od",
                "a.txt",
                "/staged/a.txt",
                Some(999),
                Some("different"),
                None,
                now(),
            )
            .unwrap();

        assert_eq!(first, second, "must return the existing row, not a new one");
        assert_eq!(
            s.outbox_record(first)
                .unwrap()
                .unwrap()
                .base_quickxor
                .as_deref(),
            Some("orig")
        );
        assert_eq!(s.open_outbox().unwrap().len(), 1);
    }

    #[test]
    fn a_failed_replay_stays_queued_with_the_error_recorded() {
        let s = Store::open_memory().unwrap();
        let id = s
            .queue_outbox("od", "a.txt", "/staged/a.txt", None, None, None, now())
            .unwrap();

        s.mark_outbox_failed(id, "connection refused").unwrap();
        let row = s.outbox_record(id).unwrap().unwrap();
        assert_eq!(row.state, "failed");
        assert_eq!(row.attempts, 1);
        assert_eq!(row.last_err.as_deref(), Some("connection refused"));
        assert_eq!(
            s.outbox_len().unwrap(),
            1,
            "a failure must not drop it from the queue"
        );

        s.mark_outbox_failed(id, "still down").unwrap();
        assert_eq!(s.outbox_record(id).unwrap().unwrap().attempts, 2);
    }

    #[test]
    fn removing_an_outbox_row_resolves_it() {
        let s = Store::open_memory().unwrap();
        let id = s
            .queue_outbox("od", "a.txt", "/staged/a.txt", None, None, None, now())
            .unwrap();
        assert_eq!(s.outbox_len().unwrap(), 1);

        s.remove_outbox(id).unwrap();
        assert_eq!(s.outbox_len().unwrap(), 0);
        assert!(s.outbox_record(id).unwrap().is_none());
        assert!(s.outbox_for("od", "a.txt").unwrap().is_none());
    }

    #[test]
    fn open_outbox_lists_oldest_first_across_remotes() {
        let s = Store::open_memory().unwrap();
        let t0 = now();
        s.queue_outbox("od-a", "a.txt", "/s/a.txt", None, None, None, t0)
            .unwrap();
        s.queue_outbox(
            "od-b",
            "b.txt",
            "/s/b.txt",
            None,
            None,
            None,
            t0 + chrono::Duration::seconds(10),
        )
        .unwrap();

        let rows = s.open_outbox().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].remote, "od-a");
        assert_eq!(rows[1].remote, "od-b");
    }

    #[test]
    fn index_stats_counts_files_and_dirs_separately() {
        let s = Store::open_memory().unwrap();
        for (path, is_dir) in [("a", 1), ("a/f1", 0), ("a/f2", 0)] {
            s.conn
                .execute(
                    "INSERT INTO node (remote, path, path_ci, parent, name, is_dir, size, mtime, seen_gen)
                     VALUES ('od', ?1, ?1, '', ?1, ?2, 0, 0, 1)",
                    params![path, is_dir],
                )
                .unwrap();
        }
        let stats = s.index_stats().unwrap();
        assert_eq!(stats.files, 2);
        assert_eq!(stats.dirs, 1);
    }

    fn file(path: &str, size: u64) -> NodeInput {
        NodeInput {
            path: path.to_string(),
            is_dir: false,
            size,
            mtime: now(),
            quickxor: Some("abc123".into()),
        }
    }

    fn dir(path: &str) -> NodeInput {
        NodeInput {
            path: path.to_string(),
            is_dir: true,
            size: 0,
            mtime: now(),
            quickxor: None,
        }
    }

    #[test]
    fn upserted_nodes_are_findable_by_path_and_by_parent() {
        let s = Store::open_memory().unwrap();
        s.upsert_nodes(
            "od",
            &[dir("Keepass"), file("Keepass/Timm.kdbx", 76_000)],
            1,
        )
        .unwrap();

        let node = s.node("od", "Keepass/Timm.kdbx").unwrap().unwrap();
        assert_eq!(node.name, "Timm.kdbx");
        assert_eq!(node.parent, "Keepass");
        assert!(!node.is_dir);
        assert_eq!(node.size, 76_000);
        assert_eq!(node.quickxor.as_deref(), Some("abc123"));

        let children = s.children("od", "Keepass").unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].path, "Keepass/Timm.kdbx");

        assert!(s.node("od", "nope").unwrap().is_none());
    }

    /// Two remotes can legitimately share a relative path; the composite key
    /// must keep them apart rather than one clobbering the other.
    #[test]
    fn two_remotes_do_not_collide_on_the_same_path() {
        let s = Store::open_memory().unwrap();
        s.upsert_nodes("od-a", &[file("notes.txt", 10)], 1).unwrap();
        s.upsert_nodes("od-b", &[file("notes.txt", 99)], 1).unwrap();

        assert_eq!(s.node("od-a", "notes.txt").unwrap().unwrap().size, 10);
        assert_eq!(s.node("od-b", "notes.txt").unwrap().unwrap().size, 99);
        assert_eq!(s.children("od-a", "").unwrap().len(), 1);
        assert_eq!(s.children("od-b", "").unwrap().len(), 1);
    }

    #[test]
    fn upserting_twice_updates_in_place_rather_than_duplicating() {
        let s = Store::open_memory().unwrap();
        s.upsert_nodes("od", &[file("a.txt", 10)], 1).unwrap();
        s.upsert_nodes("od", &[file("a.txt", 20)], 2).unwrap();

        let rows = s.children("od", "").unwrap();
        assert_eq!(rows.len(), 1, "must update, not insert a second row");
        assert_eq!(rows[0].size, 20);
    }

    /// An opportunistic update (no hash — a `stat()` cannot produce one) must
    /// not erase a hash a prior full crawl already recorded.
    #[test]
    fn an_update_with_no_hash_does_not_erase_a_previously_known_one() {
        let s = Store::open_memory().unwrap();
        s.upsert_nodes("od", &[file("a.txt", 10)], 1).unwrap();
        assert_eq!(
            s.node("od", "a.txt").unwrap().unwrap().quickxor.as_deref(),
            Some("abc123")
        );

        let mut no_hash = file("a.txt", 20);
        no_hash.quickxor = None;
        s.upsert_nodes("od", &[no_hash], 2).unwrap();

        let node = s.node("od", "a.txt").unwrap().unwrap();
        assert_eq!(node.size, 20, "the rest of the update still applies");
        assert_eq!(
            node.quickxor.as_deref(),
            Some("abc123"),
            "the hash must survive"
        );
    }

    #[test]
    fn find_child_is_case_insensitive() {
        let s = Store::open_memory().unwrap();
        s.upsert_nodes("od", &[file("Keepass/Timm.kdbx", 1)], 1)
            .unwrap();

        for name in ["timm.kdbx", "TIMM.KDBX", "Timm.kdbx"] {
            assert_eq!(
                s.find_child("od", "Keepass", name).unwrap().unwrap().path,
                "Keepass/Timm.kdbx",
                "{name}"
            );
        }
        assert!(
            s.find_child("od", "Keepass", "nope.kdbx")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn node_by_path_ci_resolves_a_full_path_regardless_of_case() {
        let s = Store::open_memory().unwrap();
        s.upsert_nodes("od", &[dir("Keepass"), file("Keepass/Timm.kdbx", 1)], 1)
            .unwrap();

        let node = s
            .node_by_path_ci("od", "keepass/timm.kdbx")
            .unwrap()
            .unwrap();
        assert_eq!(node.path, "Keepass/Timm.kdbx");
    }

    /// The core of the crawl-driven deletion path: a full crawl that no
    /// longer sees a path means it was deleted upstream, and pruning is how
    /// that reaches the offline index.
    #[test]
    fn pruning_removes_only_what_the_newer_generation_did_not_confirm() {
        let s = Store::open_memory().unwrap();
        s.upsert_nodes("od", &[file("keep.txt", 1), file("gone.txt", 1)], 1)
            .unwrap();

        // Generation 2 re-confirms only "keep.txt".
        s.upsert_nodes("od", &[file("keep.txt", 1)], 2).unwrap();
        let removed = s.prune_nodes_before("od", 2).unwrap();

        assert_eq!(removed, 1);
        assert!(s.node("od", "keep.txt").unwrap().is_some());
        assert!(s.node("od", "gone.txt").unwrap().is_none());
    }

    /// Pruning is scoped per remote: a crawl of one remote must not evict
    /// another remote's untouched entries just because it used a later
    /// generation number.
    #[test]
    fn pruning_does_not_cross_remotes() {
        let s = Store::open_memory().unwrap();
        s.upsert_nodes("od-a", &[file("a.txt", 1)], 1).unwrap();
        s.upsert_nodes("od-b", &[file("b.txt", 1)], 1).unwrap();

        s.upsert_nodes("od-a", &[file("a.txt", 1)], 5).unwrap();
        s.prune_nodes_before("od-a", 5).unwrap();

        assert!(s.node("od-a", "a.txt").unwrap().is_some());
        assert!(
            s.node("od-b", "b.txt").unwrap().is_some(),
            "a different remote's generation 1 must survive"
        );
    }

    #[test]
    fn index_generation_defaults_to_zero_and_round_trips() {
        let s = Store::open_memory().unwrap();
        assert_eq!(s.index_generation().unwrap(), 0);
        s.set_index_generation(7).unwrap();
        assert_eq!(s.index_generation().unwrap(), 7);
    }

    #[test]
    fn access_is_tracked_per_remote_and_counts_reads() {
        let s = Store::open_memory().unwrap();
        s.record_access("od", "Keepass/Timm.kdbx", now()).unwrap();
        s.record_access("od", "Keepass/Timm.kdbx", now()).unwrap();

        let reads: u32 = s
            .conn
            .query_row(
                "SELECT reads FROM access WHERE remote='od' AND path='Keepass/Timm.kdbx'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(reads, 2);
    }

    #[test]
    fn search_finds_by_substring_case_insensitively_and_scoped_to_one_remote() {
        let s = Store::open_memory().unwrap();
        s.upsert_nodes(
            "od",
            &[
                dir("Keepass"),
                file("Keepass/Timm.kdbx", 1),
                file("notes.txt", 1),
            ],
            1,
        )
        .unwrap();
        s.upsert_nodes("other", &[file("Keepass/Timm.kdbx", 1)], 1)
            .unwrap();

        let hits: Vec<String> = s
            .search("od", "KEEPASS")
            .unwrap()
            .into_iter()
            .map(|n| n.path)
            .collect();
        assert_eq!(hits, vec!["Keepass", "Keepass/Timm.kdbx"]);

        assert!(
            s.search("od", "notes")
                .unwrap()
                .iter()
                .any(|n| n.path == "notes.txt")
        );
        assert!(s.search("od", "nope").unwrap().is_empty());
        assert_eq!(
            s.search("other", "kdbx").unwrap().len(),
            1,
            "scoped to its own remote"
        );
    }

    /// `%` and `_` are SQL `LIKE` wildcards; a search for either must be taken
    /// literally, not silently turned into a wildcard match.
    #[test]
    fn search_escapes_sql_wildcard_characters_in_the_query() {
        let s = Store::open_memory().unwrap();
        s.upsert_nodes(
            "od",
            &[file("100%_done.txt", 1), file("ordinary.txt", 1)],
            1,
        )
        .unwrap();

        let hits: Vec<String> = s
            .search("od", "%_done")
            .unwrap()
            .into_iter()
            .map(|n| n.path)
            .collect();
        assert_eq!(hits, vec!["100%_done.txt"]);
    }

    #[test]
    fn readonly_open_can_read_but_not_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        {
            let s = Store::open(&path).unwrap();
            s.daemon_started(99, "0.3.0", now()).unwrap();
        }

        let ro = Store::open_readonly(&path).unwrap();
        assert_eq!(ro.daemon().unwrap().unwrap().pid, 99);
        assert!(
            ro.heartbeat(now()).is_err(),
            "read-only store must refuse writes"
        );
    }

    #[test]
    fn readonly_open_of_a_missing_database_is_an_error_not_a_panic() {
        let err = Store::open_readonly(Path::new("/nonexistent/state.db")).unwrap_err();
        assert!(err.to_string().contains("read-only"), "{err}");
    }
}
