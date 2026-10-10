//! Stable error codes.
//!
//! Every failure the user can see carries one of these, plus a one-line hint
//! saying what to do about it. This is what makes the log readable: the code is
//! greppable and stable across releases, and the hint means you do not have to
//! understand rclone's internals to act on it.

use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Code {
    /// No network. Not an error condition in itself — work is deferred.
    Offline,
    /// OneDrive rejected our token.
    AuthExpired,
    /// The FUSE mount is present but unusable (classically ENOTCONN after resume).
    MountStale,
    /// OneDrive is rate-limiting us.
    Throttled,
    /// Both sides of a pin changed; both versions were kept.
    Conflict,
    /// The `--check-access` sentinel was missing, so a destructive run was refused.
    CheckAccess,
    /// Out of disk space locally.
    DiskFull,
    /// Offline writes have been waiting too long to be replayed.
    OutboxStale,
    /// The offline index has not been refreshed recently enough to trust.
    IndexStale,
    /// The config file is invalid.
    Config,
    /// A required external program is missing or too old.
    Dependency,
    /// A command exists in the interface but its subsystem has not landed yet.
    /// Temporary, and removed as the remaining phases ship.
    NotImplemented,
    /// Anything we did not classify. Always a bug worth reporting.
    Internal,
}

impl Code {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Offline => "E_OFFLINE",
            Self::AuthExpired => "E_AUTH_EXPIRED",
            Self::MountStale => "E_MOUNT_STALE",
            Self::Throttled => "E_THROTTLED",
            Self::Conflict => "E_CONFLICT",
            Self::CheckAccess => "E_CHECK_ACCESS",
            Self::DiskFull => "E_DISK_FULL",
            Self::OutboxStale => "E_OUTBOX_STALE",
            Self::IndexStale => "E_INDEX_STALE",
            Self::Config => "E_CONFIG",
            Self::Dependency => "E_DEPENDENCY",
            Self::NotImplemented => "E_NOT_IMPLEMENTED",
            Self::Internal => "E_INTERNAL",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "E_OFFLINE" => Self::Offline,
            "E_AUTH_EXPIRED" => Self::AuthExpired,
            "E_MOUNT_STALE" => Self::MountStale,
            "E_THROTTLED" => Self::Throttled,
            "E_CONFLICT" => Self::Conflict,
            "E_CHECK_ACCESS" => Self::CheckAccess,
            "E_DISK_FULL" => Self::DiskFull,
            "E_OUTBOX_STALE" => Self::OutboxStale,
            "E_INDEX_STALE" => Self::IndexStale,
            "E_CONFIG" => Self::Config,
            "E_DEPENDENCY" => Self::Dependency,
            "E_NOT_IMPLEMENTED" => Self::NotImplemented,
            "E_INTERNAL" => Self::Internal,
            _ => return None,
        })
    }

    /// The raw hint, where `{}` stands for the subject (remote name, pin name, path).
    ///
    /// Phrased to read naturally after a `fix: ` prefix, so these must not repeat
    /// a lead-in of their own.
    pub const fn hint_template(self) -> Option<&'static str> {
        match self {
            Self::Offline => None, // deferring is the correct behaviour; nothing to fix
            Self::AuthExpired => Some("onedrive-tui setup --reauth {}"),
            Self::MountStale => Some(
                "the daemon will remount automatically; force it with: onedrive-tui mount remount",
            ),
            Self::Throttled => Some("OneDrive is throttling; the daemon backs off on its own"),
            Self::Conflict => Some("both versions were kept; review with: onedrive-tui conflicts"),
            Self::CheckAccess => Some(
                "a destructive run was refused because a side looked empty; check with: onedrive-tui doctor",
            ),
            Self::DiskFull => Some("free space, or lower vfs_cache_max_size for {}"),
            Self::OutboxStale => Some("inspect queued offline writes: onedrive-tui outbox ls"),
            Self::IndexStale => Some("refresh the offline index: onedrive-tui index refresh"),
            Self::Config => Some("check the config: onedrive-tui doctor"),
            Self::Dependency => Some("install the missing program, then: onedrive-tui doctor"),
            Self::NotImplemented => None,
            Self::Internal => Some("please report this with: onedrive-tui log --since 1h"),
        }
    }

    /// The hint with `{}` substituted. `subject` is typically a remote or pin name.
    pub fn hint(self, subject: &str) -> Option<String> {
        self.hint_template().map(|t| t.replace("{}", subject))
    }

    /// Whether this code means "try again later" rather than "something is broken".
    pub const fn is_transient(self) -> bool {
        matches!(self, Self::Offline | Self::Throttled | Self::MountStale)
    }
}

impl fmt::Display for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An error that carries a [`Code`] so the log and the CLI can render a hint.
#[derive(Debug, Clone)]
pub struct CodedError {
    pub code: Code,
    pub subject: String,
    pub msg: String,
}

impl CodedError {
    pub fn new(code: Code, subject: impl Into<String>, msg: impl Into<String>) -> Self {
        Self {
            code,
            subject: subject.into(),
            msg: msg.into(),
        }
    }

    pub fn config(msg: impl Into<String>) -> Self {
        Self::new(Code::Config, "config", msg)
    }

    pub fn hint(&self) -> Option<String> {
        self.code.hint(&self.subject)
    }
}

impl fmt::Display for CodedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.code, self.msg)
    }
}

impl std::error::Error for CodedError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_round_trip_through_strings() {
        for code in [
            Code::Offline,
            Code::AuthExpired,
            Code::MountStale,
            Code::Throttled,
            Code::Conflict,
            Code::CheckAccess,
            Code::DiskFull,
            Code::OutboxStale,
            Code::IndexStale,
            Code::Config,
            Code::Dependency,
            Code::NotImplemented,
            Code::Internal,
        ] {
            assert_eq!(Code::parse(code.as_str()), Some(code), "{code}");
        }
    }

    #[test]
    fn unknown_code_does_not_parse() {
        assert_eq!(Code::parse("E_NOPE"), None);
        assert_eq!(Code::parse(""), None);
    }

    #[test]
    fn hints_substitute_the_subject() {
        let hint = Code::AuthExpired.hint("onedrive-timm").unwrap();
        assert_eq!(hint, "onedrive-tui setup --reauth onedrive-timm");
    }

    #[test]
    fn offline_has_no_hint_because_deferring_is_correct() {
        assert!(Code::Offline.hint("x").is_none());
        assert!(Code::Offline.is_transient());
    }

    #[test]
    fn coded_error_displays_code_first() {
        let e = CodedError::new(Code::CheckAccess, "Keepass", "sentinel missing on remote");
        assert_eq!(e.to_string(), "[E_CHECK_ACCESS] sentinel missing on remote");
        assert!(e.hint().is_some());
    }
}
