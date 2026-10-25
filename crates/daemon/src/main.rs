//! `onedrive-rcd` — the OneDrive background daemon.
//!
//! Startup order is deliberate and differs from the old daemon's:
//!
//! 1. logging, so everything after this point lands in the log rather than on
//!    stderr (the old `main` loaded the config *before* initialising logging, so
//!    the most important failure — a bad config — was the one thing that never
//!    got logged);
//! 2. the `flock`, so a second instance is refused with a clear message;
//! 3. the state database;
//! 4. the config, which is validated but whose failure does not abort startup.

use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::Utc;
use onedrive_rc_core::config::Config;
use onedrive_rc_core::paths::Paths;
use onedrive_rc_core::state::Store;
use onedrive_rc_core::{VERSION, log as corelog};
use tokio::sync::{mpsc, watch};
use tracing::{error, info, warn};

use onedrive_rcd::app::{self, App};
use onedrive_rcd::lock::{self, Lock};
use onedrive_rcd::sdnotify::Notifier;
use onedrive_rcd::{configwatch, logging, mountloop, server};

/// Exit codes the shell and systemd can act on.
mod exit {
    /// Another daemon already holds the lock.
    pub const ALREADY_RUNNING: i32 = 3;
    /// `--check-config` found problems.
    pub const BAD_CONFIG: i32 = 4;
}

#[derive(Debug, Default)]
struct Args {
    /// Validate the config and exit.
    check_config: bool,
    /// Log human-readable output to stderr instead of JSONL on disk.
    foreground: bool,
    /// Print the systemd unit to stdout and exit.
    print_unit: bool,
}

fn parse_args() -> Result<Option<Args>> {
    let mut args = Args::default();
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--check-config" => args.check_config = true,
            "--foreground" | "-f" => args.foreground = true,
            "--print-unit" => args.print_unit = true,
            "--version" | "-V" => {
                println!("onedrive-rcd {VERSION}");
                return Ok(None);
            }
            "--help" | "-h" => {
                print!("{}", HELP);
                return Ok(None);
            }
            other => anyhow::bail!("unknown argument {other:?}\n\n{HELP}"),
        }
    }
    Ok(Some(args))
}

const HELP: &str = "\
onedrive-rcd — the OneDrive background daemon

Usage: onedrive-rcd [options]

Options:
  -f, --foreground   log to stderr in human-readable form instead of JSONL on disk
      --check-config validate the config file and exit
      --print-unit   print the systemd user unit to stdout and exit
  -V, --version      print the version and exit
  -h, --help         print this help and exit

The daemon is normally managed by systemd. Install the unit with:
  onedrive-rc service install
";

fn main() -> Result<()> {
    let Some(args) = parse_args()? else {
        return Ok(());
    };
    let paths = Paths::from_env();

    if args.print_unit {
        print!(
            "{}",
            onedrive_rc_core::unit::render(&std::env::current_exe()?)
        );
        return Ok(());
    }

    // `--check-config` is for scripts and for `doctor`, so it reports on stderr
    // and uses its exit code rather than touching the log or the database.
    if args.check_config {
        return match check_config(&paths) {
            Ok(()) => {
                println!("config OK: {}", paths.config_file().display());
                Ok(())
            }
            Err(msg) => {
                eprint!("{msg}");
                std::process::exit(exit::BAD_CONFIG);
            }
        };
    }

    let _log_guard = logging::init(&paths.log_dir(), initial_log_level(&paths), args.foreground)?;

    info!(version = VERSION, "onedrive-rcd starting");

    // Taken before anything else touches the state directory.
    let lock = match Lock::acquire(&paths.lock_file()) {
        Ok(lock) => lock,
        Err(e @ lock::LockError::Held { .. }) => {
            error!("{e}");
            eprintln!("{e}");
            std::process::exit(exit::ALREADY_RUNNING);
        }
        Err(e) => return Err(e).context("taking the daemon lock"),
    };
    info!(lock = %lock.path().display(), "daemon lock held");

    let runtime = tokio::runtime::Builder::new_multi_thread()
        // The daemon is a supervisor, not a compute workload; a small fixed pool
        // keeps idle memory down, which matters for something always resident.
        .worker_threads(2)
        .enable_all()
        .build()
        .context("starting the async runtime")?;

    let result = runtime.block_on(run(paths, args));

    // Dropping the lock and the log guard here, after the runtime has stopped,
    // means the final flush happens while the process is still orderly.
    drop(lock);
    result
}

/// Read just enough of the config to know what level to log at.
///
/// Done before full validation on purpose: if the config is broken we still want
/// the explanation of *why* to be logged, so we fall back to the default level
/// rather than refusing to log at all.
fn initial_log_level(paths: &Paths) -> onedrive_rc_core::config::LogLevel {
    Config::load(&paths.config_file())
        .map(|c| c.daemon.log_level)
        .unwrap_or_default()
}

