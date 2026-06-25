use std::collections::{HashSet, VecDeque};
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

use crate::processing::permission::{
    PatternKind, PatternTransition, PermissionDetector, PermissionPattern,
};
use crate::processing::{ProcessingEvent, ProcessingLayer};
use super::manager::ProcessorControl;
use crate::settings::SettingsHandle;
use crate::state::{StateSignal, TabId, TabKind};
use crate::tts::TtsRequest;

/// Tail size scanned by the permission detector after each ingest/flush.
/// Large enough for multi-line prompt UIs, small enough not to cost real
/// CPU on every tick.
const PERMISSION_SCAN_TAIL: usize = 1000;

/// Tick interval for the processing flush timer. Short enough that the
/// 200ms stability and 500ms max-hold thresholds fire promptly.
const FLUSH_TICK: Duration = Duration::from_millis(50);

/// Claude-activity (avatar Thinking) detection is primarily content-based:
/// the `claude_working` pattern matches Claude's on-screen busy footer
/// (`esc to interrupt`), which is present for exactly as long as a request
/// is in flight. While that marker shows we hold `output_active` regardless
/// of the byte stream, so a thinking pause (Claude routinely emits nothing
/// for >0.5s mid-response) no longer collapses the avatar to Idle. The two
/// timers below are backstops only.
///
/// Byte-burst duration before the timer alone considers the child to be
/// generating — the fallback path for the rare response that never paints
/// the marker. Real responses sustain bytes for seconds; per-keystroke TUI
/// redraws are tens of ms, so anything shorter is churn.
const CLAUDE_BURST_MIN: Duration = Duration::from_millis(1000);

/// Quiet interval that closes a burst once the marker is gone. Only fires
/// `ClaudeOutputStopped` when `claude_working` is NOT currently matched —
/// the marker, not this timer, decides Idle while Claude is working.
const CLAUDE_QUIET: Duration = Duration::from_millis(500);

/// Safety valve: if the working marker is somehow still matched but the
/// byte stream has been completely silent this long, treat it as stale
/// (e.g. ghost footer text left in the cell grid) and release Idle. The
/// live spinner repaints its elapsed-second counter ~once/sec, so a true
/// in-flight request keeps bytes flowing well under this window.
const CLAUDE_WORKING_STALE: Duration = Duration::from_secs(6);

#[derive(serde::Serialize, Clone)]
struct PtyExitPayload {
    tab: TabId,
    exit: String,
}

