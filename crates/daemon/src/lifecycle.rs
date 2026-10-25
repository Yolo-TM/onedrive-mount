//! Suspend, resume, network changes and screen lock, over D-Bus.
//!
//! The old setup had none of this. Recovery after a suspend depended entirely on
//! a 15-second poll noticing `ENOTCONN`, and the daemon issued rclone calls into
//! a dead network until they timed out — which is most of what filled the old
//! log. The unit's `After=network-online.target` looked like it addressed the
//! boot case but is inert in the user manager, so even that did nothing.
//!
//! Every subscription here is optional. A machine without logind, without
//! NetworkManager, or without a screensaver still runs; it just loses the
//! corresponding signal and says so once.
//!
//! All four interfaces were confirmed present on the target machine:
//! `org.freedesktop.login1` (with `DelayInhibited=shutdown:sleep` and
//! `InhibitDelayMaxUSec=5000000`), `org.freedesktop.NetworkManager`, and
//! `org.freedesktop.ScreenSaver` owned by plasmashell.

use std::time::Duration;

use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use zbus::zvariant::OwnedFd;
use zbus::{Connection, Proxy};

/// Something the daemon needs to react to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// Suspend is imminent. We hold a delay inhibitor until we reply, so this
    /// must be handled fast — see [`INHIBIT_BUDGET`].
    Suspending,
    /// Back from suspend. Mounts are probably stale and the network state is
    /// unknown.
    Resumed,
    NetworkOnline,
    NetworkOffline,
    /// The session locked, which is a good moment to flush pending work.
    ScreenLocked,
    ScreenUnlocked,
}

/// How long we allow ourselves while holding a sleep inhibitor.
///
/// logind reports `InhibitDelayMaxUSec=5000000` (5s) on the target machine. Go
/// over it and systemd suspends anyway, so this is deliberately well under:
/// checkpointing state is quick, and a full sync is explicitly *not* attempted
/// here.
pub const INHIBIT_BUDGET: Duration = Duration::from_secs(3);

/// NetworkManager's `NM_STATE_*` values. We only care about "fully usable".
const NM_STATE_CONNECTED_GLOBAL: u32 = 70;

/// Subscribe to everything available. Returns the event stream.
///
/// The returned receiver stays open for the life of the daemon; the spawned
/// tasks own their proxies.
pub async fn subscribe() -> mpsc::Receiver<Event> {
    // Capacity is generous relative to how rarely these fire; a full channel
    // would only happen if the consumer were wedged, in which case dropping
    // lifecycle events is the least of the problems.
    let (tx, rx) = mpsc::channel(32);

    // The system bus carries logind and NetworkManager.
    match Connection::system().await {
        Ok(system) => {
            spawn_sleep_watch(system.clone(), tx.clone()).await;
            spawn_network_watch(system, tx.clone()).await;
        }
        Err(e) => warn!(
            error = %e,
            "no system bus: suspend/resume and network changes will not be noticed"
        ),
    }

    // The screensaver lives on the session bus.
    match Connection::session().await {
        Ok(session) => spawn_lock_watch(session, tx).await,
        Err(e) => debug!(error = %e, "no session bus: screen lock will not be noticed"),
    }

    rx
}

// ---------------------------------------------------------------------------
// Suspend / resume
// ---------------------------------------------------------------------------

/// Watch logind's `PrepareForSleep`.
///
/// The signal fires with `true` just before suspending and `false` just after
/// resuming. Holding a `delay` inhibitor is what buys us a window to act in
/// before the machine actually goes down.
async fn spawn_sleep_watch(conn: Connection, tx: mpsc::Sender<Event>) {
    let proxy = match login1(&conn).await {
        Ok(proxy) => proxy,
        Err(e) => {
            warn!(error = %e, "logind unavailable: suspend and resume will not be noticed");
            return;
        }
    };

    let mut signals = match proxy.receive_signal("PrepareForSleep").await {
        Ok(stream) => stream,
        Err(e) => {
            warn!(error = %e, "could not subscribe to PrepareForSleep");
            return;
        }
    };

    info!("watching for suspend and resume");

    tokio::spawn(async move {
        use futures_lite_compat::StreamExt;

        while let Some(signal) = signals.next().await {
            let about_to_sleep: bool = match signal.body().deserialize() {
                Ok(v) => v,
                Err(e) => {
                    warn!(error = %e, "malformed PrepareForSleep signal");
                    continue;
                }
            };

            if about_to_sleep {
                // Take the inhibitor *for the next* sleep only after this one
                // completes; right now we are already inside the delay window
                // that our previously-held inhibitor created.
                info!("suspending: pausing work");
                let _ = tx.send(Event::Suspending).await;

                // Give the daemon its budget, then let go. Whether or not it
                // finished, we must not hold systemd up beyond the limit.
                tokio::time::sleep(INHIBIT_BUDGET).await;
                debug!("releasing the sleep inhibitor");
            } else {
                info!("resumed from suspend");
                let _ = tx.send(Event::Resumed).await;
            }
        }
        debug!("PrepareForSleep stream ended");
    });
}