fn check_config(paths: &Paths) -> std::result::Result<(), String> {
    let path = paths.config_file();
    if !path.exists() {
        return Err(format!(
            "no config file at {}\nrun: onedrive-rc setup\n",
            path.display()
        ));
    }
    Config::load_validated(&path, paths)
        .map(|_| ())
        .map_err(|e| format!("{}\n", e.msg))
}

async fn run(paths: Paths, _args: Args) -> Result<()> {
    let notifier = Notifier::from_env();
    if notifier.is_supervised() {
        info!(
            watchdog = ?notifier.watchdog_interval(),
            "running under a service manager"
        );
    } else {
        info!("no service manager detected; running standalone");
    }

    let store = Store::open(&paths.state_db()).context("opening the state database")?;
    store
        .daemon_started(std::process::id(), VERSION, Utc::now())
        .context("recording daemon startup")?;

    // Load the config, but treat failure as a reportable condition rather than a
    // reason to exit — exiting here is what turned a typo into a restart loop.
    let config_path = paths.config_file();
    let (config, config_error) = if config_path.exists() {
        match Config::load_validated(&config_path, &paths) {
            Ok(config) => {
                info!(
                    remotes = config.remotes.len(),
                    path = %config_path.display(),
                    "config loaded"
                );
                (config, None)
            }
            Err(e) => {
                error!(code = %e.code, "{}", e.msg);
                (Config::default(), Some(e.msg))
            }
        }
    } else {
        info!(path = %config_path.display(), "no config yet; run: onedrive-rc setup");
        (Config::default(), None)
    };

    let (reload_tx, reload_rx) = mpsc::channel::<()>(1);
    let (control_tx, control_rx) = mpsc::channel::<app::Control>(4);
    let app = Arc::new(App::new(
        paths.clone(),
        store,
        config,
        config_error.clone(),
        notifier,
        reload_tx,
        control_tx,
    ));

    if let Some(err) = &config_error {
        app.event(
            corelog::Level::Error,
            Some(onedrive_rc_core::Code::Config),
            "config",
            err,
        );
    }
    app.sync_pins_to_store();

    // One prune at startup, so a daemon that has been down a while does not wait
    // an hour to clean up after itself.
    {
        let app = Arc::clone(&app);
        tokio::task::spawn_blocking(move || {
            let config = app.config();
            if let Err(e) = corelog::prune(
                &app.paths.log_dir(),
                config.daemon.log_retention_days,
                config.daemon.log_retention_size.as_bytes(),
                Utc::now().date_naive(),
            ) {
                warn!(error = %e, "startup log prune failed");
            }
        });
    }

    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let socket = paths.socket();
    let listener = server::bind(&socket).context("binding the control socket")?;
    info!(socket = %socket.display(), "control socket ready");
    let server_task = tokio::spawn(server::serve(
        listener,
        socket,
        Arc::clone(&app),
        shutdown_rx.clone(),
    ));

    // The mount supervisor runs on its own task so that nothing mount-related
    // can block the main loop — the failure mode that used to freeze the whole
    // daemon when a FUSE mount hung.
    let mount_task = tokio::spawn(mountloop::run(Arc::clone(&app), control_rx, shutdown_rx));

    // Keep the watcher alive for as long as we run; dropping it stops the watch.
    let (config_changed, _watcher) = match configwatch::watch(&config_path) {
        Ok(pair) => (pair.0, Some(pair.1)),
        Err(e) => {
            // Not fatal: reloads can still be triggered over IPC or with SIGHUP.
            warn!(error = %e, "config watching unavailable; use `onedrive-rc service restart` or SIGHUP after edits");
            let (_tx, rx) = mpsc::channel::<()>(1);
            (rx, None)
        }
    };

    // Readiness is declared only once everything above is actually up, so a
    // failure before this point shows as a failed unit rather than a running
    // service that does nothing.
    app.notifier().ready();
    info!("ready");

    let result = app::run(app, reload_rx, config_changed, shutdown_tx).await;

    if let Err(e) = server_task.await {
        warn!(error = %e, "ipc server task did not shut down cleanly");
    }
    if let Err(e) = mount_task.await {
        warn!(error = %e, "mount supervisor did not shut down cleanly");
    }

    // When sockets had to fall back out of the runtime directory, that
    // directory is ours and nothing else cleans it up — tmpfiles only tidies
    // `/run/user`. `remove_dir` declines to remove a non-empty directory, so a
    // leftover socket from a concurrent instance is safe from this.
    if paths.socket_is_fallback() {
        let _ = std::fs::remove_dir(paths.socket_dir());
    }
    info!("stopped");
    result
}
