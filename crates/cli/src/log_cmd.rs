//! `onedrive-rc log` — read the one log stream.
//!
//! The whole point of this command is that there *is* one stream to read. The
//! old setup had two files in two timestamp formats (nanosecond-precision RFC
//! 3339 on one side, `YYYY/MM/DD` second-precision on the other) that could not
//! be merged or grepped together, with no rotation on the larger one.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::Utc;
use clap::Args;
use onedrive_rc_core::log::{Filter, LOG_PREFIX, Level, render_stream};
use onedrive_rc_core::paths::Paths;
use onedrive_rc_core::units::Dur;

#[derive(Debug, Args)]
pub struct LogArgs {
    /// Follow the log as it grows.
    #[arg(short, long)]
    pub follow: bool,

    /// Only lines at or above this level (error, warn, info, debug, trace).
    #[arg(long, value_name = "LEVEL")]
    pub level: Option<String>,

    /// Only lines from the last duration, e.g. 1h, 30m, 2d.
    #[arg(long, value_name = "DURATION")]
    pub since: Option<String>,

    /// Only lines about this remote.
    #[arg(long)]
    pub remote: Option<String>,

    /// Only lines about this pin.
    #[arg(long)]
    pub pin: Option<String>,

    /// Show at most this many lines (ignored with --follow).
    #[arg(short = 'n', long, default_value_t = 200)]
    pub lines: usize,
}

impl LogArgs {
    fn filter(&self) -> Result<Filter> {
        let min_level = match &self.level {
            Some(text) => Some(Level::parse(text).with_context(|| {
                format!("{text:?} is not a level (error, warn, info, debug, trace)")
            })?),
            None => None,
        };

        let since = match &self.since {
            Some(text) => {
                let d = Dur::parse(text).map_err(|e| anyhow::anyhow!("--since {text:?}: {e}"))?;
                Some(Utc::now() - chrono::Duration::seconds(d.as_secs() as i64))
            }
            None => None,
        };

        Ok(Filter {
            min_level,
            since,
            remote: self.remote.clone(),
            pin: self.pin.clone(),
        })
    }
}

/// The last `n` rendered lines, unfiltered — for the TUI's log pane. Returns
/// an empty list rather than an error when there is nothing yet (no daemon
/// has run), since a dashboard pane says that itself rather than failing.
pub fn tail(paths: &Paths, n: usize) -> Result<Vec<String>> {
    let files = log_files(&paths.log_dir())?;
    let mut rendered = Vec::new();
    for file in &files {
        let text =
            std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
        rendered.extend(render_stream(text.lines(), &Filter::default()));
    }
    let start = rendered.len().saturating_sub(n);
    Ok(rendered.split_off(start))
}

pub fn run(paths: &Paths, args: &LogArgs) -> Result<()> {
    let dir = paths.log_dir();
    let filter = args.filter()?;

    let mut files = log_files(&dir)?;
    if files.is_empty() {
        anyhow::bail!(
            "no log files in {}\nthe daemon writes them once it starts: onedrive-rc service restart",
            dir.display()
        );
    }

    if args.follow {
        // Only the current file grows, so that is the one to follow.
        let current = files.pop().expect("non-empty");
        return follow(&current, &filter);
    }

    // Read oldest-first so the output is chronological across rotations.
    let mut rendered = Vec::new();
    for file in &files {
        let text =
            std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
        rendered.extend(render_stream(text.lines(), &filter));
    }

    let start = rendered.len().saturating_sub(args.lines);
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for line in &rendered[start..] {
        // A closed pipe (`| head`) is a normal way to stop, not an error.
        if writeln!(out, "{line}").is_err() {
            return Ok(());
        }
    }
    Ok(())
}

/// Our log files, oldest first. Rotated names sort chronologically by date, and
/// the current un-suffixed file is always the newest.
fn log_files(dir: &PathBuf) -> Result<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };

    let mut rotated: Vec<(String, PathBuf)> = Vec::new();
    let mut current = None;

    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with(LOG_PREFIX) || !entry.metadata()?.is_file() {
            continue;
        }
        match name
            .strip_prefix(LOG_PREFIX)
            .and_then(|r| r.strip_prefix('.'))
        {
            Some(date) => rotated.push((date.to_owned(), entry.path())),
            None => current = Some(entry.path()),
        }
    }

    rotated.sort();
    let mut files: Vec<PathBuf> = rotated.into_iter().map(|(_, p)| p).collect();
    files.extend(current);
    Ok(files)
}

