//! The log format, its human rendering, and retention.
//!
//! One stream, JSON lines, at `~/.local/state/onedrive-rc/log/onedrive-rcd.jsonl.<date>`.
//! The daemon writes it; `onedrive-rc log` renders it. rclone's own output is
//! parsed from its `--use-json-log` stdout and re-emitted into *this* stream, so
//! there is one timeline in one format rather than two files with different
//! timestamp precision that cannot be merged.
//!
//! Retention is enforced here, by both age and total size. The old design had a
//! one-shot truncate at startup that only applied to the daemon's own file, which
//! is how rclone's log reached 1.4 GB.

use std::cmp::Reverse;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Local, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use crate::code::Code;

/// Base name the daemon hands to its rolling appender; rotated files get a
/// `.YYYY-MM-DD` suffix appended by the appender.
pub const LOG_PREFIX: &str = "onedrive-rcd.jsonl";

// ---------------------------------------------------------------------------
// Records
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Level {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "TRACE",
            Self::Debug => "DEBUG",
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_uppercase().as_str() {
            "TRACE" => Self::Trace,
            "DEBUG" => Self::Debug,
            "INFO" => Self::Info,
            "WARN" | "WARNING" => Self::Warn,
            "ERROR" | "ERR" => Self::Error,
            _ => return None,
        })
    }
}

/// The structured fields we attach to every event worth showing the user.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fields {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Stable error code, so the log is greppable across releases.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// What the event is about: `mount/timm`, `pin/Keepass`, `index`, ...
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// Set to `rclone` on lines we ingested from a child process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin: Option<String>,
}

/// One line of the log. Unknown keys are tolerated so an older CLI can still
/// read a newer daemon's log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub timestamp: DateTime<Utc>,
    pub level: Level,
    #[serde(default)]
    pub target: String,
    #[serde(default)]
    pub fields: Fields,
}

impl Record {
    pub fn code(&self) -> Option<Code> {
        self.fields.code.as_deref().and_then(Code::parse)
    }

    /// `pin/Keepass` style label, falling back to the tracing target.
    ///
    /// The crate prefix is stripped from the fallback: every line comes from
    /// this daemon, so `onedrive_rcd::lifecycle` carries no more information than
    /// `lifecycle` while costing eleven characters of a fixed-width column.
    pub fn subject(&self) -> &str {
        self.fields
            .subject
            .as_deref()
            .or(self.fields.pin.as_deref())
            .or(self.fields.remote.as_deref())
            .unwrap_or_else(|| {
                self.target
                    .strip_prefix("onedrive_rcd::")
                    .unwrap_or(&self.target)
            })
    }

    pub fn message(&self) -> &str {
        self.fields.message.as_deref().unwrap_or("")
    }

    /// The actionable hint for this line, if its code has one.
    ///
    /// Only offered for warnings and errors: an `INFO` line about deferring work
    /// while offline does not need to tell anyone to do anything.
    pub fn hint(&self) -> Option<String> {
        if self.level < Level::Warn {
            return None;
        }
        let subject = self
            .fields
            .remote
            .as_deref()
            .or(self.fields.pin.as_deref())
            .unwrap_or("");
        self.code()?.hint(subject)
    }

    pub fn parse_line(line: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(line)
    }
}

// ---------------------------------------------------------------------------
// Filtering
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct Filter {
    /// Only records at or above this level.
    pub min_level: Option<Level>,
    pub since: Option<DateTime<Utc>>,
    pub remote: Option<String>,
    pub pin: Option<String>,
}

impl Filter {
    pub fn matches(&self, r: &Record) -> bool {
        if let Some(min) = self.min_level
            && r.level < min
        {
            return false;
        }
        if let Some(since) = self.since
            && r.timestamp < since
        {
            return false;
        }
        if let Some(remote) = &self.remote
            && r.fields.remote.as_deref() != Some(remote.as_str())
        {
            return false;
        }
        if let Some(pin) = &self.pin
            && r.fields.pin.as_deref() != Some(pin.as_str())
        {
            return false;
        }
        true
    }
}

// ---------------------------------------------------------------------------
// Human rendering
// ---------------------------------------------------------------------------

/// Wide enough for the longest subject we generate routinely,
/// `mount/<remote-name>`, so the code column stays aligned and can be scanned
/// down. Anything longer overflows rather than being truncated — a misaligned
/// line is better than a line whose subject you cannot read.
const SUBJECT_WIDTH: usize = 19;
const CODE_WIDTH: usize = 15;

