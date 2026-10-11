//! Driving rclone's own non-interactive config state machine.
//!
//! `rclone config create/update --non-interactive`, then repeated
//! `rclone config update --continue --state <s> --result <r>` until rclone
//! reports an empty `State`, is the only supported way to script rclone's own
//! OAuth dance — this is documented in `rclone config create --help` and is
//! what rclone's own interactive prompt uses internally. Ported from the old
//! GUI's `src/bin/gui/rclone_config_wizard.rs`, which got the protocol right;
//! what's fixed here, found by driving a real `onedrive`-type creation
//! against this machine's rclone (verified: reaching the `config_is_local`
//! and `config_token` questions, and a rejected token, needs no credentials
//! and no network call of our own):
//!
//! - every step runs under [`STEP_TIMEOUT`] rather than blocking forever on a
//!   hung child, except the OAuth wait itself ([`OAUTH_TIMEOUT`]), which is a
//!   human being waited on rather than rclone;
//! - a non-empty `Error` with a non-empty `State` is **not** fatal — verified
//!   against a real rejected token above, rclone re-offers the identical
//!   question rather than aborting. The old GUI code treated this as a hard
//!   `WizardStep::Error`, which would have ended the wizard on a single typo.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// Ordinary steps must answer within this long — rclone itself is local and
/// fast; anything slower means it is hung, not thinking.
const STEP_TIMEOUT: Duration = Duration::from_secs(20);

/// The OAuth step runs rclone's own local web server and blocks until the
/// browser redirect lands on it, or returns a `config_token` question whose
/// answer is a human pasting a string — both wait on a person, not rclone.
pub const OAUTH_TIMEOUT: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Question {
    #[serde(rename = "State")]
    pub state: String,
    #[serde(rename = "Option")]
    pub option: Option<QuestionOption>,
    #[serde(rename = "Error")]
    pub error: String,
}

impl Question {
    /// The conversation is over and the remote exists (or creation failed
    /// with nothing further to ask — `validate` at the config-load level
    /// catches an incomplete remote either way).
    pub fn is_done(&self) -> bool {
        self.state.is_empty()
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct QuestionOption {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Help")]
    pub help: String,
    #[serde(rename = "Examples")]
    pub examples: Option<Vec<Example>>,
    #[serde(rename = "IsPassword")]
    pub is_password: bool,
    #[serde(rename = "DefaultStr")]
    pub default_str: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Example {
    #[serde(rename = "Value")]
    pub value: String,
    #[serde(rename = "Help")]
    pub help: String,
}

/// First step for a brand-new remote.
pub fn create(config: &Path, name: &str, remote_type: &str) -> Result<Question> {
    run(
        config,
        &["config", "create", "--non-interactive", name, remote_type],
        STEP_TIMEOUT,
    )
}

/// First step for `--reauth`: `--all` so rclone asks every question again
/// — including the OAuth one — rather than skipping it as unchanged.
pub fn reauth(config: &Path, name: &str) -> Result<Question> {
    run(
        config,
        &["config", "update", "--non-interactive", "--all", name],
        STEP_TIMEOUT,
    )
}

/// Every step after the first, for either mode — rclone's continuation
/// protocol does not distinguish how the conversation started, only the
/// `--state` token it handed back.
pub fn answer(config: &Path, name: &str, state: &str, result: &str) -> Result<Question> {
    let timeout = if result == "true" {
        OAUTH_TIMEOUT
    } else {
        STEP_TIMEOUT
    };
    run(
        config,
        &[
            "config",
            "update",
            "--continue",
            "--state",
            state,
            "--result",
            result,
            name,
        ],
        timeout,
    )
}

fn run(config: &Path, args: &[&str], timeout: Duration) -> Result<Question> {
    let mut child = Command::new("rclone")
        .arg(format!("--config={}", config.display()))
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning rclone (is it on PATH?)")?;

    let output = wait_timeout(&mut child, timeout)?;
    parse(&output)
}

/// Polls rather than `wait_with_output`'s unbounded block, so a hung rclone
/// gets killed instead of wedging the whole wizard — the `kill_on_drop`-style
/// fix the plan calls for, adapted to `std::process` since the CLI has no
/// async runtime.
fn wait_timeout(
    child: &mut std::process::Child,
    timeout: Duration,
) -> Result<std::process::Output> {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().context("waiting for rclone")? {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            if let Some(mut s) = child.stdout.take() {
                let _ = s.read_to_end(&mut stdout);
            }
            if let Some(mut s) = child.stderr.take() {
                let _ = s.read_to_end(&mut stderr);
            }
            return Ok(std::process::Output {
                status,
                stdout,
                stderr,
            });
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "rclone did not respond within {}s (is it waiting on something, e.g. a browser?)",
                timeout.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn parse(output: &std::process::Output) -> Result<Question> {
    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.trim().is_empty() {
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let msg = stderr
                .lines()
                .find(|l| l.contains("Error:"))
                .unwrap_or(stderr.trim())
                .to_string();
            bail!("{msg}");
        }
        return Ok(Question {
            state: String::new(),
            option: None,
            error: String::new(),
        });
    }

    serde_json::from_str(stdout.trim())
        .with_context(|| format!("parsing rclone's response: {stdout}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drives real `rclone config create`/`update --continue` against this
    /// machine's actual rclone, far enough to prove the continuation protocol
    /// genuinely works — no credentials or network call of ours is needed to
    /// reach the OAuth question, decline the local-browser flow, reach the
    /// token-paste question, and see a bad answer come back as a *recoverable*
    /// error rather than aborting. This is the exact sequence that made the
    /// `Error`-is-fatal bug in the ported GUI code visible.
    #[test]
    fn the_onedrive_oauth_question_sequence_is_driven_for_real() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("rclone.conf");

        let q = create(&config, "wiztest", "onedrive").expect("rclone must be on PATH");
        let option = q.option.as_ref().expect("a question, not done yet");
        assert_eq!(option.name, "config_is_local");
        assert!(!q.is_done());

        // Decline the local-browser flow — the headless path, which asks for
        // a token pasted from `rclone authorize` run elsewhere.
        let q2 = answer(&config, "wiztest", &q.state, "false").unwrap();
        let option2 = q2.option.as_ref().expect("a question, not done yet");
        assert_eq!(option2.name, "config_token");
        assert!(option2.help.contains("rclone authorize"));

        // A garbage token is rejected, but the conversation is NOT over: the
        // same state comes back with a non-empty Error, which must be treated
        // as "ask again", not as a fatal abort.
        let q3 = answer(&config, "wiztest", &q2.state, "not-a-real-token").unwrap();
        assert!(!q3.error.is_empty());
        assert_eq!(
            q3.state, q2.state,
            "a rejected answer must re-offer the same question"
        );
        assert!(!q3.is_done(), "a recoverable error must not read as done");
    }
}
