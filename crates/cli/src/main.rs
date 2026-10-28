//! `onedrive-rc` — the control surface for `onedrive-rcd`.
//!
//! One binary: subcommands for scripting, and (from P5) a dashboard when invoked
//! with no arguments. It replaces a 2,251-line egui GUI whose four-step setup was
//! spread across three screen regions, and which re-read a multi-megabyte log
//! file on every frame.
//!
//! Commands that depend on subsystems which have not landed yet are present and
//! say so plainly, rather than being hidden — the interface is the contract.

mod client;
mod doctor;
mod index_cmd;
mod log_cmd;
mod rclone_wizard;
mod render;
mod service;
mod setup;
mod tui;

use anyhow::Result;
use chrono::Utc;
use clap::{Args, Parser, Subcommand};
use onedrive_rc_core::VERSION;
use onedrive_rc_core::ipc::Request;
use onedrive_rc_core::paths::Paths;

#[derive(Debug, Parser)]
#[command(
    name = "onedrive-rc",
    version = VERSION,
    about = "Control and inspect the OneDrive daemon",
    long_about = None,
    disable_help_subcommand = true,
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Show a health summary. Exits 0 healthy, 1 degraded, 2 broken.
    Status(StatusArgs),
    /// Check the things that commonly break, with a fix for each.
    Doctor,
    /// Show the daemon log.
    Log(log_cmd::LogArgs),
    /// Ask the daemon to re-read its config now.
    Reload,
    /// Install, remove or restart the systemd user service.
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// Manage always-offline folders.
    Pin {
        #[command(subcommand)]
        action: PinAction,
    },
    /// Review and resolve conflicts.
    Conflicts {
        #[command(subcommand)]
        action: Option<ConflictAction>,
    },
    /// Inspect and act on writes queued while offline.
    Outbox {
        #[command(subcommand)]
        action: Option<OutboxAction>,
    },
    /// Refresh the offline index.
    Index {
        #[command(subcommand)]
        action: IndexAction,
    },
    /// List a directory, offline — straight from the index.
    Ls {
        /// Path to list; omit for the root.
        path: Option<String>,
    },
    /// Find paths by name, offline — straight from the index.
    Find {
        /// Case-insensitive substring to search for.
        pattern: String,
    },
    /// Act on the mounts directly.
    Mount {
        #[command(subcommand)]
        action: MountAction,
    },
    /// Guided first-time setup: rclone OAuth, mount point, pins, the
    /// service, and a first crawl.
    Setup {
        /// Re-run just the OAuth half against an existing remote, e.g. after
        /// a token expires. Skips mount point, pins, and the service.
        #[arg(long, value_name = "REMOTE")]
        reauth: Option<String>,
    },
}