/// Reader task. Lives on the blocking pool because PTY reads block on most platforms.
///
/// V1.4-04 D: the reader also feeds the per-tab scrollback ring buffer.
/// `scrollback` is shared with `PtyManager` so persistence and the
/// `pty_get_scrollback` Tauri command can snapshot the same bytes. The
/// ring is bounded at `scrollback_cap` — surplus is dropped from the
/// front via `pop_front`. Lock contention is minimal: only this reader
/// writes; the snapshot/persist paths are rare reads. `StdMutex` keeps
/// every critical section tight (no awaits) and avoids the awkward
/// blocking-pool-vs-tokio-mutex interplay.
pub fn spawn_reader(
    mut reader: Box<dyn Read + Send>,
    tx: mpsc::Sender<Vec<u8>>,
    cancel: CancellationToken,
    scrollback: Arc<StdMutex<VecDeque<u8>>>,
    scrollback_cap: usize,
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
                    // V1.4-04 D: append to the cross-restart ring
                    // before forwarding. Doing it before the
                    // `blocking_send` means the ring captures every
                    // byte even if the processor is back-pressured;
                    // doing it after would risk losing bytes if the
                    // forwarder drops mid-read.
                    if scrollback_cap > 0 {
                        if let Ok(mut ring) = scrollback.lock() {
                            ring.extend(&buf[..n]);
                            crate::pty::scrollback::trim_ring(&mut ring, scrollback_cap);
                        }
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
///
/// Per-kind gating (Phase 3 of MILESTONE-V3-01):
/// - AI tabs: full pipeline — permission/question/working detection,
///   claude-output activity tracking, TTS dispatch.
/// - Shell tabs: bypass all three. The vte parser still runs (xterm.js needs
///   correctly-rendered bytes) but no TTS request is ever sent, no permission
///   pattern is scanned, and `ClaudeOutputStarted/Stopped` never fires for
///   them.
#[allow(clippy::too_many_arguments)]
pub fn spawn_processor(
    tab: TabId,
    mut rx: mpsc::Receiver<Vec<u8>>,
    channel: Channel<String>,
    mut control_rx: mpsc::Receiver<ProcessorControl>,
    tts_segments: mpsc::Sender<TtsRequest>,
    cancel: CancellationToken,
    user_typed_tts: Arc<StdMutex<HashSet<String>>>,
    state_signals: mpsc::Sender<StateSignal>,
    settings: SettingsHandle,
    patterns: Arc<Vec<PermissionPattern>>,
) {
    tokio::spawn(async move {
        // V1.4-03: `channel` is `mut` so the `ProcessorControl::ChannelChange`
        // arm can swap it out when the JS-side xterm is recreated for a
        // renderer-flip. Bytes pending in `rx` during the swap remain there
        // (mpsc is FIFO; cancel-safe across select polls) and dispatch to
        // the new channel on the next iteration.
        let mut channel = channel;

        let kind = tab.kind();
        let is_shell = matches!(kind, TabKind::Shell);

        let mut layer = ProcessingLayer::with_user_typed_filter(user_typed_tts);
        {
            let s = settings.current();
            layer.set_max_hold(Duration::from_millis(s.processing.max_hold_ms as u64));
            layer.set_speak_all(s.tab_speak_all_output(tab.as_str()));
        }
        let mut settings_rx = settings.subscribe();
        let mut tick = tokio::time::interval(FLUSH_TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        // Detector. AI tabs use the user-editable patterns loaded from
        // patterns.json at startup (subscription and local Claude both run
        // the same binary, so they share the pattern set); Shell tabs get
        // an empty pattern list, and the detector itself is never invoked
        // for them anyway — see `is_shell` below.
        let detector_patterns: Vec<PermissionPattern> = match kind {
            TabKind::AiTool => (*patterns).clone(),
            TabKind::Shell => Vec::new(),
        };
        let mut detector = PermissionDetector::new(detector_patterns);

        let mut output_active = false;
        let mut burst_start: Option<tokio::time::Instant> = None;
        let mut last_byte_time: Option<tokio::time::Instant> = None;
        // True while the `claude_working` marker ("esc to interrupt") is on
        // screen. Authoritative for the avatar's Thinking state; the byte
        // timers above are only a fallback. Updated by run_permission_check.
        let mut working_active = false;

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
                            // Pick up a live toggle of "speak all output" for
                            // this tab — no PTY restart needed.
                            layer.set_speak_all(s.tab_speak_all_output(tab.as_str()));
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            debug!("pty processor: settings channel closed");
                        }
                    }
                }
                ctrl = control_rx.recv() => {
                    match ctrl {
                        Some(ProcessorControl::ChannelChange(next)) => {
                            // V1.4-03: swap output channel without
                            // restarting the PTY. No replay here — bytes
                            // that arrived during the rebind window are
                            // queued in `rx` and dispatch against the
                            // new channel on the next iteration.
                            // Frontend handles visual continuity via the
                            // serialize-snapshot replay.
                            debug!(?tab, "pty processor: channel rebind");
                            channel = next;
                        }
                        None => {
                            // Sender dropped — only happens if PtyHandle
                            // is freed without going through `shutdown`,
                            // which would also cancel us. Defensive.
                            debug!(?tab, "pty processor: control channel closed");
                        }
                    }
                }
                maybe = rx.recv() => {
                    match maybe {
                        Some(bytes) => {
                            // Per-tab raw content capture. Disabled by
                            // default; fast-path no-op when off.
                            crate::content::write(&tab, &bytes);
                            let events = layer.ingest(&bytes);
                            let saw_terminal_bytes = events.iter().any(|e| matches!(
                                e,
                                ProcessingEvent::TerminalBytes(b) if !b.is_empty()
                            ));
                            if dispatch_events(&tab, is_shell, events, &channel, &tts_segments).await.is_err() {
                                break;
                            }
                            if saw_terminal_bytes && !is_shell {
                                let now = tokio::time::Instant::now();
                                if burst_start.is_none() {
                                    burst_start = Some(now);
                                }
                                last_byte_time = Some(now);
                                run_permission_check(
                                    &tab,
                                    &mut detector,
                                    &layer,
                                    &state_signals,
                                    &mut working_active,
                                );
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
                    if dispatch_events(&tab, is_shell, events, &channel, &tts_segments).await.is_err() {
                        break;
                    }
                    if saw_terminal_bytes && !is_shell {
                        let now = tokio::time::Instant::now();
                        if burst_start.is_none() {
                            burst_start = Some(now);
                        }
                        last_byte_time = Some(now);
                        run_permission_check(
                            &tab,
                            &mut detector,
                            &layer,
                            &state_signals,
                            &mut working_active,
                        );
                    }

                    // Avatar activity (Thinking↔Idle). Content-first: the
                    // `claude_working` marker is authoritative while it's on
                    // screen; the byte timers are fallbacks.
                    if !is_shell {
                        if !output_active {
                            // Enter "working" on the marker, or — for a
                            // response that never paints it — a sustained
                            // byte burst.
                            let burst_ready = burst_start
                                .is_some_and(|s| s.elapsed() >= CLAUDE_BURST_MIN);
                            if working_active || burst_ready {
                                output_active = true;
                                let _ = state_signals
                                    .try_send(StateSignal::ClaudeOutputStarted { tab: tab.clone() });
                            }
                        } else {
                            // Leave "working" only once the marker is gone and
                            // the stream has settled — a thinking pause with
                            // the marker still up must NOT release Idle. The
                            // stale guard frees a marker left ghosting in the
                            // grid with no underlying byte activity.
                            let quiet = last_byte_time
                                .is_some_and(|t| t.elapsed() >= CLAUDE_QUIET);
                            let stale = last_byte_time
                                .is_some_and(|t| t.elapsed() >= CLAUDE_WORKING_STALE);
                            if (!working_active && quiet) || stale {
                                if stale && working_active {
                                    // Looked stuck — drop the detector's
                                    // latched Working state so a genuine
                                    // resumption can re-trigger it.
                                    detector.force_clear(PatternKind::Working);
                                    working_active = false;
                                }
                                output_active = false;
                                let _ = state_signals
                                    .try_send(StateSignal::ClaudeOutputStopped { tab: tab.clone() });
                                burst_start = None;
                                last_byte_time = None;
                            }
                        }
                    }
                }
            }
        }
        debug!(?tab, "pty processor task exiting");
    });
}

/// Scan the rendered tail for known prompt patterns and forward edge
/// transitions to the state manager. No-op for tabs configured with empty
/// patterns (Shell tabs).
fn run_permission_check(
    tab: &TabId,
    detector: &mut PermissionDetector,
    layer: &ProcessingLayer,
    state_signals: &mpsc::Sender<StateSignal>,
    working_active: &mut bool,
) {
    let rendered = layer.recent_rendered(PERMISSION_SCAN_TAIL);
    // Opt-in capture for pattern characterization. Enable with
    // RUST_LOG=perm_capture=debug to dump the exact rendered tail the
    // detector matches against; pick distinctive substrings from the dump
    // and add them to <exe-dir>/patterns.json.
    if tracing::enabled!(target: "perm_capture", tracing::Level::DEBUG) {
        let escaped = rendered.replace('\n', "\\n");
        tracing::debug!(target: "perm_capture", ?tab, rendered = %escaped, "perm capture");
    }
    for transition in detector.check(&rendered) {
        match transition {
            PatternTransition::Detected { kind: PatternKind::Permission, pattern_name } => {
                debug!(?tab, pattern = pattern_name, "permission prompt detected");
                let _ = state_signals
                    .try_send(StateSignal::PermissionPromptDetected { tab: tab.clone() });
            }
            PatternTransition::Resolved { kind: PatternKind::Permission, pattern_name } => {
                debug!(?tab, pattern = pattern_name, "permission prompt resolved");
                let _ = state_signals
                    .try_send(StateSignal::PermissionPromptResolved { tab: tab.clone() });
            }
            PatternTransition::Detected { kind: PatternKind::Question, pattern_name } => {
                debug!(?tab, pattern = pattern_name, "question prompt detected");
                let _ = state_signals
                    .try_send(StateSignal::QuestionPromptDetected { tab: tab.clone() });
            }
            PatternTransition::Resolved { kind: PatternKind::Question, pattern_name } => {
                debug!(?tab, pattern = pattern_name, "question prompt resolved");
                let _ = state_signals
                    .try_send(StateSignal::QuestionPromptResolved { tab: tab.clone() });
            }
            // Working transitions don't emit directly — they update the
            // marker level the activity block reads to drive ClaudeOutput
            // Started/Stopped (content-first, byte timers as fallback).
            PatternTransition::Detected { kind: PatternKind::Working, pattern_name } => {
                debug!(?tab, pattern = pattern_name, "claude working detected");
                *working_active = true;
            }
            PatternTransition::Resolved { kind: PatternKind::Working, pattern_name } => {
                debug!(?tab, pattern = pattern_name, "claude working resolved");
                *working_active = false;
            }
        }
    }
}

async fn dispatch_events(
    tab: &TabId,
    is_shell: bool,
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
                if is_shell {
                    // Shell tabs never speak. The vte parser still ran (we
                    // need it for xterm bytes), but any tag content the
                    // scanner picked out is dropped on the floor.
                    debug!(target: "tts_stub", ?tab, text = %text, "shell tab: dropping TTS segment");
                    continue;
                }
                debug!(target: "tts_stub", ?tab, text = %text, "extracted TTS segment");
                let req = TtsRequest::Synthesize { tab: tab.clone(), text, suppressible: true };
                if tts_segments.send(req).await.is_err() {
                    debug!("pty processor: TTS segment channel closed (worker not running)");
                }
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
        use std::time::{Duration, Instant};
        const POLL: Duration = Duration::from_millis(100);
        // Once cancellation is observed (shutdown/restart is killing the child)
        // keep polling only briefly: if the kill works the child reports its
        // status and we emit a clean pty-exit; if it never dies we must NOT
        // park this blocking-pool thread forever — that leaks a thread per
        // restart and eventually exhausts tokio's blocking pool, wedging all
        // spawn_blocking work (PTY writes/resizes). Give up after the grace.
        const KILL_GRACE: Duration = Duration::from_secs(5);
        let mut give_up_at: Option<Instant> = None;

        let exit = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) => {}
                Err(e) => break Err(e),
            }
            if cancel.is_cancelled() {
                let deadline = *give_up_at.get_or_insert_with(|| Instant::now() + KILL_GRACE);
                if Instant::now() >= deadline {
                    warn!(
                        ?tab,
                        "pty child did not exit after kill; abandoning waiter (process may be orphaned)"
                    );
                    debug!(?tab, "pty waiter task exiting (abandoned)");
                    return;
                }
            }
            std::thread::sleep(POLL);
        };
        cancel.cancel();
        let (exit_str, code) = match &exit {
            // `portable_pty::ExitStatus::exit_code()` is u32; the bit pattern
            // round-trips through i32 cleanly for display, and this matches
            // how shells (and the Tauri-side overlay) usually print exit
            // codes.
            Ok(status) => (format!("{:?}", status), Some(status.exit_code() as i32)),
            Err(e) => (format!("error: {}", e), None),
        };
        let _ = state_signals.try_send(StateSignal::SubprocessExited { tab: tab.clone(), code });
        if let Err(e) = app.emit(
            "pty-exit",
            PtyExitPayload {
                tab: tab.clone(),
                exit: exit_str,
            },
        ) {
            warn!(error = %e, "failed to emit pty-exit");
        }
        debug!(?tab, "pty waiter task exiting");
    });
}
