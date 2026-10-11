//! Desktop notifications that persist while the problem does.
//!
//! Plain `org.freedesktop.Notifications` over D-Bus — on the target machine that
//! name is owned by `plasmashell`, so these are ordinary Plasma notifications
//! with no custom UI, no tray icon and no toolkit dependency.
//!
//! The requirement that shapes this module is that a conflict notification must
//! **keep being visible for as long as the conflict exists**, not fire once and
//! vanish. Three things together achieve that without resorting to spam:
//!
//! * `urgency = critical` and the `resident` hint, so the notification does not
//!   auto-expire;
//! * the returned id is stored in SQLite and **reused** when re-asserting, so
//!   repeating it replaces the existing notification rather than stacking up;
//! * a re-assert every 30 minutes while the conflict row is open, so dismissing
//!   it does not make the problem invisible.
//!
//! The old daemon had `notify_conflicts()` sitting in `rclone.rs` marked
//! `#[allow(dead_code)]` and never called from anywhere, which is why a conflict
//! that renamed a password database aside produced no visible sign at all.

use std::collections::HashMap;

use anyhow::{Context, Result};
use zbus::Connection;
use zbus::zvariant::Value;

/// How long between re-assertions of an unresolved problem.
pub const REASSERT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// freedesktop urgency levels.
const URGENCY_CRITICAL: u8 = 2;

/// What a notification is about, which decides its text and actions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Alert {
    /// Both sides changed and both versions were kept.
    Conflict {
        pin: String,
        /// The canonical file, e.g. `Timm.kdbx`.
        file: String,
        /// The timestamped copies kept beside it.
        copies: Vec<String>,
        /// Where they are, so the user can be sent straight there.
        folder: String,
        /// True when rclone left no winner and we chose one.
        restored: bool,
    },
    /// A pin refused to sync for a protective reason. This is not a transient
    /// failure and will not clear on its own.
    Refused {
        pin: String,
        msg: String,
        folder: String,
    },
}

impl Alert {
    pub fn summary(&self) -> String {
        match self {
            Self::Conflict { pin, .. } => format!("OneDrive — conflict in {pin}"),
            Self::Refused { pin, .. } => format!("OneDrive — {pin} needs attention"),
        }
    }

    /// Body text. Plasma renders a small subset of HTML, so this stays plain
    /// apart from newlines.
    pub fn body(&self) -> String {
        match self {
            Self::Conflict {
                file,
                copies,
                restored,
                ..
            } => {
                let mut out = format!(
                    "{file} changed on this computer and on OneDrive.\nBoth versions were kept:\n"
                );
                out.push_str(&format!("  {file}  (in use)\n"));
                for copy in copies {
                    out.push_str(&format!("  {copy}\n"));
                }
                if *restored {
                    out.push_str(
                        "\nThe two versions had identical timestamps, so the larger one was \
                         put back under the original name.\n",
                    );
                }
                out.push_str("\nMerge them, then delete the extra copy.");
                out
            }
            Self::Refused { msg, .. } => {
                format!("{msg}\n\nNothing was changed. Run `onedrive-tui doctor` for checks.")
            }
        }
    }

    pub fn folder(&self) -> &str {
        match self {
            Self::Conflict { folder, .. } | Self::Refused { folder, .. } => folder,
        }
    }

    /// A stable key, so repeated detections of the same problem reuse one
    /// notification instead of producing a new one each time.
    pub fn key(&self) -> String {
        match self {
            Self::Conflict { pin, file, .. } => format!("conflict:{pin}:{file}"),
            Self::Refused { pin, .. } => format!("refused:{pin}"),
        }
    }
}

/// Sends notifications, remembering their ids so they can be replaced.
pub struct Notifier {
    conn: Option<Connection>,
    /// Alert key to the id the server gave us.
    ids: HashMap<String, u32>,
}