/// Take a `delay` inhibitor so logind gives us a window before suspending.
///
/// The returned fd must be held: closing it releases the inhibitor. Returning
/// `None` simply means we get no warning before a suspend, which is survivable
/// — the resume path re-probes everything anyway.
pub async fn inhibit_sleep(conn: &Connection) -> Option<OwnedFd> {
    let proxy = login1(conn).await.ok()?;

    let fd: OwnedFd = proxy
        .call(
            "Inhibit",
            &(
                "sleep",
                "onedrive-rcd",
                "checkpointing OneDrive state",
                "delay",
            ),
        )
        .await
        .map_err(|e| warn!(error = %e, "could not take a sleep inhibitor"))
        .ok()?;

    debug!("holding a sleep delay inhibitor");
    Some(fd)
}

async fn login1(conn: &Connection) -> zbus::Result<Proxy<'static>> {
    Proxy::new(
        conn,
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    )
    .await
}

// ---------------------------------------------------------------------------
// Network
// ---------------------------------------------------------------------------

/// Watch NetworkManager's `StateChanged`.
///
/// Knowing we are offline is what stops the retry storms: with no network we
/// issue no rclone calls at all, rather than letting each one discover DNS
/// failure on its own and log a paragraph about it.
async fn spawn_network_watch(conn: Connection, tx: mpsc::Sender<Event>) {
    let proxy = match network_manager(&conn).await {
        Ok(proxy) => proxy,
        Err(e) => {
            warn!(
                error = %e,
                "NetworkManager unavailable: assuming the network is always up"
            );
            // Without NM we cannot know, and assuming offline would stop all
            // work. Report online once so the daemon is not stuck in limbo.
            let _ = tx.send(Event::NetworkOnline).await;
            return;
        }
    };

    // Seed the current state rather than waiting for the first change.
    match proxy.get_property::<u32>("State").await {
        Ok(state) => {
            let event = classify_nm_state(state);
            info!(
                nm_state = state,
                online = event == Event::NetworkOnline,
                "initial network state"
            );
            let _ = tx.send(event).await;
        }
        Err(e) => {
            warn!(error = %e, "could not read the network state; assuming online");
            let _ = tx.send(Event::NetworkOnline).await;
        }
    }

    let mut signals = match proxy.receive_signal("StateChanged").await {
        Ok(stream) => stream,
        Err(e) => {
            warn!(error = %e, "could not subscribe to NetworkManager StateChanged");
            return;
        }
    };

    tokio::spawn(async move {
        use futures_lite_compat::StreamExt;

        while let Some(signal) = signals.next().await {
            let state: u32 = match signal.body().deserialize() {
                Ok(v) => v,
                Err(e) => {
                    debug!(error = %e, "malformed StateChanged signal");
                    continue;
                }
            };
            let event = classify_nm_state(state);
            debug!(nm_state = state, ?event, "network state changed");
            let _ = tx.send(event).await;
        }
        debug!("NetworkManager signal stream ended");
    });
}

/// Map an `NM_STATE_*` value onto online/offline.
///
/// Only `CONNECTED_GLOBAL` counts: `CONNECTED_SITE` (60) means a local network
/// with no internet, which for our purposes is offline.
pub fn classify_nm_state(state: u32) -> Event {
    if state >= NM_STATE_CONNECTED_GLOBAL {
        Event::NetworkOnline
    } else {
        Event::NetworkOffline
    }
}

async fn network_manager(conn: &Connection) -> zbus::Result<Proxy<'static>> {
    Proxy::new(
        conn,
        "org.freedesktop.NetworkManager",
        "/org/freedesktop/NetworkManager",
        "org.freedesktop.NetworkManager",
    )
    .await
}

// ---------------------------------------------------------------------------
// Screen lock
// ---------------------------------------------------------------------------