/// Render one record the way `onedrive-rc log` shows it.
///
/// Fixed-width columns so the eye can scan down them, with the hint on a
/// continuation line aligned under the message.
///
/// Records are stored in UTC so they are unambiguous and sortable, but displayed
/// in local time — you are reading this to correlate against something that just
/// happened to you, not against a UTC clock.
pub fn render(r: &Record) -> String {
    let time = r.timestamp.with_timezone(&Local).format("%H:%M:%S");
    let level = r.level.as_str();
    let subject = r.subject();
    let code = r.code().map(|c| c.as_str()).unwrap_or("");

    // Built as one string so continuation lines can be measured from it rather
    // than recomputed from the column widths, which drifts the moment a subject
    // or code overflows its column.
    let prefix = format!("{time}  {level:<5}  {subject:<SUBJECT_WIDTH$}  {code:<CODE_WIDTH$} ");
    let indent = prefix.chars().count();

    let mut out = String::with_capacity(prefix.len() + r.message().len());
    out.push_str(&prefix);

    // A multi-line message (the config validator reports every problem at once)
    // must stay inside its column instead of resetting to the left margin.
    for (i, line) in r.message().lines().enumerate() {
        if i > 0 {
            let _ = write!(out, "\n{:indent$}", "");
        }
        out.push_str(line);
    }

    if let Some(hint) = r.hint() {
        let _ = write!(out, "\n{:indent$}fix: {hint}", "");
    }
    out
}

/// Render a whole log stream, skipping records the filter rejects.
///
/// Unparseable lines are surfaced rather than dropped: a corrupt log line is
/// itself information, and silently swallowing it is how you end up unable to
/// explain a gap.
pub fn render_stream<'a>(lines: impl Iterator<Item = &'a str>, filter: &Filter) -> Vec<String> {
    let mut out = Vec::new();
    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match Record::parse_line(line) {
            Ok(r) if filter.matches(&r) => out.push(render(&r)),
            Ok(_) => {}
            Err(e) => out.push(format!("(unparseable log line: {e})")),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Retention
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Pruned {
    pub removed: Vec<PathBuf>,
    pub bytes_freed: u64,
    pub bytes_kept: u64,
}

/// A rotated log file and the date parsed out of its name.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Rotated {
    path: PathBuf,
    /// `None` for the current, un-suffixed file, which sorts newest.
    date: Option<NaiveDate>,
    bytes: u64,
}

fn collect(dir: &Path) -> std::io::Result<Vec<Rotated>> {
    let mut found = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(found),
        Err(e) => return Err(e),
    };

    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with(LOG_PREFIX) {
            continue;
        }
        let meta = entry.metadata()?;
        if !meta.is_file() {
            continue;
        }

        // `onedrive-rcd.jsonl` (current) or `onedrive-rcd.jsonl.2026-10-08` (rotated).
        let date = name
            .strip_prefix(LOG_PREFIX)
            .and_then(|rest| rest.strip_prefix('.'))
            .and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok());

        found.push(Rotated {
            path: entry.path(),
            date,
            bytes: meta.len(),
        });
    }
    Ok(found)
}

/// Total bytes of all log files in `dir`. Used by `status`.
pub fn dir_size(dir: &Path) -> std::io::Result<u64> {
    Ok(collect(dir)?.iter().map(|r| r.bytes).sum())
}