impl Notifier {
    /// Connect to the session bus. A machine with no session bus — a headless
    /// box, a user not logged in — simply gets no notifications, which must not
    /// stop the daemon.
    pub async fn connect() -> Self {
        match Connection::session().await {
            Ok(conn) => {
                tracing::info!("desktop notifications available");
                Self {
                    conn: Some(conn),
                    ids: HashMap::new(),
                }
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "no session bus: conflicts will be reported in the log and \
                     `onedrive-tui status`, but not on screen"
                );
                Self {
                    conn: None,
                    ids: HashMap::new(),
                }
            }
        }
    }

    /// Whether notifications will actually be delivered.
    pub fn is_available(&self) -> bool {
        self.conn.is_some()
    }

    /// Show an alert, replacing any existing one for the same subject.
    ///
    /// `previous_id` is the id persisted in the database, so a daemon restart
    /// keeps replacing the same notification rather than adding a second.
    pub async fn show(&mut self, alert: &Alert, previous_id: Option<u32>) -> Result<Option<u32>> {
        let Some(conn) = &self.conn else {
            return Ok(None);
        };

        let key = alert.key();
        let replaces = previous_id
            .or_else(|| self.ids.get(&key).copied())
            .unwrap_or(0);

        let mut hints: HashMap<&str, Value> = HashMap::new();
        hints.insert("urgency", Value::U8(URGENCY_CRITICAL));
        // Keeps it on screen rather than letting it expire into history. The
        // server advertises `persistence`, so this is honoured; the zero
        // `expire_timeout` below is the stronger, spec-level guarantee.
        hints.insert("resident", Value::Bool(true));
        hints.insert("x-kde-origin-name", Value::Str("OneDrive".into()));
        // A native clickable link to the folder. Deliberately used *instead of*
        // action buttons: servicing an action means listening for
        // `ActionInvoked`, and a button that does nothing when clicked is worse
        // than no button. The server advertises `x-kde-urls`, so this needs no
        // handling on our side.
        hints.insert(
            "x-kde-urls",
            Value::Array(vec![format!("file://{}", alert.folder())].into()),
        );

        // Empty: see the `x-kde-urls` comment above.
        let actions: Vec<&str> = Vec::new();

        let id: u32 = conn
            .call_method(
                Some("org.freedesktop.Notifications"),
                "/org/freedesktop/Notifications",
                Some("org.freedesktop.Notifications"),
                "Notify",
                &(
                    "onedrive",    // app_name
                    replaces,      // replaces_id
                    "folder-sync", // app_icon
                    alert.summary(),
                    alert.body(),
                    actions,
                    hints,
                    // Never time out on its own; `resident` alone is advisory.
                    0i32,
                ),
            )
            .await
            .context("sending a desktop notification")?
            .body()
            .deserialize()
            .context("reading the notification id")?;

        self.ids.insert(key, id);
        Ok(Some(id))
    }

    /// Withdraw a notification once its problem is genuinely resolved.
    pub async fn close(&mut self, key: &str, id: u32) -> Result<()> {
        let Some(conn) = &self.conn else {
            return Ok(());
        };
        self.ids.remove(key);

        conn.call_method(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            Some("org.freedesktop.Notifications"),
            "CloseNotification",
            &(id,),
        )
        .await
        .context("closing a desktop notification")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conflict() -> Alert {
        Alert::Conflict {
            pin: "Keepass".into(),
            file: "Timm.kdbx".into(),
            copies: vec!["Timm.conflict-remote-2026-10-08T124530Z.kdbx".into()],
            folder: "/home/t/onedrive-keepass".into(),
            restored: false,
        }
    }

    #[test]
    fn a_conflict_names_the_file_and_both_versions() {
        let alert = conflict();
        assert_eq!(alert.summary(), "OneDrive — conflict in Keepass");

        let body = alert.body();
        assert!(body.contains("Timm.kdbx"), "{body}");
        assert!(
            body.contains("Timm.conflict-remote-2026-10-08T124530Z.kdbx"),
            "{body}"
        );
        assert!(body.contains("Merge them"), "{body}");
        // Nothing alarming about data loss, because none happened.
        assert!(!body.to_lowercase().contains("lost"), "{body}");
    }

    /// When we chose the winner rather than rclone, say so: the user needs to
    /// know the canonical file might not be the one they expect.
    #[test]
    fn a_restored_canonical_name_is_explained() {
        let mut alert = conflict();
        if let Alert::Conflict { restored, .. } = &mut alert {
            *restored = true;
        }
        let body = alert.body();
        assert!(body.contains("identical timestamps"), "{body}");
        assert!(body.contains("larger one"), "{body}");
    }

    #[test]
    fn a_refusal_says_nothing_was_changed() {
        let alert = Alert::Refused {
            pin: "Keepass".into(),
            msg: "refused to run: the .onedrive-check sentinel is missing on one side".into(),
            folder: "/home/t/onedrive-keepass".into(),
        };
        assert_eq!(alert.summary(), "OneDrive — Keepass needs attention");

        let body = alert.body();
        assert!(body.contains("Nothing was changed"), "{body}");
        assert!(body.contains("doctor"), "{body}");
    }

    /// The key is what makes a repeat replace rather than stack, so it must be
    /// stable for the same problem and distinct for different ones.
    #[test]
    fn keys_identify_a_problem_not_an_occurrence() {
        let a = conflict();
        let b = conflict();
        assert_eq!(a.key(), b.key());

        let mut other_file = conflict();
        if let Alert::Conflict { file, .. } = &mut other_file {
            *file = "Other.kdbx".into();
        }
        assert_ne!(a.key(), other_file.key());

        let refused = Alert::Refused {
            pin: "Keepass".into(),
            msg: "x".into(),
            folder: "/f".into(),
        };
        assert_ne!(a.key(), refused.key());
    }

    /// A different *occurrence* of the same conflict — a second copy added —
    /// still has the same key, so it replaces rather than piling up.
    #[test]
    fn more_copies_of_the_same_file_reuse_the_notification() {
        let mut later = conflict();
        if let Alert::Conflict { copies, .. } = &mut later {
            copies.push("Timm.conflict-local-2026-10-08T130000Z.kdbx".into());
        }
        assert_eq!(conflict().key(), later.key());
        assert!(later.body().contains("130000Z"), "{}", later.body());
    }

    /// Without a session bus the daemon must still run; it just cannot show
    /// anything. Connecting must never hang or panic.
    #[tokio::test]
    async fn connecting_degrades_gracefully() {
        let notifier =
            tokio::time::timeout(std::time::Duration::from_secs(20), Notifier::connect()).await;
        assert!(notifier.is_ok(), "connect() must not hang");
    }

    /// Sending with no bus is a no-op rather than an error, so callers do not
    /// have to branch on availability.
    #[tokio::test]
    async fn showing_without_a_bus_is_a_no_op() {
        let mut notifier = Notifier {
            conn: None,
            ids: HashMap::new(),
        };
        assert!(!notifier.is_available());
        assert_eq!(notifier.show(&conflict(), None).await.unwrap(), None);
        notifier
            .close("conflict:Keepass:Timm.kdbx", 7)
            .await
            .unwrap();
    }

    #[test]
    fn the_reassert_interval_is_persistent_without_being_spam() {
        assert!(REASSERT_INTERVAL >= std::time::Duration::from_secs(10 * 60));
        assert!(REASSERT_INTERVAL <= std::time::Duration::from_secs(60 * 60));
    }
}
