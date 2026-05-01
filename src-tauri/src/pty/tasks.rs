use std::io::Read;

use base64::prelude::*;
use portable_pty::Child;
use tauri::ipc::Channel;
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

/// Reader task. Lives on the blocking pool because PTY reads block on most platforms.
/// Sends raw byte chunks to an mpsc receiver consumed by `spawn_forwarder`.
pub fn spawn_reader(
    mut reader: Box<dyn Read + Send>,
    tx: mpsc::Sender<Vec<u8>>,
    cancel: CancellationToken,
) {
    tokio::task::spawn_blocking(move || {
        let mut buf = [0u8; 4096];
        loop {
            if cancel.is_cancelled() {
                debug!("pty reader: cancelled");
                break;
            }
            match reader.read(&mut buf) {
                Ok(0) => {
                    debug!("pty reader: EOF");
                    break;
                }
                Ok(n) => {
                    if tx.blocking_send(buf[..n].to_vec()).is_err() {
                        debug!("pty reader: forwarder dropped");
                        break;
                    }
                }
                Err(e) => {
                    if cancel.is_cancelled() {
                        break;
                    }
                    warn!(error = %e, "pty reader: read error");
                    break;
                }
            }
        }
        debug!("pty reader task exiting");
    });
}

/// Forwarder task. Single emit site for `pty-output`. M2 replaces the body
/// with `processing_layer.ingest(bytes)` and dispatches `ProcessingEvent`s.
pub fn spawn_forwarder(
    mut rx: mpsc::Receiver<Vec<u8>>,
    channel: Channel<String>,
    cancel: CancellationToken,
) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    debug!("pty forwarder: cancelled");
                    break;
                }
                maybe = rx.recv() => {
                    match maybe {
                        Some(bytes) => {
                            let encoded = BASE64_STANDARD.encode(&bytes);
                            if let Err(e) = channel.send(encoded) {
                                warn!(error = %e, "pty forwarder: channel send failed");
                                break;
                            }
                        }
                        None => {
                            debug!("pty forwarder: bytes channel closed");
                            break;
                        }
                    }
                }
            }
        }
        debug!("pty forwarder task exiting");
    });
}

/// Waiter task. Blocks on child.wait(), emits `pty-exit` on the AppHandle, and
/// cancels sibling tasks so the reader/forwarder unwind cleanly.
pub fn spawn_waiter(
    mut child: Box<dyn Child + Send + Sync>,
    app: AppHandle,
    cancel: CancellationToken,
) {
    tokio::task::spawn_blocking(move || {
        let exit = child.wait();
        cancel.cancel();
        let payload = match exit {
            Ok(status) => format!("{:?}", status),
            Err(e) => format!("error: {}", e),
        };
        if let Err(e) = app.emit("pty-exit", payload) {
            warn!(error = %e, "failed to emit pty-exit");
        }
        debug!("pty waiter task exiting");
    });
}