/// Tail the current file, rendering new lines as they arrive.
fn follow(path: &PathBuf, filter: &Filter) -> Result<()> {
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut reader = BufReader::new(file);

    // Render what is already there, then block for more.
    let mut buffer = String::new();
    while reader.read_line(&mut buffer)? > 0 {
        buffer.clear();
    }

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    loop {
        buffer.clear();
        match reader.read_line(&mut buffer)? {
            0 => std::thread::sleep(std::time::Duration::from_millis(250)),
            _ => {
                for line in render_stream(std::iter::once(buffer.trim()), filter) {
                    if writeln!(out, "{line}").is_err() {
                        return Ok(());
                    }
                }
                let _ = out.flush();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> LogArgs {
        LogArgs {
            follow: false,
            level: None,
            since: None,
            remote: None,
            pin: None,
            lines: 200,
        }
    }

    fn write_log(paths: &Paths, suffix: Option<&str>, lines: &[&str]) {
        let dir = paths.log_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let name = match suffix {
            Some(s) => format!("{LOG_PREFIX}.{s}"),
            None => LOG_PREFIX.to_string(),
        };
        std::fs::write(dir.join(name), format!("{}\n", lines.join("\n"))).unwrap();
    }

    fn line(level: &str, msg: &str) -> String {
        format!(
            r#"{{"timestamp":"2026-10-08T10:40:43Z","level":"{level}","target":"t","fields":{{"message":"{msg}"}}}}"#
        )
    }

    #[test]
    fn files_are_ordered_oldest_first_with_the_current_one_last() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path());

        write_log(&paths, Some("2026-10-05"), &["a"]);
        write_log(&paths, Some("2026-10-07"), &["b"]);
        write_log(&paths, None, &["c"]);

        let files = log_files(&paths.log_dir()).unwrap();
        let names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec![
                "onedrive-rcd.jsonl.2026-10-05",
                "onedrive-rcd.jsonl.2026-10-07",
                "onedrive-rcd.jsonl",
            ]
        );
    }

    #[test]
    fn ignores_unrelated_files() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path());
        std::fs::create_dir_all(paths.log_dir()).unwrap();
        std::fs::write(paths.log_dir().join("rclone-mount.log"), "noise").unwrap();
        write_log(&paths, None, &["x"]);

        assert_eq!(log_files(&paths.log_dir()).unwrap().len(), 1);
    }

    #[test]
    fn a_missing_log_dir_explains_how_to_get_one() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path());

        let err = run(&paths, &args()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("no log files"), "{msg}");
        assert!(msg.contains("service restart"), "{msg}");
    }

    #[test]
    fn rejects_an_invalid_level_with_the_valid_set() {
        let mut a = args();
        a.level = Some("loud".into());
        let err = a.filter().unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("is not a level"), "{msg}");
        assert!(msg.contains("warn"), "{msg}");
    }

    #[test]
    fn rejects_an_invalid_since_duration() {
        let mut a = args();
        a.since = Some("yesterday".into());
        let err = a.filter().unwrap_err();
        assert!(format!("{err:#}").contains("--since"), "{err:#}");
    }

    /// `--since` accepts the compound and day forms the old parser rejected.
    #[test]
    fn accepts_rich_since_durations() {
        for text in ["1h", "30m", "2d", "1h30m"] {
            let mut a = args();
            a.since = Some(text.into());
            let filter = a.filter().unwrap();
            assert!(filter.since.is_some(), "{text}");
        }
    }

    #[test]
    fn builds_a_filter_from_every_flag() {
        let a = LogArgs {
            follow: false,
            level: Some("warn".into()),
            since: Some("1h".into()),
            remote: Some("od".into()),
            pin: Some("Keepass".into()),
            lines: 10,
        };
        let filter = a.filter().unwrap();
        assert_eq!(filter.min_level, Some(Level::Warn));
        assert!(filter.since.is_some());
        assert_eq!(filter.remote.as_deref(), Some("od"));
        assert_eq!(filter.pin.as_deref(), Some("Keepass"));
    }

    #[test]
    fn reads_across_rotated_files() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path());
        write_log(&paths, Some("2026-10-07"), &[&line("INFO", "older")]);
        write_log(&paths, None, &[&line("ERROR", "newer")]);

        // `run` writes to stdout, so exercise the pieces it composes instead.
        let files = log_files(&paths.log_dir()).unwrap();
        let mut rendered = Vec::new();
        for file in &files {
            let text = std::fs::read_to_string(file).unwrap();
            rendered.extend(render_stream(text.lines(), &Filter::default()));
        }
        assert_eq!(rendered.len(), 2);
        assert!(rendered[0].contains("older"));
        assert!(rendered[1].contains("newer"));
    }

    #[test]
    fn level_filtering_drops_quieter_lines() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path());
        write_log(
            &paths,
            None,
            &[&line("INFO", "chatter"), &line("ERROR", "the problem")],
        );

        let mut a = args();
        a.level = Some("warn".into());
        let filter = a.filter().unwrap();

        let text = std::fs::read_to_string(paths.log_dir().join(LOG_PREFIX)).unwrap();
        let rendered = render_stream(text.lines(), &filter);
        assert_eq!(rendered.len(), 1);
        assert!(rendered[0].contains("the problem"));
    }

    #[test]
    fn run_succeeds_against_a_real_log() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path());
        write_log(&paths, None, &[&line("INFO", "hello")]);

        run(&paths, &args()).expect("reading a valid log should succeed");
    }

    #[test]
    fn tail_returns_the_most_recent_lines_only() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path());
        write_log(
            &paths,
            None,
            &[
                &line("INFO", "one"),
                &line("INFO", "two"),
                &line("INFO", "three"),
            ],
        );

        let lines = tail(&paths, 2).unwrap();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("two"), "{lines:?}");
        assert!(lines[1].contains("three"), "{lines:?}");
    }

    #[test]
    fn tail_with_no_log_dir_is_an_empty_list_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path());
        assert_eq!(tail(&paths, 10).unwrap(), Vec::<String>::new());
    }
}
