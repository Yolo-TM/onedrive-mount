//! Logging setup: one JSONL stream, with retention actually enforced.
//!
//! Differences from the old setup, all of which were causes of the "logs are not
//! easily understandable" complaint:
//!
//! * One sink, not two. There is no second rclone log file in a different
//!   timestamp format; rclone's `--use-json-log` output is parsed and re-emitted
//!   into this stream (P1), so there is a single greppable timeline.
//! * Retention runs on a timer, by age *and* total size. The old code truncated
//!   once at startup and only touched its own file, which is how rclone's log
//!   reached 1.4 GB.
//! * No duplicate stderr sink when supervised. Under systemd the old daemon
//!   logged everything twice, to the file and to the journal, at two different
//!   levels.

use std::path::Path;

use anyhow::{Context, Result};
use onedrive_rc_core::config::LogLevel;
use onedrive_rc_core::log::LOG_PREFIX;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{EnvFilter, fmt};

/// Hold this for the process lifetime; dropping it flushes the writer.
#[derive(Debug)]
pub struct LogGuard(#[allow(dead_code)] WorkerGuard);

/// The on-disk JSONL layer.
///
/// Factored out so tests can exercise the *production* configuration against a
/// writer they control, instead of asserting against a duplicated copy of it.
/// Keeping the timestamp format here in step with what
/// `onedrive_rc_core::log::Record` parses is the contract between the daemon's
/// writer and the CLI's renderer.
fn json_layer<S>(
    writer: impl for<'a> fmt::MakeWriter<'a> + Send + Sync + 'static,
) -> Box<dyn tracing_subscriber::Layer<S> + Send + Sync>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fmt::layer()
        .json()
        .with_timer(fmt::time::ChronoUtc::rfc_3339())
        .with_current_span(false)
        .with_span_list(false)
        .with_target(true)
        .with_writer(writer)
        .boxed()
}

/// Install the global subscriber.
///
/// `to_stderr` selects human-readable output on stderr instead of JSONL on disk,
/// which is what you want when running the daemon by hand in a terminal.
pub fn init(log_dir: &Path, level: LogLevel, to_stderr: bool) -> Result<LogGuard> {
    std::fs::create_dir_all(log_dir)
        .with_context(|| format!("creating log directory {}", log_dir.display()))?;

    // `RUST_LOG` wins when set, so a one-off debug session needs no config edit.
    // The old unit set `Environment=RUST_LOG=info` while the code built its
    // filter with `EnvFilter::try_new`, which does not read the environment at
    // all — so that setting was silently inert.
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level.as_str()));

    let appender = tracing_appender::rolling::daily(log_dir, LOG_PREFIX);
    let (writer, guard) = tracing_appender::non_blocking(appender);

    let registry = tracing_subscriber::registry().with(filter);

    if to_stderr {
        registry
            .with(
                fmt::layer()
                    .with_writer(std::io::stderr)
                    .with_timer(fmt::time::ChronoLocal::rfc_3339())
                    .with_target(true)
                    .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr().lock())),
            )
            .try_init()
            .ok();
    } else {
        registry.with(json_layer(writer)).try_init().ok();
    }

    Ok(LogGuard(guard))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The daemon must come up even if the log directory cannot be created,
    /// reported as an error rather than a panic.
    #[test]
    fn reports_an_unusable_log_directory() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("log");
        std::fs::write(&blocker, "I am a file, not a directory").unwrap();

        let err = init(&blocker, LogLevel::Info, false).unwrap_err();
        assert!(
            err.to_string().contains("creating log directory"),
            "{err:#}"
        );
    }

    /// The daemon's JSONL layer must emit exactly what `onedrive_rc_core::log` can
    /// parse back — that contract is the whole reason there is one log stream
    /// instead of two.
    ///
    /// Scoped with `with_default` rather than installing the global subscriber:
    /// `try_init` can only win once per process, so a test that relied on it
    /// would pass or fail depending on which other test got there first.
    #[test]
    fn the_json_layer_emits_records_the_cli_can_parse() {
        use std::sync::{Arc, Mutex};

        use onedrive_rc_core::log::{Level, Record};
        use tracing_subscriber::prelude::*;

        /// Collects written bytes so the assertion needs no file or flush.
        #[derive(Clone, Default)]
        struct Buffer(Arc<Mutex<Vec<u8>>>);

        impl std::io::Write for Buffer {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        impl<'a> fmt::MakeWriter<'a> for Buffer {
            type Writer = Self;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let buffer = Buffer::default();
        let subscriber = tracing_subscriber::registry().with(json_layer(buffer.clone()));

        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(
                code = "E_OFFLINE",
                subject = "pin/Keepass",
                pin = "Keepass",
                "deferred: no network"
            );
        });

        let written = buffer.0.lock().unwrap().clone();
        let text = String::from_utf8(written).expect("valid utf-8");
        let line = text
            .lines()
            .find(|l| l.contains("E_OFFLINE"))
            .unwrap_or_else(|| panic!("nothing was written; got:\n{text}"));

        let record = Record::parse_line(line)
            .unwrap_or_else(|e| panic!("CLI cannot parse what the daemon wrote: {e}\n{line}"));
        assert_eq!(record.level, Level::Warn);
        assert_eq!(record.message(), "deferred: no network");
        assert_eq!(record.subject(), "pin/Keepass");
        assert_eq!(record.fields.pin.as_deref(), Some("Keepass"));
        assert_eq!(record.code(), Some(onedrive_rc_core::Code::Offline));
    }

    /// `init` must actually produce a file with the expected name shape, which
    /// is what `onedrive-rc log` and the retention pruner both key off.
    #[test]
    fn init_creates_a_log_file_with_the_expected_name() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("log");

        let guard = init(&log_dir, LogLevel::Info, false).unwrap();
        drop(guard);

        let names: Vec<String> = std::fs::read_dir(&log_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().any(|n| n.starts_with(LOG_PREFIX)),
            "expected a {LOG_PREFIX}* file, got {names:?}"
        );
    }
}
