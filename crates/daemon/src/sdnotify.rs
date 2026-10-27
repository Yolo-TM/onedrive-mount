//! Minimal `sd_notify` support: readiness, status text, and the watchdog.
//!
//! This is the other half of the "it survives" story. The unit is `Type=notify`
//! with `WatchdogSec=60`, so:
//!
//! * systemd does not consider the service started until we say `READY=1`,
//!   which means a dependency failure at startup is visible as a failed unit
//!   rather than an "active" service that does nothing;
//! * if we stop pinging `WATCHDOG=1`, systemd kills and restarts us. That is
//!   the recovery path for the old `mount_manager.rs:160` bug class, where an
//!   unbounded await on a hung FUSE mount froze the entire event loop and
//!   nothing — no config reload, no signal handling — ever ran again.
//!
//! Everything here is a no-op when not running under systemd, so the daemon can
//! still be run directly from a shell.

use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixDatagram;
use std::time::Duration;

/// Talks to the service manager, if there is one.
#[derive(Debug)]
pub struct Notifier {
    socket: Option<std::ffi::OsString>,
    watchdog: Option<Duration>,
}

impl Notifier {
    /// Read the environment systemd sets up for us.
    pub fn from_env() -> Self {
        let socket = std::env::var_os("NOTIFY_SOCKET").filter(|s| !s.is_empty());

        // WATCHDOG_PID, when set, means the watchdog is addressed at a specific
        // process; ignore it if that is not us.
        let for_us = match std::env::var("WATCHDOG_PID") {
            Ok(pid) => pid.trim().parse::<u32>().ok() == Some(std::process::id()),
            Err(_) => true,
        };

        let watchdog = std::env::var("WATCHDOG_USEC")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|usec| *usec > 0 && for_us)
            .map(Duration::from_micros);

        Self { socket, watchdog }
    }

    /// Whether a service manager is listening. Useful for logging which mode we
    /// are in; all the `notify_*` calls are safe regardless.
    pub fn is_supervised(&self) -> bool {
        self.socket.is_some()
    }

    /// How often to ping, i.e. half the configured watchdog interval. `None`
    /// when no watchdog is configured.
    pub fn watchdog_interval(&self) -> Option<Duration> {
        self.watchdog.map(|d| d / 2)
    }

    pub fn ready(&self) {
        self.send("READY=1\n");
    }

    pub fn stopping(&self) {
        self.send("STOPPING=1\n");
    }

    pub fn reloading(&self) {
        self.send("RELOADING=1\n");
    }

    /// One line of text shown by `systemctl status`.
    pub fn status(&self, text: &str) {
        // Newlines would be read as additional directives.
        let text = text.replace('\n', " ");
        self.send(&format!("STATUS={text}\n"));
    }

    pub fn watchdog_ping(&self) {
        self.send("WATCHDOG=1\n");
    }

    /// Relax the watchdog just before the machine suspends. The daemon is
    /// frozen with everything else, and on wake systemd can judge the deadline
    /// missed before our timer has run once — which kills a perfectly healthy
    /// daemon and, in a suspend/resume loop, restarts it hundreds of times.
    pub fn watchdog_suspend(&self) {
        if self.watchdog.is_some() {
            self.send("WATCHDOG_USEC=3600000000\nWATCHDOG=1\n");
        }
    }

    /// Restore the configured watchdog interval after waking.
    pub fn watchdog_resume(&self) {
        if let Some(d) = self.watchdog {
            self.send(&format!("WATCHDOG_USEC={}\nWATCHDOG=1\n", d.as_micros()));
        }
    }

    /// Ask the manager to restart us now, rather than waiting out the watchdog
    /// interval. Used by the mount supervisor in P1 when it detects a wedge it
    /// cannot recover from in-process.
    #[allow(dead_code)]
    pub fn watchdog_trigger(&self) {
        self.send("WATCHDOG=trigger\n");
    }

    /// Best-effort by design: failing to talk to the service manager must never
    /// take the daemon down, and there is nothing useful to do about it anyway.
    fn send(&self, msg: &str) {
        let Some(socket) = &self.socket else { return };
        let _ = send_datagram(socket, msg.as_bytes());
    }
}

fn send_datagram(socket: &std::ffi::OsStr, payload: &[u8]) -> std::io::Result<()> {
    let sock = UnixDatagram::unbound()?;
    let bytes = socket.as_bytes();

    // systemd uses a leading '@' to mean the abstract namespace, and a leading
    // NUL for the same thing on the wire.
    if bytes.first() == Some(&b'@') || bytes.first() == Some(&0) {
        use std::os::linux::net::SocketAddrExt;
        let addr = std::os::unix::net::SocketAddr::from_abstract_name(&bytes[1..])?;
        sock.connect_addr(&addr)?;
        sock.send(payload)?;
    } else {
        sock.send_to(payload, socket)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Without `NOTIFY_SOCKET` everything must be a silent no-op so the daemon
    /// runs fine straight from a shell.
    #[test]
    fn unsupervised_is_a_no_op() {
        let n = Notifier {
            socket: None,
            watchdog: None,
        };
        assert!(!n.is_supervised());
        assert_eq!(n.watchdog_interval(), None);

        // None of these may panic.
        n.ready();
        n.status("hello");
        n.watchdog_ping();
        n.reloading();
        n.stopping();
        n.watchdog_trigger();
    }

    #[test]
    fn pings_at_half_the_watchdog_interval() {
        let n = Notifier {
            socket: None,
            watchdog: Some(Duration::from_secs(60)),
        };
        assert_eq!(n.watchdog_interval(), Some(Duration::from_secs(30)));
    }

    #[test]
    fn sends_the_systemd_protocol_over_a_path_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notify");
        let listener = UnixDatagram::bind(&path).unwrap();

        let n = Notifier {
            socket: Some(path.into_os_string()),
            watchdog: None,
        };
        assert!(n.is_supervised());

        n.ready();
        n.watchdog_ping();
        n.status("mount up, 1 pin in sync");

        let mut buf = [0u8; 256];
        let len = listener.recv(&mut buf).unwrap();
        assert_eq!(&buf[..len], b"READY=1\n");

        let len = listener.recv(&mut buf).unwrap();
        assert_eq!(&buf[..len], b"WATCHDOG=1\n");

        let len = listener.recv(&mut buf).unwrap();
        assert_eq!(&buf[..len], b"STATUS=mount up, 1 pin in sync\n");
    }

    /// A newline in status text would be read as another directive.
    #[test]
    fn status_text_is_flattened_to_one_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notify");
        let listener = UnixDatagram::bind(&path).unwrap();

        let n = Notifier {
            socket: Some(path.into_os_string()),
            watchdog: None,
        };
        n.status("line one\nREADY=1");

        let mut buf = [0u8; 256];
        let len = listener.recv(&mut buf).unwrap();
        assert_eq!(&buf[..len], b"STATUS=line one READY=1\n");
    }

    /// A socket that has gone away must not take the daemon down.
    #[test]
    fn a_dead_notify_socket_is_tolerated() {
        let n = Notifier {
            socket: Some(std::ffi::OsString::from("/nonexistent/notify")),
            watchdog: None,
        };
        n.ready();
        n.watchdog_ping();
    }

    #[test]
    fn from_env_is_consistent() {
        // Cannot safely mutate env here; just assert the derived values agree.
        let n = Notifier::from_env();
        match n.watchdog {
            Some(full) => assert!(n.watchdog_interval().unwrap() < full),
            None => assert_eq!(n.watchdog_interval(), None),
        }
    }
}
