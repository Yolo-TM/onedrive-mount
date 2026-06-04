mod app;
mod config_watcher;
mod mount_manager;
mod rclone;
mod signal;
mod status_writer;
mod sync_executor;
mod sync_scheduler;

#[cfg(test)]
mod tests;

use anyhow::{Context, Result};
use onedrive_mount::{config::Config, paths::{config_file, daemon_pid_file, expand_tilde}, pid_lock::PidLock};
use tracing::error;
use tracing_subscriber::{fmt, EnvFilter, prelude::*};

#[tokio::main]
async fn main() -> Result<()> {
    // Handle --version before acquiring the PID lock so it works even if another instance runs
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("onedrive-mountd {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    check_rclone_or_exit();

    let config_path = config_file();
    let config = Config::load(&config_path)
        .with_context(|| format!("loading config from {}", config_path.display()))?;

    // Guard must be held for the process lifetime — stored in a local so Drop runs on exit
    let _logging_guard = init_logging(&config);

    let _lock = match PidLock::acquire(&daemon_pid_file()) {
        Ok(lock) => lock,
        Err(pid) => {
            error!("daemon already running (pid {pid}), exiting");
            std::process::exit(1);
        }
    };

    if let Err(e) = app::run(config).await {
        error!(error = %e, "daemon exited with error");
        std::process::exit(1);
    }

    Ok(())
}

fn check_rclone_or_exit() {
    let ok = std::process::Command::new("rclone")
        .arg("version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        eprintln!("error: 'rclone' not found in PATH. Install rclone before running the daemon.");
        std::process::exit(1);
    }
}

/// Initialises logging and returns the flush guard.
/// The caller must keep the guard alive for the process lifetime so the final
/// log lines are flushed before the process exits.
fn init_logging(config: &Config) -> tracing_appender::non_blocking::WorkerGuard {
    let log_path = expand_tilde(&config.log.file);
    let level = config.log.level.to_lowercase();

    let filter = EnvFilter::try_new(match level.as_str() {
        "debug"  => "debug",
        "info"   => "info",
        "notice" => "info",
        "error"  => "error",
        _        => "info",
    }).unwrap_or_else(|_| EnvFilter::new("info"));

    let log_dir  = log_path.parent().unwrap_or(std::path::Path::new("."));
    let log_file = log_path.file_name().unwrap_or(std::ffi::OsStr::new("onedrive-mount.log"));

    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let (file_writer, guard) = tracing_appender::non_blocking(
        tracing_appender::rolling::never(log_dir, log_file),
    );

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(file_writer).with_ansi(false))
        .init();

    guard
}
