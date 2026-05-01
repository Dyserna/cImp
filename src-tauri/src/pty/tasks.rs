use std::io::Read;
use std::time::Duration;

use base64::prelude::*;
use portable_pty::Child;
use tauri::ipc::Channel;
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::processing::{ProcessingEvent, ProcessingLayer};

/// Tick interval for the processing flush timer. Short enough that the
/// 200ms stability and 500ms max-hold thresholds fire promptly.
const FLUSH_TICK: Duration = Duration::from_millis(50);

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

/// Processor task. Owns a `ProcessingLayer`, drives it from the PTY byte stream
/// and a steady 50 ms flush tick, and dispatches resulting events:
///
/// - `TerminalBytes` → base64-encoded and pushed through the Tauri Channel.
/// - `TtsSegment` → logged at INFO with `target = "tts_stub"` (M3 swaps this
///   for the real synthesizer).
/// - `Stalled` → diagnostic warning.
///
/// Single-owner pattern: the layer is `&mut` only inside this task, so no
/// extra locking is required.
pub fn spawn_processor(
    mut rx: mpsc::Receiver<Vec<u8>>,
    channel: Channel<String>,
    cancel: CancellationToken,
) {
    tokio::spawn(async move {
        let mut layer = ProcessingLayer::new();
        let mut tick = tokio::time::interval(FLUSH_TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    debug!("pty processor: cancelled");
                    break;
                }
                maybe = rx.recv() => {
                    match maybe {
                        Some(bytes) => {
                            let events = layer.ingest(&bytes);
                            if dispatch_events(events, &channel).is_err() {
                                break;
                            }
                        }
                        None => {
                            debug!("pty processor: bytes channel closed");
                            break;
                        }
                    }
                }
                _ = tick.tick() => {
                    let events = layer.flush_pending();
                    if dispatch_events(events, &channel).is_err() {
                        break;
                    }
                }
            }
        }
        debug!("pty processor task exiting");
    });
}

fn dispatch_events(events: Vec<ProcessingEvent>, channel: &Channel<String>) -> Result<(), ()> {
    for ev in events {
        match ev {
            ProcessingEvent::TerminalBytes(bytes) => {
                if bytes.is_empty() {
                    continue;
                }
                let encoded = BASE64_STANDARD.encode(&bytes);
                if let Err(e) = channel.send(encoded) {
                    warn!(error = %e, "pty processor: channel send failed");
                    return Err(());
                }
            }
            ProcessingEvent::TtsSegment(text) => {
                info!(target: "tts_stub", text = %text, "would speak");
            }
            ProcessingEvent::Stalled => {
                warn!("pty processor: stalled");
            }
        }
    }
    Ok(())
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
