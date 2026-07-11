use onedrive_mount::{paths::status_file, status::DaemonStatus};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::warn;

pub fn start(
    mut rx: watch::Receiver<DaemonStatus>,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let path = status_file();

        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                Ok(()) = rx.changed() => {
                    let snapshot = rx.borrow_and_update().clone();
                    if let Err(e) = snapshot.save(&path) {
                        warn!(error = %e, "failed to write status file");
                    }
                }
            }
        }

        let mut snapshot = rx.borrow().clone();
        for remote in &mut snapshot.remotes {
            remote.mount = onedrive_mount::status::MountState::Unmounted;
            for rule in &mut remote.sync_rules {
                rule.next_sync = None;
                if matches!(rule.state, onedrive_mount::status::SyncState::Running) {
                    rule.state = onedrive_mount::status::SyncState::Idle;
                }
            }
        }
        let _ = snapshot.save(&path);
    })
}
