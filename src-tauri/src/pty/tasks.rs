use std::collections::HashSet;
use std::io::Read;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use base64::prelude::*;
use portable_pty::Child;
use tauri::ipc::Channel;
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::processing::{ProcessingEvent, ProcessingLayer};
use crate::settings::SettingsHandle;
use crate::state::StateSignal;

/// Tick interval for the processing flush timer. Short enough that the
/// 200ms stability and 500ms max-hold thresholds fire promptly.
const FLUSH_TICK: Duration = Duration::from_millis(50);

/// Output-burst duration before we consider claude to be actually
/// generating. Real responses sustain bytes for seconds; per-keystroke
/// TUI input-box redraws are tens of ms; a chain of TUI redraws caused
/// by mid-drag window jitter rarely lasts beyond ~1s. Anything shorter
/// is treated as churn and ignored.
const CLAUDE_BURST_MIN: Duration = Duration::from_millis(1000);

/// Quiet interval that closes a burst. After this much silence we fire
/// ClaudeOutputStopped (if Started was fired) and reset the burst tracker.
const CLAUDE_QUIET: Duration = Duration::from_millis(500);

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
                    // Log raw PTY input (pre-processing) when `pty_in` target
                    // is enabled. Lets us see what claude actually outputs vs
                    // what reaches xterm after marker stripping.
                    if tracing::enabled!(target: "pty_in", tracing::Level::DEBUG) {
                        let pretty: String = String::from_utf8_lossy(&buf[..n])
                            .chars()
                            .map(|c| if c.is_control() && c != '\n' && c != '\r' && c != '\t' {
                                format!("\\x{:02x}", c as u32)
                            } else {
                                c.to_string()
                            })
                            .collect();
                        debug!(target: "pty_in", len = n, bytes = %pretty);
                    }
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
/// - `TtsSegment` → forwarded to the TTS worker via the segment mpsc.
/// - `Stalled` → diagnostic warning.
///
/// Single-owner pattern: the layer is `&mut` only inside this task, so no
/// extra locking is required.
pub fn spawn_processor(
    mut rx: mpsc::Receiver<Vec<u8>>,
    channel: Channel<String>,
    tts_segments: mpsc::Sender<String>,
    cancel: CancellationToken,
    user_typed_tts: Arc<StdMutex<HashSet<String>>>,
    state_signals: mpsc::Sender<StateSignal>,
    settings: SettingsHandle,
) {
    tokio::spawn(async move {
        let mut layer = ProcessingLayer::with_user_typed_filter(user_typed_tts);
        layer.set_max_hold(Duration::from_millis(
            settings.current().processing.max_hold_ms as u64,
        ));
        let mut settings_rx = settings.subscribe();
        let mut tick = tokio::time::interval(FLUSH_TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        // Output-activity tracking. Two timers:
        //   - `burst_start`: when bytes first appeared in the current
        //     burst. If the burst sustains for CLAUDE_BURST_MIN, we
        //     consider claude to be actually generating and fire Started.
        //     This filters out per-keystroke TUI input-box redraws (~50
        //     bytes over a few ms) from real responses (seconds of
        //     continuous tokens).
        //   - `last_byte_time`: most recent byte. If it's been quiet for
        //     CLAUDE_QUIET, the burst is over — fire Stopped if Started
        //     was fired, then reset both timers.
        let mut output_active = false;
        let mut burst_start: Option<tokio::time::Instant> = None;
        let mut last_byte_time: Option<tokio::time::Instant> = None;

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    debug!("pty processor: cancelled");
                    break;
                }
                changed = settings_rx.recv() => {
                    match changed {
                        Ok(s) => {
                            layer.set_max_hold(Duration::from_millis(
                                s.processing.max_hold_ms as u64,
                            ));
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            debug!("pty processor: settings channel closed");
                        }
                    }
                }
                maybe = rx.recv() => {
                    match maybe {
                        Some(bytes) => {
                            let events = layer.ingest(&bytes);
                            let saw_terminal_bytes = events.iter().any(|e| matches!(
                                e,
                                ProcessingEvent::TerminalBytes(b) if !b.is_empty()
                            ));
                            if dispatch_events(events, &channel, &tts_segments).await.is_err() {
                                break;
                            }
                            if saw_terminal_bytes {
                                let now = tokio::time::Instant::now();
                                if burst_start.is_none() {
                                    burst_start = Some(now);
                                }
                                last_byte_time = Some(now);
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
                    let saw_terminal_bytes = events.iter().any(|e| matches!(
                        e,
                        ProcessingEvent::TerminalBytes(b) if !b.is_empty()
                    ));
                    if dispatch_events(events, &channel, &tts_segments).await.is_err() {
                        break;
                    }
                    if saw_terminal_bytes {
                        let now = tokio::time::Instant::now();
                        if burst_start.is_none() {
                            burst_start = Some(now);
                        }
                        last_byte_time = Some(now);
                    }

                    // Promote a sustained burst to ClaudeOutputStarted.
                    if !output_active {
                        if let Some(start) = burst_start {
                            if start.elapsed() >= CLAUDE_BURST_MIN {
                                output_active = true;
                                let _ = state_signals
                                    .try_send(StateSignal::ClaudeOutputStarted);
                            }
                        }
                    }

                    // Quiet → close the burst.
                    if let Some(t) = last_byte_time {
                        if t.elapsed() >= CLAUDE_QUIET {
                            if output_active {
                                output_active = false;
                                let _ = state_signals
                                    .try_send(StateSignal::ClaudeOutputStopped);
                            }
                            burst_start = None;
                            last_byte_time = None;
                        }
                    }
                }
            }
        }
        debug!("pty processor task exiting");
    });
}

async fn dispatch_events(
    events: Vec<ProcessingEvent>,
    channel: &Channel<String>,
    tts_segments: &mpsc::Sender<String>,
) -> Result<(), ()> {
    for ev in events {
        match ev {
            ProcessingEvent::TerminalBytes(bytes) => {
                if bytes.is_empty() {
                    continue;
                }
                // Debug visibility: dump emitted bytes as a printable string with
                // ANSI/CTRL escapes shown, so we can compare against what xterm
                // renders and pinpoint corruption.
                if tracing::enabled!(target: "pty_emit", tracing::Level::DEBUG) {
                    let pretty: String = String::from_utf8_lossy(&bytes)
                        .chars()
                        .map(|c| if c.is_control() && c != '\n' && c != '\r' && c != '\t' {
                            format!("\\x{:02x}", c as u32)
                        } else {
                            c.to_string()
                        })
                        .collect();
                    debug!(target: "pty_emit", len = bytes.len(), bytes = %pretty);
                }
                let encoded = BASE64_STANDARD.encode(&bytes);
                if let Err(e) = channel.send(encoded) {
                    warn!(error = %e, "pty processor: terminal channel send failed");
                    return Err(());
                }
            }
            ProcessingEvent::TtsSegment(text) => {
                info!(target: "tts_stub", text = %text, "extracted TTS segment");
                if tts_segments.send(text).await.is_err() {
                    debug!("pty processor: TTS segment channel closed (worker not running)");
                }
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
    state_signals: mpsc::Sender<StateSignal>,
) {
    tokio::task::spawn_blocking(move || {
        let exit = child.wait();
        cancel.cancel();
        let payload = match exit {
            Ok(status) => format!("{:?}", status),
            Err(e) => format!("error: {}", e),
        };
        // Push the avatar into Error before the frontend hears about the
        // exit — the IPC event below is informational, the state signal
        // actually drives the UI.
        let _ = state_signals.try_send(StateSignal::SubprocessExited);
        if let Err(e) = app.emit("pty-exit", payload) {
            warn!(error = %e, "failed to emit pty-exit");
        }
        debug!("pty waiter task exiting");
    });
}
