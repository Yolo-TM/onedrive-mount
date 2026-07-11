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

pub fn sync_now_listener() -> Signal {
    signal::unix::signal(signal::unix::SignalKind::user_defined1())
        .expect("failed to install SIGUSR1 handler")
}

pub async fn wait_for_sync_now(sig: &mut Signal) {
    sig.recv().await;
}