#[derive(Debug, Args)]
struct StatusArgs {
    /// Emit the full report as JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Subcommand)]
enum ServiceAction {
    /// Write the unit, enable it, start it, and enable linger.
    Install,
    /// Stop and remove the unit.
    Uninstall,
    /// Restart the daemon.
    Restart,
    /// Pass through `systemctl --user status`.
    Status,
}

#[derive(Debug, Subcommand)]
enum PinAction {
    /// List configured pins.
    Ls,
    /// Sync one pin now, or all of them.
    Sync {
        /// Pin name; omit for all.
        name: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum ConflictAction {
    /// List every unresolved conflict.
    Ls,
    /// Decide what to do about one.
    Resolve {
        /// Conflict id, as shown by `conflicts ls`.
        id: i64,
        /// Which version to keep. `both` changes no files and simply stops the
        /// alert — use it once you have merged them yourself.
        #[arg(long, value_enum)]
        keep: KeepArg,
    },
}

/// Mirrors [`onedrive_rc_core::ipc::Keep`]; separate so the protocol type does not
/// have to depend on clap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum KeepArg {
    Local,
    Remote,
    Both,
}

impl From<KeepArg> for onedrive_rc_core::ipc::Keep {
    fn from(arg: KeepArg) -> Self {
        match arg {
            KeepArg::Local => Self::Local,
            KeepArg::Remote => Self::Remote,
            KeepArg::Both => Self::Both,
        }
    }
}

#[derive(Debug, Subcommand)]
enum OutboxAction {
    /// List every write still queued.
    Ls,
    /// Replay now instead of waiting for the next reconnect.
    Retry {
        /// Remote name; omit for every enabled remote.
        remote: Option<String>,
    },
    /// Drop one queued write and delete its staged file.
    Discard {
        /// Outbox id, as shown by `outbox ls`.
        id: i64,
    },
}

#[derive(Debug, Subcommand)]
enum MountAction {
    /// Mount now (also lifts an earlier `unmount`).
    Mount {
        /// Remote name; omit for all enabled remotes.
        remote: Option<String>,
    },
    /// Unmount and stay unmounted until `mount` (or a daemon restart).
    Unmount {
        /// Remote name; omit for all enabled remotes.
        remote: Option<String>,
    },
    /// Remount now, without waiting out any restart backoff.
    Remount {
        /// Remote name; omit for all enabled remotes.
        remote: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum IndexAction {
    /// Refresh the index.
    Refresh {
        /// Do a full crawl rather than an incremental update.
        #[arg(long)]
        full: bool,
    },
}

fn main() {
    let cli = Cli::parse();
    let paths = Paths::from_env();

    match dispatch(&cli, &paths) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            // `{:#}` includes the anyhow context chain, which is where the
            // actionable part of the message usually lives.
            eprintln!("onedrive-rc: {e:#}");
            std::process::exit(1);
        }
    }
}

fn dispatch(cli: &Cli, paths: &Paths) -> Result<i32> {
    match &cli.command {
        None => {
            tui::run(paths)?;
            Ok(0)
        }

        Some(Cmd::Status(args)) => {
            let report = client::status(paths)?;
            if args.json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print!("{}", render::status(&report, Utc::now()));
            }
            Ok(report.health().0.exit_code())
        }

        Some(Cmd::Doctor) => {
            let checks = doctor::run(paths);
            print!("{}", doctor::render(&checks));
            Ok(match doctor::worst(&checks) {
                doctor::Verdict::Ok => 0,
                doctor::Verdict::Warn => 1,
                doctor::Verdict::Fail => 2,
            })
        }

        Some(Cmd::Log(args)) => {
            log_cmd::run(paths, args)?;
            Ok(0)
        }

        Some(Cmd::Reload) => {
            let response = client::request(paths, &Request::Reload)?;
            print_response(&response);
            Ok(exit_for(&response))
        }

        Some(Cmd::Service { action }) => {
            let output = match action {
                ServiceAction::Install => service::install(paths)?,
                ServiceAction::Uninstall => service::uninstall(paths)?,
                ServiceAction::Restart => service::restart(paths)?,
                ServiceAction::Status => service::status()?,
            };
            print!("{output}");
            Ok(0)
        }

        Some(Cmd::Pin { action }) => match action {
            PinAction::Ls => {
                let report = client::status(paths)?;
                if report.pins.is_empty() {
                    println!("no pins configured");
                } else {
                    for pin in &report.pins {
                        println!("{}", render::pin_line(pin, Utc::now()));
                    }
                }
                Ok(0)
            }
            PinAction::Sync { name } => {
                let response = client::request(paths, &Request::PinSync { name: name.clone() })?;
                print_response(&response);
                Ok(exit_for(&response))
            }
        },

        // No subcommand lists, because looking is the common case and it is
        // never destructive.
        Some(Cmd::Conflicts { action }) => match action {
            None | Some(ConflictAction::Ls) => {
                let response = client::request(paths, &Request::ConflictList)?;
                match response {
                    onedrive_rc_core::ipc::Response::Conflicts { conflicts: list } => {
                        print!("{}", render::conflicts(&list, Utc::now()));
                        // Non-zero while anything is unresolved, so this can
                        // drive a shell prompt indicator.
                        Ok(if list.is_empty() { 0 } else { 1 })
                    }
                    other => {
                        print_response(&other);
                        Ok(exit_for(&other))
                    }
                }
            }
            Some(ConflictAction::Resolve { id, keep }) => {
                let response = client::request(
                    paths,
                    &Request::ConflictResolve {
                        id: *id,
                        keep: (*keep).into(),
                    },
                )?;
                print_response(&response);
                Ok(exit_for(&response))
            }
        },

        // No subcommand lists, same reasoning as `conflicts`: looking is the
        // common case and it is never destructive.
        Some(Cmd::Outbox { action }) => match action {
            None | Some(OutboxAction::Ls) => {
                let response = client::request(paths, &Request::OutboxList)?;
                match response {
                    onedrive_rc_core::ipc::Response::Outbox { rows } => {
                        print!("{}", render::outbox(&rows, Utc::now()));
                        // Non-zero while anything is queued, so this can drive
                        // a shell prompt indicator, same as `conflicts`.
                        Ok(if rows.is_empty() { 0 } else { 1 })
                    }
                    other => {
                        print_response(&other);
                        Ok(exit_for(&other))
                    }
                }
            }
            Some(OutboxAction::Retry { remote }) => {
                let response = client::request(
                    paths,
                    &Request::OutboxRetry {
                        remote: remote.clone(),
                    },
                )?;
                print_response(&response);
                Ok(exit_for(&response))
            }
            Some(OutboxAction::Discard { id }) => {
                let response = client::request(paths, &Request::OutboxDiscard { id: *id })?;
                print_response(&response);
                Ok(exit_for(&response))
            }
        },

        Some(Cmd::Index { action }) => match action {
            IndexAction::Refresh { full } => {
                let response = client::request(paths, &Request::IndexRefresh { full: *full })?;
                print_response(&response);
                Ok(exit_for(&response))
            }
        },

        Some(Cmd::Ls { path }) => {
            print!("{}", index_cmd::ls(paths, path.as_deref())?);
            Ok(0)
        }

        Some(Cmd::Find { pattern }) => {
            print!("{}", index_cmd::find(paths, pattern)?);
            Ok(0)
        }

        Some(Cmd::Mount { action }) => {
            let request = match action {
                MountAction::Mount { remote } => Request::MountUp {
                    remote: remote.clone(),
                },
                MountAction::Unmount { remote } => Request::MountDown {
                    remote: remote.clone(),
                },
                MountAction::Remount { remote } => Request::MountRemount {
                    remote: remote.clone(),
                },
            };
            let response = client::request(paths, &request)?;
            print_response(&response);
            Ok(exit_for(&response))
        }

        Some(Cmd::Setup { reauth }) => setup::run(paths, reauth.as_deref()),
    }
}

fn print_response(response: &onedrive_rc_core::ipc::Response) {
    use onedrive_rc_core::ipc::Response;
    match response {
        Response::Ok { msg } => println!("{msg}"),
        Response::Err { code, msg, hint } => {
            eprintln!("[{code}] {msg}");
            if let Some(hint) = hint {
                eprintln!("fix: {hint}");
            }
        }
        Response::Pong { version, protocol } => {
            println!("onedrive-rcd {version} (protocol {protocol})");
        }
        Response::Status(_) => println!("ok"),
        Response::Conflicts { conflicts } => print!("{}", render::conflicts(conflicts, Utc::now())),
        Response::Outbox { rows } => print!("{}", render::outbox(rows, Utc::now())),
    }
}

fn exit_for(response: &onedrive_rc_core::ipc::Response) -> i32 {
    match response {
        onedrive_rc_core::ipc::Response::Err { .. } => 1,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_command_tree_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_the_documented_invocations() {
        assert!(matches!(
            Cli::parse_from(["onedrive-rc", "status"]).command,
            Some(Cmd::Status(StatusArgs { json: false }))
        ));
        assert!(matches!(
            Cli::parse_from(["onedrive-rc", "status", "--json"]).command,
            Some(Cmd::Status(StatusArgs { json: true }))
        ));
        assert!(matches!(
            Cli::parse_from(["onedrive-rc", "doctor"]).command,
            Some(Cmd::Doctor)
        ));
        assert!(matches!(
            Cli::parse_from(["onedrive-rc", "service", "install"]).command,
            Some(Cmd::Service {
                action: ServiceAction::Install
            })
        ));
        assert!(matches!(
            Cli::parse_from(["onedrive-rc", "index", "refresh", "--full"]).command,
            Some(Cmd::Index {
                action: IndexAction::Refresh { full: true }
            })
        ));

        match Cli::parse_from(["onedrive-rc", "ls"]).command {
            Some(Cmd::Ls { path }) => assert_eq!(path, None),
            other => panic!("unexpected {other:?}"),
        }
        match Cli::parse_from(["onedrive-rc", "ls", "Keepass"]).command {
            Some(Cmd::Ls { path }) => assert_eq!(path.as_deref(), Some("Keepass")),
            other => panic!("unexpected {other:?}"),
        }
        match Cli::parse_from(["onedrive-rc", "find", "Timm"]).command {
            Some(Cmd::Find { pattern }) => assert_eq!(pattern, "Timm"),
            other => panic!("unexpected {other:?}"),
        }
        assert!(
            Cli::try_parse_from(["onedrive-rc", "find"]).is_err(),
            "a pattern is required"
        );

        // The command the E_MOUNT_STALE hint tells people to run, so it has to
        // exist and parse exactly as written there.
        match Cli::parse_from(["onedrive-rc", "mount", "remount"]).command {
            Some(Cmd::Mount {
                action: MountAction::Remount { remote },
            }) => assert_eq!(remote, None),
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(
            Cli::parse_from(["onedrive-rc", "mount", "unmount"]).command,
            Some(Cmd::Mount {
                action: MountAction::Unmount { remote: None }
            })
        ));
        assert!(matches!(
            Cli::parse_from(["onedrive-rc", "mount", "mount", "od"]).command,
            Some(Cmd::Mount {
                action: MountAction::Mount { remote: Some(_) }
            })
        ));
        match Cli::parse_from(["onedrive-rc", "mount", "remount", "od"]).command {
            Some(Cmd::Mount {
                action: MountAction::Remount { remote },
            }) => assert_eq!(remote.as_deref(), Some("od")),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn a_bare_invocation_is_reserved_for_the_dashboard() {
        assert!(Cli::parse_from(["onedrive-rc"]).command.is_none());
    }

    #[test]
    fn pin_sync_takes_an_optional_name() {
        match Cli::parse_from(["onedrive-rc", "pin", "sync"]).command {
            Some(Cmd::Pin {
                action: PinAction::Sync { name },
            }) => assert_eq!(name, None),
            other => panic!("unexpected {other:?}"),
        }
        match Cli::parse_from(["onedrive-rc", "pin", "sync", "Keepass"]).command {
            Some(Cmd::Pin {
                action: PinAction::Sync { name },
            }) => {
                assert_eq!(name.as_deref(), Some("Keepass"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    /// Looking at conflicts has to be the default, because it is the
    /// non-destructive half and the one the notification tells people to run.
    #[test]
    fn conflicts_defaults_to_listing() {
        assert!(matches!(
            Cli::parse_from(["onedrive-rc", "conflicts"]).command,
            Some(Cmd::Conflicts { action: None })
        ));
        assert!(matches!(
            Cli::parse_from(["onedrive-rc", "conflicts", "ls"]).command,
            Some(Cmd::Conflicts {
                action: Some(ConflictAction::Ls)
            })
        ));
    }

    #[test]
    fn conflict_resolution_requires_an_explicit_choice() {
        for (arg, expected) in [
            ("local", KeepArg::Local),
            ("remote", KeepArg::Remote),
            ("both", KeepArg::Both),
        ] {
            match Cli::parse_from(["onedrive-rc", "conflicts", "resolve", "3", "--keep", arg])
                .command
            {
                Some(Cmd::Conflicts {
                    action: Some(ConflictAction::Resolve { id, keep }),
                }) => {
                    assert_eq!(id, 3);
                    assert_eq!(keep, expected);
                }
                other => panic!("unexpected {other:?}"),
            }
        }

        // No default: deleting a version of someone's password database is not
        // something to do because a flag was forgotten.
        assert!(Cli::try_parse_from(["onedrive-rc", "conflicts", "resolve", "3"]).is_err());
        assert!(
            Cli::try_parse_from([
                "onedrive-rc",
                "conflicts",
                "resolve",
                "3",
                "--keep",
                "newer"
            ])
            .is_err()
        );
    }

    /// Same reasoning as `conflicts_defaults_to_listing`: looking must be the
    /// default, since it is the non-destructive half.
    #[test]
    fn outbox_defaults_to_listing() {
        assert!(matches!(
            Cli::parse_from(["onedrive-rc", "outbox"]).command,
            Some(Cmd::Outbox { action: None })
        ));
        assert!(matches!(
            Cli::parse_from(["onedrive-rc", "outbox", "ls"]).command,
            Some(Cmd::Outbox {
                action: Some(OutboxAction::Ls)
            })
        ));
    }

    #[test]
    fn outbox_retry_and_discard_parse() {
        match Cli::parse_from(["onedrive-rc", "outbox", "retry"]).command {
            Some(Cmd::Outbox {
                action: Some(OutboxAction::Retry { remote }),
            }) => assert_eq!(remote, None),
            other => panic!("unexpected {other:?}"),
        }
        match Cli::parse_from(["onedrive-rc", "outbox", "retry", "onedrive-timm"]).command {
            Some(Cmd::Outbox {
                action: Some(OutboxAction::Retry { remote }),
            }) => assert_eq!(remote.as_deref(), Some("onedrive-timm")),
            other => panic!("unexpected {other:?}"),
        }
        match Cli::parse_from(["onedrive-rc", "outbox", "discard", "7"]).command {
            Some(Cmd::Outbox {
                action: Some(OutboxAction::Discard { id }),
            }) => assert_eq!(id, 7),
            other => panic!("unexpected {other:?}"),
        }
        assert!(Cli::try_parse_from(["onedrive-rc", "outbox", "discard"]).is_err());
    }

    #[test]
    fn setup_takes_an_optional_reauth_remote() {
        assert!(matches!(
            Cli::parse_from(["onedrive-rc", "setup"]).command,
            Some(Cmd::Setup { reauth: None })
        ));
        match Cli::parse_from(["onedrive-rc", "setup", "--reauth", "onedrive-timm"]).command {
            Some(Cmd::Setup { reauth }) => assert_eq!(reauth.as_deref(), Some("onedrive-timm")),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn unknown_commands_are_rejected() {
        assert!(Cli::try_parse_from(["onedrive-rc", "nonsense"]).is_err());
    }

    #[test]
    fn error_responses_exit_non_zero() {
        use onedrive_rc_core::Code;
        use onedrive_rc_core::ipc::Response;

        assert_eq!(exit_for(&Response::Ok { msg: "done".into() }), 0);
        assert_eq!(
            exit_for(&Response::err(Code::NotImplemented, "pins", "nope")),
            1
        );
    }
}
