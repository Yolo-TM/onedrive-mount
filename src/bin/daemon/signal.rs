// Signal handling for daemon lifecycle and sync triggers

use tokio::signal;
use tokio::signal::unix::Signal;

pub async fn wait_for_shutdown() {
    let ctrl_c = async { signal::ctrl_c().await.ok() };

    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

/// Creates the SIGUSR1 signal listener. Call once at daemon startup.
pub fn sync_now_listener() -> Signal {
    signal::unix::signal(signal::unix::SignalKind::user_defined1())
        .expect("failed to install SIGUSR1 handler")
}

/// Waits for the next SIGUSR1 (sent by the GUI "Sync Now" button).
pub async fn wait_for_sync_now(sig: &mut Signal) {
    sig.recv().await;
}