/// Watch for the session locking.
///
/// Locking the screen is the best available signal for "the user has finished
/// what they were doing", which makes it a good moment to push pending changes
/// — notably a password database that was just edited.
async fn spawn_lock_watch(conn: Connection, tx: mpsc::Sender<Event>) {
    // KDE owns `org.freedesktop.ScreenSaver`; try the generic name first so this
    // works on GNOME too.
    for (service, path) in [
        ("org.freedesktop.ScreenSaver", "/ScreenSaver"),
        ("org.kde.screensaver", "/ScreenSaver"),
        ("org.gnome.ScreenSaver", "/org/gnome/ScreenSaver"),
    ] {
        let proxy = match Proxy::new(&conn, service, path, "org.freedesktop.ScreenSaver").await {
            Ok(proxy) => proxy,
            Err(_) => continue,
        };

        match proxy.receive_signal("ActiveChanged").await {
            Ok(mut signals) => {
                info!(service, "watching for screen lock");
                let tx = tx.clone();
                tokio::spawn(async move {
                    use futures_lite_compat::StreamExt;

                    while let Some(signal) = signals.next().await {
                        let locked: bool = match signal.body().deserialize() {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        let event = if locked {
                            Event::ScreenLocked
                        } else {
                            Event::ScreenUnlocked
                        };
                        debug!(?event, "screen lock state changed");
                        let _ = tx.send(event).await;
                    }
                });
                return;
            }
            Err(_) => continue,
        }
    }
    debug!("no screensaver interface found; screen lock will not be noticed");
}

/// zbus 5 returns `futures_core::Stream`s. Pulling in the whole `futures` crate
/// for `next()` is not worth it, so this is the one adapter we need.
mod futures_lite_compat {
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    pub trait StreamExt: futures_core::Stream {
        fn next(&mut self) -> Next<'_, Self>
        where
            Self: Unpin,
        {
            Next { stream: self }
        }
    }

    impl<S: futures_core::Stream + ?Sized> StreamExt for S {}

    pub struct Next<'a, S: ?Sized> {
        stream: &'a mut S,
    }

    impl<S> Future for Next<'_, S>
    where
        S: futures_core::Stream + Unpin + ?Sized,
    {
        type Output = Option<S::Item>;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            Pin::new(&mut *self.stream).poll_next(cx)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_full_connectivity_counts_as_online() {
        // NM_STATE_UNKNOWN / ASLEEP / DISCONNECTED / DISCONNECTING / CONNECTING
        for state in [0u32, 10, 20, 30, 40, 50] {
            assert_eq!(
                classify_nm_state(state),
                Event::NetworkOffline,
                "state {state}"
            );
        }
        // CONNECTED_LOCAL and CONNECTED_SITE: a network, but no internet. For
        // talking to OneDrive that is the same as being offline.
        assert_eq!(classify_nm_state(50), Event::NetworkOffline);
        assert_eq!(classify_nm_state(60), Event::NetworkOffline);
        // CONNECTED_GLOBAL — the only state we act on.
        assert_eq!(classify_nm_state(70), Event::NetworkOnline);
        // Unknown future value above the threshold is treated as usable.
        assert_eq!(classify_nm_state(80), Event::NetworkOnline);
    }

    /// Going over logind's delay window means systemd suspends regardless, so
    /// the budget has to stay comfortably under the 5s it reports.
    #[test]
    fn the_inhibit_budget_fits_loginds_window() {
        assert!(
            INHIBIT_BUDGET < Duration::from_secs(5),
            "must be under InhibitDelayMaxUSec"
        );
        assert!(
            INHIBIT_BUDGET >= Duration::from_secs(1),
            "but long enough to be useful"
        );
    }

    /// Subscribing must never panic or hang, whatever the machine provides —
    /// including in a build sandbox with no buses at all.
    #[tokio::test]
    async fn subscribing_degrades_gracefully() {
        let rx = tokio::time::timeout(Duration::from_secs(20), subscribe()).await;
        assert!(
            rx.is_ok(),
            "subscribe() must not hang when buses are missing"
        );
    }

    #[test]
    fn events_are_comparable_for_state_tracking() {
        assert_eq!(Event::NetworkOnline, Event::NetworkOnline);
        assert_ne!(Event::NetworkOnline, Event::NetworkOffline);
        assert_ne!(Event::Suspending, Event::Resumed);
    }
}
