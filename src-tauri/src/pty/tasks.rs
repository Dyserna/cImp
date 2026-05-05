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
use tracing::{debug, warn};

use crate::processing::{ProcessingEvent, ProcessingLayer};
use crate::settings::SettingsHandle;
use crate::state::{StateSignal, TabId};
use crate::tts::TtsRequest;

/// Tick interval for the processing flush timer. Short enough that the
/// 200ms stability and 500ms max-hold thresholds fire promptly.
const FLUSH_TICK: Duration = Duration::from_millis(50);

/// Output-burst duration before we consider the child to be actually
/// generating. Real responses sustain bytes for seconds; per-keystroke
/// TUI input-box redraws are tens of ms. Anything shorter is treated as
/// churn and ignored.
const CLAUDE_BURST_MIN: Duration = Duration::from_millis(1000);

/// Quiet interval that closes a burst. After this much silence we fire
/// ClaudeOutputStopped (if Started was fired) and reset the burst tracker.
const CLAUDE_QUIET: Duration = Duration::from_millis(500);

#[derive(serde::Serialize, Clone)]
struct PtyExitPayload {
    tab: TabId,
    exit: String,
}

/// Reader task. Lives on the blocking pool because PTY reads block on most platforms.
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
/// and a steady 50 ms flush tick, and dispatches resulting events. Each
/// emitted state signal is tagged with the owning tab so the manager can
/// route it to the correct per-tab `TabState`.
#[allow(clippy::too_many_arguments)]
pub fn spawn_processor(
    tab: TabId,
    mut rx: mpsc::Receiver<Vec<u8>>,
    channel: Channel<String>,
    tts_segments: mpsc::Sender<TtsRequest>,
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

        let mut output_active = false;
        let mut burst_start: Option<tokio::time::Instant> = None;
        let mut last_byte_time: Option<tokio::time::Instant> = None;

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    debug!(?tab, "pty processor: cancelled");
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
                            if dispatch_events(tab, events, &channel, &tts_segments).await.is_err() {
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
                            debug!(?tab, "pty processor: bytes channel closed");
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
                    if dispatch_events(tab, events, &channel, &tts_segments).await.is_err() {
                        break;
                    }
                    if saw_terminal_bytes {
                        let now = tokio::time::Instant::now();
                        if burst_start.is_none() {
                            burst_start = Some(now);
                        }
                        last_byte_time = Some(now);
                    }

                    if !output_active {
                        if let Some(start) = burst_start {
                            if start.elapsed() >= CLAUDE_BURST_MIN {
                                output_active = true;
                                let _ = state_signals
                                    .try_send(StateSignal::ClaudeOutputStarted { tab });
                            }
                        }
                    }

                    if let Some(t) = last_byte_time {
                        if t.elapsed() >= CLAUDE_QUIET {
                            if output_active {
                                output_active = false;
                                let _ = state_signals
                                    .try_send(StateSignal::ClaudeOutputStopped { tab });
                            }
                            burst_start = None;
                            last_byte_time = None;
                        }
                    }
                }
            }
        }
        debug!(?tab, "pty processor task exiting");
    });
}

async fn dispatch_events(
    tab: TabId,
    events: Vec<ProcessingEvent>,
    channel: &Channel<String>,
    tts_segments: &mpsc::Sender<TtsRequest>,
) -> Result<(), ()> {
    for ev in events {
        match ev {
            ProcessingEvent::TerminalBytes(bytes) => {
                if bytes.is_empty() {
                    continue;
                }
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
                debug!(target: "tts_stub", ?tab, text = %text, "extracted TTS segment");
                let req = TtsRequest::Synthesize { tab, text };
                if tts_segments.send(req).await.is_err() {
                    debug!("pty processor: TTS segment channel closed (worker not running)");
                }
            }
            ProcessingEvent::Stalled => {
                warn!(?tab, "pty processor: stalled");
            }
        }
    }
    Ok(())
}

/// Waiter task. Blocks on child.wait(), emits `pty-exit` on the AppHandle, and
/// cancels sibling tasks so the reader/processor unwind cleanly.
pub fn spawn_waiter(
    tab: TabId,
    mut child: Box<dyn Child + Send + Sync>,
    app: AppHandle,
    cancel: CancellationToken,
    state_signals: mpsc::Sender<StateSignal>,
) {
    tokio::task::spawn_blocking(move || {
        let exit = child.wait();
        cancel.cancel();
        let exit_str = match exit {
            Ok(status) => format!("{:?}", status),
            Err(e) => format!("error: {}", e),
        };
        let _ = state_signals.try_send(StateSignal::SubprocessExited { tab });
        if let Err(e) = app.emit(
            "pty-exit",
            PtyExitPayload {
                tab,
                exit: exit_str,
            },
        ) {
            warn!(error = %e, "failed to emit pty-exit");
        }
        debug!(?tab, "pty waiter task exiting");
    });
}
