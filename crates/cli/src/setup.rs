//! `onedrive-tui setup` — one command that finishes the job.
//!
//! The old GUI made completing OAuth step 1 of 4: you then had to find "Add
//! to config", then "Save" in a status bar, then "Install service", also a
//! small button in the same corner. Here it is: rclone OAuth → mount point →
//! pins → config written → service installed → linger enabled → first index
//! crawl queued, in one run. `--reauth <remote>` re-runs just the OAuth half
//! against an existing remote, which is what `Code::AuthExpired`'s hint
//! (`onedrive-tui setup --reauth <remote>`) points at.

use std::io::{self, Write};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use onedrive_core::config::Config;
use onedrive_core::ipc::Request;
use onedrive_core::paths::Paths;

use crate::rclone_wizard::{self, Question};
use crate::{client, service};

/// How long to wait for `onedrived` to bind its socket after `service
/// install` starts it, before giving up and telling the user to check it
/// themselves. Starting a daemon and binding a unix socket is fast; this is
/// generous, not a tight budget.
const DAEMON_STARTUP_TIMEOUT: Duration = Duration::from_secs(10);

pub fn run(paths: &Paths, reauth: Option<&str>) -> Result<i32> {
    match reauth {
        Some(name) => reauth_remote(paths, name),
        None => guided(paths),
    }
}

fn reauth_remote(paths: &Paths, name: &str) -> Result<i32> {
    check_rclone_present()?;
    println!("Re-authenticating {name} with rclone...\n");
    let q = rclone_wizard::reauth(&paths.rclone_config(), name)
        .with_context(|| format!("starting rclone config update for {name}"))?;
    drive(&paths.rclone_config(), name, q)?;
    println!("\n{name} re-authenticated.");

    // The daemon only picks up a token refresh by remounting; it watches the
    // rclone config file itself for nothing, since that is rclone's file, not
    // ours. A remount is cheap and makes the fix take effect immediately
    // rather than on the next unrelated restart.
    if client::try_request(
        paths,
        &Request::MountRemount {
            remote: Some(name.into()),
        },
    )?
    .is_some()
    {
        println!("remount requested.");
    }
    Ok(0)
}

fn guided(paths: &Paths) -> Result<i32> {
    println!("onedrive-tui setup\n");
    check_rclone_present()?;

    let config_path = paths.config_file();
    if config_path.exists() {
        println!("A config already exists at {}.", config_path.display());
        if !confirm("Reuse it and (re)install the service now?", true)? {
            bail!(
                "edit or remove {} yourself, then run setup again",
                config_path.display()
            );
        }
        return finish(paths);
    }

    let remote_name = prompt("Remote name", "onedrive")?;
    let mount_point = prompt("Mount point", "~/OneDrive")?;

    println!("\nAuthenticating {remote_name} with OneDrive...");
    println!("(a browser window should open; if nothing has a browser, answer `false`");
    println!(" to the first question for the headless token-paste flow)\n");
    let q = rclone_wizard::create(&paths.rclone_config(), &remote_name, "onedrive")
        .with_context(|| format!("starting rclone config create for {remote_name}"))?;
    drive(&paths.rclone_config(), &remote_name, q)?;
    println!("\n{remote_name} authenticated.\n");

    let mut pins_toml = String::new();
    while confirm(
        "Add a folder to keep always available offline (a pin)?",
        pins_toml.is_empty(),
    )? {
        let name = prompt("  Pin name", "Documents")?;
        let remote_path = prompt("  Remote path (relative to the remote root)", "Documents")?;
        let default_local = format!("~/onedrive-{}", name.to_lowercase().replace(' ', "-"));
        let local_path = prompt("  Local directory", &default_local)?;
        pins_toml.push_str(&format!(
            "\n[[remote.pin]]\nname = \"{name}\"\nremote_path = \"{remote_path}\"\nlocal_path = \"{local_path}\"\n"
        ));
        println!();
    }

    let text = format!(
        "[[remote]]\nname = \"{remote_name}\"\nmount_point = \"{mount_point}\"\n{pins_toml}"
    );
    let config: Config = toml::from_str(&text).context("building the generated config")?;
    config.validate(paths)?;
    config.save(&config_path)?;
    println!("wrote {}", config_path.display());

    finish(paths)
}

fn finish(paths: &Paths) -> Result<i32> {
    println!("\nInstalling the service...");
    print!("{}", service::install(paths)?);

    println!("\nWaiting for onedrived to start...");
    wait_for_daemon(paths)?;

    match client::request(paths, &Request::IndexRefresh { full: true }) {
        Ok(_) => println!("first index crawl queued."),
        Err(e) => println!("could not queue the first crawl yet: {e:#}"),
    }

    println!();
    print!(
        "{}",
        crate::render::status(&client::status(paths)?, chrono::Utc::now())
    );
    println!(
        "\nSetup complete. `onedrive-tui` for the dashboard, `onedrive-tui doctor` to check health."
    );
    Ok(0)
}

fn wait_for_daemon(paths: &Paths) -> Result<()> {
    let deadline = Instant::now() + DAEMON_STARTUP_TIMEOUT;
    loop {
        if client::try_request(paths, &Request::Ping)?.is_some() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!(
                "onedrived did not answer within {}s; check `onedrive-tui service status`",
                DAEMON_STARTUP_TIMEOUT.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn check_rclone_present() -> Result<()> {
    match std::process::Command::new("rclone").arg("version").output() {
        Ok(out) if out.status.success() => Ok(()),
        _ => bail!(
            "rclone is not on PATH\nfix: install rclone (nixos: environment.systemPackages = [ pkgs.rclone ])"
        ),
    }
}

/// Drive rclone's question/answer loop to completion on the terminal. Every
/// question is handled uniformly — including the local-browser one — rather
/// than auto-answering it the way the old GUI did: a TUI is just as likely to
/// be run over SSH with no local browser, so the user gets to actually answer
/// "do you have one," the same question rclone itself would ask interactively.
fn drive(config: &std::path::Path, name: &str, mut q: Question) -> Result<()> {
    loop {
        if !q.error.is_empty() {
            println!("that didn't work: {}", q.error.trim());
        }
        if q.is_done() {
            return Ok(());
        }
        let Some(option) = q.option.clone() else {
            return Ok(());
        };

        println!("{}", option.help.trim());
        if let Some(examples) = &option.examples {
            for e in examples {
                println!("  {} — {}", e.value, e.help);
            }
        }
        let answer_text = prompt(&option.name, &option.default_str)?;
        println!();
        q = rclone_wizard::answer(config, name, &q.state, &answer_text)?;
    }
}

fn prompt(label: &str, default: &str) -> Result<String> {
    if default.is_empty() {
        print!("{label}: ");
    } else {
        print!("{label} [{default}]: ");
    }
    io::stdout().flush().ok();
    let mut line = String::new();
    io::stdin()
        .read_line(&mut line)
        .context("reading from stdin")?;
    let line = line.trim();
    Ok(if line.is_empty() {
        default.to_string()
    } else {
        line.to_string()
    })
}

fn confirm(label: &str, default_yes: bool) -> Result<bool> {
    let suffix = if default_yes { "[Y/n]" } else { "[y/N]" };
    print!("{label} {suffix}: ");
    io::stdout().flush().ok();
    let mut line = String::new();
    io::stdin()
        .read_line(&mut line)
        .context("reading from stdin")?;
    Ok(match line.trim().to_lowercase().as_str() {
        "" => default_yes,
        "y" | "yes" => true,
        "n" | "no" => false,
        _ => default_yes,
    })
}