/// Enforce retention by age and by total size.
///
/// `today` is passed in rather than read from the clock so this is deterministic
/// and testable. The newest file is never deleted, however far over budget we
/// are — losing the log you are currently writing is not a sensible way to save
/// disk space.
pub fn prune(
    dir: &Path,
    keep_days: u32,
    keep_bytes: u64,
    today: NaiveDate,
) -> std::io::Result<Pruned> {
    let mut files = collect(dir)?;
    let mut pruned = Pruned::default();

    // Newest first. The current (un-suffixed) file has no date and must sort as
    // the newest, not the oldest — otherwise size-based pruning deletes the file
    // we are actively writing.
    files.sort_by_key(|f| Reverse(f.date.unwrap_or(NaiveDate::MAX)));

    let cutoff = today - chrono::Duration::days(i64::from(keep_days));
    let mut keep: Vec<Rotated> = Vec::new();

    for (i, file) in files.into_iter().enumerate() {
        let too_old = file.date.is_some_and(|d| d < cutoff);
        // Index 0 is the newest file and is always retained.
        if too_old && i > 0 {
            std::fs::remove_file(&file.path)?;
            pruned.bytes_freed += file.bytes;
            pruned.removed.push(file.path);
        } else {
            keep.push(file);
        }
    }

    // Then trim oldest-first until we are under the size budget.
    let mut total: u64 = keep.iter().map(|f| f.bytes).sum();
    while total > keep_bytes && keep.len() > 1 {
        let oldest = keep.pop().expect("len > 1");
        std::fs::remove_file(&oldest.path)?;
        total -= oldest.bytes;
        pruned.bytes_freed += oldest.bytes;
        pruned.removed.push(oldest.path);
    }

    pruned.bytes_kept = total;
    Ok(pruned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(h: u32, m: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 8, h, m, s).unwrap()
    }

    fn record(level: Level, code: Option<&str>, subject: &str, msg: &str) -> Record {
        Record {
            timestamp: at(10, 40, 43),
            level,
            target: "onedrive-rcd".into(),
            fields: Fields {
                message: Some(msg.into()),
                code: code.map(Into::into),
                subject: Some(subject.into()),
                ..Default::default()
            },
        }
    }

    #[test]
    fn parses_the_json_shape_tracing_subscriber_emits() {
        let line = r#"{"timestamp":"2026-10-08T10:40:43.564763916Z","level":"WARN","target":"onedrive_rcd::pins","fields":{"message":"deferred: no network","code":"E_OFFLINE","subject":"pin/Keepass","pin":"Keepass"}}"#;
        let r = Record::parse_line(line).unwrap();
        assert_eq!(r.level, Level::Warn);
        assert_eq!(r.code(), Some(Code::Offline));
        assert_eq!(r.subject(), "pin/Keepass");
        assert_eq!(r.message(), "deferred: no network");
        assert_eq!(r.fields.pin.as_deref(), Some("Keepass"));
    }

    #[test]
    fn tolerates_unknown_keys_so_an_old_cli_can_read_a_new_log() {
        let line = r#"{"timestamp":"2026-10-08T10:40:43Z","level":"INFO","target":"t","fields":{"message":"m"},"span":{"x":1},"threadName":"main"}"#;
        assert_eq!(Record::parse_line(line).unwrap().message(), "m");
    }

    /// The crate prefix carries no information — every line is ours — and eats
    /// a third of the subject column.
    #[test]
    fn the_crate_prefix_is_stripped_from_the_fallback_subject() {
        let mut r = record(Level::Info, None, "x", "m");
        r.fields.subject = None;
        r.target = "onedrive_rcd::lifecycle".into();
        assert_eq!(r.subject(), "lifecycle");

        r.target = "something_else::bit".into();
        assert_eq!(
            r.subject(),
            "something_else::bit",
            "only our own prefix goes"
        );
    }

    /// Columns have to line up for the eye to scan down them, which is the whole
    /// reason for fixed widths.
    #[test]
    fn the_code_column_aligns_across_typical_subjects() {
        let mut columns = std::collections::HashSet::new();
        for subject in ["mount/onedrive-timm", "pin/Keepass", "config", "lifecycle"] {
            let r = record(Level::Warn, Some("E_OFFLINE"), subject, "m");
            let line = render(&r);
            columns.insert(line.find("E_OFFLINE").expect("code is present"));
        }
        assert_eq!(
            columns.len(),
            1,
            "the code column moved between subjects: {columns:?}"
        );
    }

    #[test]
    fn a_record_with_no_fields_still_parses() {
        let line = r#"{"timestamp":"2026-10-08T10:40:43Z","level":"ERROR","target":"t"}"#;
        let r = Record::parse_line(line).unwrap();
        assert_eq!(r.message(), "");
        assert_eq!(r.code(), None);
        assert_eq!(r.subject(), "t", "falls back to the tracing target");
    }

    #[test]
    fn levels_round_trip_and_order_by_severity() {
        assert!(Level::Error > Level::Warn);
        assert!(Level::Warn > Level::Info);
        assert!(Level::Info > Level::Debug);
        assert!(Level::Debug > Level::Trace);
        for l in [
            Level::Trace,
            Level::Debug,
            Level::Info,
            Level::Warn,
            Level::Error,
        ] {
            assert_eq!(Level::parse(l.as_str()), Some(l));
        }
        assert_eq!(Level::parse("warning"), Some(Level::Warn));
        assert_eq!(Level::parse("nope"), None);
    }

    #[test]
    fn a_local_time_stamp_with_an_offset_parses_to_the_same_instant() {
        let line = r#"{"timestamp":"2026-10-08T12:40:43.564763916+02:00","level":"INFO","target":"t","fields":{"message":"m"}}"#;
        let record: Record = serde_json::from_str(line).unwrap();
        assert_eq!(
            record.timestamp,
            at(10, 40, 43) + chrono::Duration::nanoseconds(564_763_916)
        );
    }

    #[test]
    fn renders_the_documented_layout() {
        let r = record(
            Level::Warn,
            Some("E_OFFLINE"),
            "pin/Keepass",
            "deferred: no network",
        );
        let out = render(&r);

        // Displayed in local time, so derive the expectation rather than
        // hardcoding one timezone's answer.
        let expected_time = r.timestamp.with_timezone(&Local).format("%H:%M:%S");
        assert!(
            out.starts_with(&format!(
                "{expected_time}  WARN   pin/Keepass          E_OFFLINE"
            )),
            "{out}"
        );
        assert!(out.ends_with("deferred: no network"), "{out}");
        // E_OFFLINE has no hint, so no continuation line.
        assert_eq!(out.lines().count(), 1, "{out}");
    }

    /// The config validator emits every problem in one message; those lines must
    /// stay in the message column instead of resetting to the left margin.
    #[test]
    fn a_multi_line_message_stays_inside_its_column() {
        let r = record(
            Level::Error,
            Some("E_CONFIG"),
            "config",
            "2 problems in the config:\n  1. a remote has an empty name\n  2. mount_point is empty",
        );
        let out = render(&r);
        let lines: Vec<&str> = out.lines().collect();

        let first_col = lines[0].find("2 problems").unwrap();
        assert_eq!(
            lines[1].find("1. a remote").unwrap(),
            first_col + 2,
            "{out}"
        );
        assert_eq!(
            lines[2].find("2. mount_point").unwrap(),
            first_col + 2,
            "{out}"
        );
        // The hint still comes last, at the same indent as the message.
        assert!(lines[3].trim_start().starts_with("fix:"), "{out}");
        assert_eq!(lines[3].find("fix:").unwrap(), first_col, "{out}");
    }

    #[test]
    fn renders_the_fix_hint_on_a_continuation_line() {
        let mut r = record(
            Level::Error,
            Some("E_AUTH_EXPIRED"),
            "mount/timm",
            "OneDrive token rejected",
        );
        r.fields.remote = Some("onedrive-timm".into());

        let out = render(&r);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2, "{out}");
        assert!(lines[0].contains("E_AUTH_EXPIRED"), "{out}");
        assert!(
            lines[1]
                .trim_start()
                .starts_with("fix: onedrive-rc setup --reauth onedrive-timm"),
            "{out}"
        );
        // The hint lines up under the message column.
        let msg_col = lines[0].find("OneDrive token rejected").unwrap();
        assert_eq!(lines[1].find("fix:").unwrap(), msg_col, "{out}");
    }

    #[test]
    fn info_lines_do_not_nag_with_hints() {
        let mut r = record(Level::Info, Some("E_CONFLICT"), "pin/Keepass", "noticed");
        r.fields.pin = Some("Keepass".into());
        assert!(r.hint().is_none());
        assert_eq!(render(&r).lines().count(), 1);
    }

    #[test]
    fn filters_by_level_time_remote_and_pin() {
        let mut warn = record(Level::Warn, None, "a", "w");
        warn.fields.remote = Some("od".into());
        warn.fields.pin = Some("Keepass".into());
        let info = record(Level::Info, None, "a", "i");

        let by_level = Filter {
            min_level: Some(Level::Warn),
            ..Default::default()
        };
        assert!(by_level.matches(&warn));
        assert!(!by_level.matches(&info));

        let later = Filter {
            since: Some(at(11, 0, 0)),
            ..Default::default()
        };
        assert!(!later.matches(&warn));
        let earlier = Filter {
            since: Some(at(10, 0, 0)),
            ..Default::default()
        };
        assert!(earlier.matches(&warn));

        let by_remote = Filter {
            remote: Some("od".into()),
            ..Default::default()
        };
        assert!(by_remote.matches(&warn));
        assert!(!by_remote.matches(&info));

        let by_pin = Filter {
            pin: Some("Keepass".into()),
            ..Default::default()
        };
        assert!(by_pin.matches(&warn));
        let other_pin = Filter {
            pin: Some("Other".into()),
            ..Default::default()
        };
        assert!(!other_pin.matches(&warn));

        assert!(
            Filter::default().matches(&info),
            "an empty filter matches everything"
        );
    }

    #[test]
    fn renders_a_stream_and_surfaces_corrupt_lines() {
        let lines = [
            r#"{"timestamp":"2026-10-08T10:40:43Z","level":"INFO","target":"t","fields":{"message":"one"}}"#,
            "",
            "{ truncated",
            r#"{"timestamp":"2026-10-08T10:40:44Z","level":"ERROR","target":"t","fields":{"message":"two"}}"#,
        ];
        let out = render_stream(lines.into_iter(), &Filter::default());
        assert_eq!(out.len(), 3, "{out:?}");
        assert!(out[0].contains("one"));
        assert!(out[1].contains("unparseable log line"), "{}", out[1]);
        assert!(out[2].contains("two"));
    }

    #[test]
    fn a_record_round_trips_through_json() {
        let r = record(Level::Warn, Some("E_OFFLINE"), "pin/Keepass", "m");
        let text = serde_json::to_string(&r).unwrap();
        assert_eq!(Record::parse_line(&text).unwrap(), r);
    }

    // -- retention ---------------------------------------------------------

    fn write_log(dir: &Path, suffix: Option<&str>, bytes: usize) -> PathBuf {
        let name = match suffix {
            Some(s) => format!("{LOG_PREFIX}.{s}"),
            None => LOG_PREFIX.to_string(),
        };
        let path = dir.join(name);
        std::fs::write(&path, vec![b'x'; bytes]).unwrap();
        path
    }

    fn day(d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, d).unwrap()
    }

    #[test]
    fn prunes_by_age() {
        let dir = tempfile::tempdir().unwrap();
        let old = write_log(dir.path(), Some("2026-09-01"), 10);
        let recent = write_log(dir.path(), Some("2026-10-07"), 10);
        let current = write_log(dir.path(), None, 10);

        let pruned = prune(dir.path(), 7, u64::MAX, day(8)).unwrap();
        assert_eq!(pruned.removed, vec![old.clone()]);
        assert_eq!(pruned.bytes_freed, 10);
        assert!(!old.exists());
        assert!(recent.exists());
        assert!(current.exists());
    }

    #[test]
    fn prunes_by_total_size_oldest_first() {
        let dir = tempfile::tempdir().unwrap();
        let d5 = write_log(dir.path(), Some("2026-10-05"), 100);
        let d6 = write_log(dir.path(), Some("2026-10-06"), 100);
        let d7 = write_log(dir.path(), Some("2026-10-07"), 100);
        let current = write_log(dir.path(), None, 100);

        // Budget fits two files, so the two oldest go.
        let pruned = prune(dir.path(), 365, 250, day(8)).unwrap();
        assert_eq!(pruned.removed, vec![d5, d6]);
        assert_eq!(pruned.bytes_kept, 200);
        assert!(d7.exists());
        assert!(current.exists());
    }

    /// Deleting the log you are currently writing is not a sensible way to free
    /// disk space, however far over budget we are.
    #[test]
    fn never_deletes_the_newest_file() {
        let dir = tempfile::tempdir().unwrap();
        let current = write_log(dir.path(), None, 5000);

        let pruned = prune(dir.path(), 0, 10, day(8)).unwrap();
        assert!(pruned.removed.is_empty(), "{pruned:?}");
        assert!(current.exists());
        assert_eq!(pruned.bytes_kept, 5000);
    }

    #[test]
    fn keeps_the_newest_even_when_every_file_is_too_old() {
        let dir = tempfile::tempdir().unwrap();
        write_log(dir.path(), Some("2026-01-01"), 10);
        let newest = write_log(dir.path(), Some("2026-02-01"), 10);

        let pruned = prune(dir.path(), 1, u64::MAX, day(8)).unwrap();
        assert_eq!(pruned.removed.len(), 1);
        assert!(
            newest.exists(),
            "the newest file survives even if it is ancient"
        );
    }

    #[test]
    fn ignores_unrelated_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notes.txt"), "keep me").unwrap();
        std::fs::write(dir.path().join("rclone-mount.log"), vec![b'x'; 9999]).unwrap();
        write_log(dir.path(), None, 10);

        assert_eq!(
            dir_size(dir.path()).unwrap(),
            10,
            "only our own files are counted"
        );
        let pruned = prune(dir.path(), 0, 0, day(8)).unwrap();
        assert!(pruned.removed.is_empty());
        assert!(dir.path().join("notes.txt").exists());
    }

    #[test]
    fn a_missing_log_dir_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope");
        assert_eq!(dir_size(&missing).unwrap(), 0);
        assert_eq!(prune(&missing, 7, 100, day(8)).unwrap(), Pruned::default());
    }

    #[test]
    fn reports_total_size() {
        let dir = tempfile::tempdir().unwrap();
        write_log(dir.path(), None, 100);
        write_log(dir.path(), Some("2026-10-07"), 250);
        assert_eq!(dir_size(dir.path()).unwrap(), 350);
    }
}
