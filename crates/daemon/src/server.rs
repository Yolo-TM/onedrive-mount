//! The IPC server.
//!
//! One request per connection, answered and closed. Every connection is handled
//! with a timeout and a bounded read, so a stuck or hostile client cannot hold
//! daemon resources or make it allocate without limit.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use onedrive_rc_core::ipc::{self, MAX_FRAME_BYTES, Request, Response};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;
use tracing::{debug, warn};

use crate::app::App;

/// A client that connects and then says nothing must not pin a task forever.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);

/// Bind the socket, replacing a leftover one from a previous run.
pub fn bind(socket: &Path) -> Result<UnixListener> {
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    // A socket file left behind by a SIGKILLed daemon would make bind fail with
    // EADDRINUSE. We already hold the flock at this point, so no other daemon
    // can be listening and removing it is safe.
    match std::fs::remove_file(socket) {
        Ok(()) => debug!(path = %socket.display(), "removed a stale socket"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(e).with_context(|| format!("removing stale {}", socket.display()));
        }
    }

    UnixListener::bind(socket).with_context(|| format!("binding {}", socket.display()))
}

/// Accept and answer until `shutdown` flips.
pub async fn serve(
    listener: UnixListener,
    socket: PathBuf,
    app: Arc<App>,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    break;
                }
            }
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        let app = Arc::clone(&app);
                        tokio::spawn(async move {
                            if let Err(e) = handle(stream, app).await {
                                debug!(error = %e, "ipc client dropped");
                            }
                        });
                    }
                    Err(e) => {
                        // Transient accept errors (EMFILE under load) must not
                        // kill the only control channel into the daemon.
                        warn!(error = %e, "ipc accept failed");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
        }
    }

    // Tidy up so the next start does not have to treat it as stale.
    let _ = std::fs::remove_file(&socket);
    debug!("ipc server stopped");
}

async fn handle(stream: UnixStream, app: Arc<App>) -> Result<()> {
    tokio::time::timeout(CLIENT_TIMEOUT, async move {
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half).take(MAX_FRAME_BYTES as u64);

        let mut line = String::new();
        reader.read_line(&mut line).await?;

        let response = match ipc::decode_frame::<Request>(line.trim()) {
            Ok(req) => {
                debug!(?req, "ipc request");
                app.handle(req).await
            }
            Err(e) => Response::err(
                onedrive_rc_core::Code::Internal,
                "ipc",
                format!("could not parse request: {e}"),
            ),
        };

        write_half.write_all(&ipc::encode_frame(&response)?).await?;
        write_half.flush().await?;
        Ok::<(), anyhow::Error>(())
    })
    .await
    .context("ipc client timed out")?
}

// `AsyncReadExt::take` needs the trait in scope for the BufReader chain above.
use tokio::io::AsyncReadExt;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_replaces_a_stale_socket_file() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("onedrive-rcd.sock");

        // Simulate what a SIGKILLed daemon leaves behind.
        std::fs::write(&socket, b"stale").unwrap();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _guard = rt.enter();
        let listener = bind(&socket).expect("a stale socket must not block startup");
        drop(listener);
    }

    #[test]
    fn bind_creates_the_runtime_directory() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("nested/onedrive-rcd.sock");

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _guard = rt.enter();
        let _listener = bind(&socket).unwrap();
        assert!(socket.exists());
    }
}
